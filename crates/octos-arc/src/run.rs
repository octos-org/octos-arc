//! `octos arc run --spec runner-spec.json --policy arc-policy.toml`: one
//! command for a whole ARC task. The runner spec is the contract between the
//! platform glue (`arc/main.py`) and the kernel; the policy file carries
//! every tunable and points at the prompt templates.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Args;
use eyre::{Result, WrapErr, ensure};
use serde::{Deserialize, Serialize};

use crate::events::Events;
use crate::flow::{Flow, FlowInputs};
use crate::llm::{Completer, DryRunCompleter, LlmClient, ModelRoute, UsageLedger};
use crate::policy::Policy;
use crate::prompts::Prompts;
use crate::tree;

/// Written by the glue; read by the kernel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerSpec {
    #[serde(default = "default_schema")]
    pub schema_version: u32,
    /// Directory holding requirements.yaml (or the file itself).
    pub requirement_path: PathBuf,
    /// The workspace the platform grades (frontend/ + backend/ live here).
    pub output_dir: PathBuf,
    /// The port the grader starts the backend on; never bound during generation.
    pub web_port: u16,
    /// Directory with the official Playwright specs, when found.
    #[serde(default)]
    pub tests_dir: Option<PathBuf>,
    /// The adapter bundle (a `local-grader/` Playwright may live there).
    #[serde(default)]
    pub bundle_dir: Option<PathBuf>,
    /// Evolution: the previous run's requirement table, copied by the glue before
    /// the platform runtime stores the new tree over `.arc/traceability/requirements.json`.
    #[serde(default)]
    pub previous_requirements: Option<PathBuf>,
    pub model: ModelRoute,
}

fn default_schema() -> u32 {
    1
}

impl RunnerSpec {
    pub fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .wrap_err_with(|| format!("reading runner spec {}", path.display()))?;
        let spec: Self = serde_json::from_str(&text)
            .wrap_err_with(|| format!("parsing runner spec {}", path.display()))?;
        ensure!(
            spec.schema_version == 1,
            "unsupported runner spec schema {}",
            spec.schema_version
        );
        ensure!(spec.web_port > 0, "runner spec needs a web_port");
        Ok(spec)
    }
}

#[derive(Debug, Args)]
pub struct RunCommand {
    /// runner-spec.json written by the platform glue.
    #[arg(long, value_name = "FILE")]
    pub spec: PathBuf,
    /// arc-policy.toml; built-in defaults when omitted.
    #[arg(long, value_name = "FILE")]
    pub policy: Option<PathBuf>,
    /// Ordered per-request model rules (also OCTOS_ARC_MODEL_ROUTES).
    #[arg(long, value_name = "JSON")]
    pub model_routes_json: Option<String>,
    /// Walk the whole flow without calling the model (also OCTOS_ARC_DRYRUN=1).
    #[arg(long)]
    pub dry_run: bool,
    /// Override one policy field (KEY=VALUE, e.g. repair.rounds=2); wins over
    /// the environment and the policy file.
    #[arg(long = "set", value_name = "KEY=VALUE")]
    pub overrides: Vec<String>,
}

/// Resolve the policy: file → environment → command line.
pub fn resolve_policy(path: Option<&Path>, overrides: &[String]) -> Result<(Policy, Vec<String>)> {
    let mut policy = Policy::load(path)?;
    let mut applied = policy.apply_env(&|name| std::env::var(name).ok())?;
    for item in overrides {
        let (key, value) = item
            .split_once('=')
            .ok_or_else(|| eyre::eyre!("--set expects KEY=VALUE, got {item:?}"))?;
        policy.set(key.trim(), value)?;
        applied.push(format!("--set {item}"));
    }
    policy.validate()?;
    Ok((policy, applied))
}

/// Prompt directory: `prompts.dir` relative to the policy file.
pub fn prompts_dir(policy_path: Option<&Path>, policy: &Policy) -> Option<PathBuf> {
    let dir = PathBuf::from(&policy.prompts.dir);
    let resolved = if dir.is_absolute() {
        dir
    } else {
        policy_path?.parent()?.join(dir)
    };
    resolved.is_dir().then_some(resolved)
}

pub fn execute_run(command: RunCommand) -> Result<i32> {
    let spec = RunnerSpec::read(&command.spec)?;
    let raw_routes = command
        .model_routes_json
        .clone()
        .unwrap_or_else(|| std::env::var("OCTOS_ARC_MODEL_ROUTES").unwrap_or_default());
    let routes = crate::routing::parse(&raw_routes)?;
    let (mut policy, applied) = resolve_policy(command.policy.as_deref(), &command.overrides)?;
    if command.dry_run {
        policy.debug.dry_run = true;
    }
    let prompts = Prompts::load(prompts_dir(command.policy.as_deref(), &policy).as_deref())?;
    let tree = tree::load(&spec.requirement_path)?;
    std::fs::create_dir_all(&spec.output_dir)?;
    let output_dir = spec.output_dir.canonicalize()?;
    let mut spec = RunnerSpec { output_dir, ..spec };
    let arc_dir = spec.output_dir.join(".arc");
    let mut events = Events::open(&arc_dir)?;
    events.log(format!(
        "[policy] {}{}",
        command
            .policy
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "built-in defaults".into()),
        if applied.is_empty() {
            String::new()
        } else {
            format!("; overrides: {}", applied.join(", "))
        }
    ));
    if !prompts.overrides().is_empty() {
        events.log(format!(
            "[policy] prompt overrides: {}",
            prompts.overrides().len()
        ));
    }
    let ledger = UsageLedger::shared(&arc_dir, &spec.model.model, &spec.model.provider);
    let dry_run = policy.debug.dry_run;
    let relay = if routes.is_empty() || dry_run {
        None
    } else {
        let relay = crate::routing::Relay::start(&spec.model.base_url, routes, &arc_dir)?;
        spec.model.base_url = relay.base_url.clone();
        ledger
            .lock()
            .expect("usage ledger")
            .disable_single_model_pricing();
        events.log("[routing] per-request model selection enabled; actual models recorded in model-routes.jsonl");
        Some(relay)
    };
    let llm: Box<dyn Completer> = if dry_run {
        events.log("[llm] dry run: no model calls");
        Box::new(DryRunCompleter { calls: 0 })
    } else {
        let chat_timeout = Duration::from_secs(policy.budget.node_timeout_seconds.max(60));
        events.log(format!(
            "[llm] {} via {} (key from {})",
            spec.model.model, spec.model.base_url, spec.model.api_key_env
        ));
        Box::new(LlmClient::new(
            &spec.model,
            &policy.reasoning,
            ledger.clone(),
            &arc_dir,
            chat_timeout,
            policy.debug.dump_requests,
        )?)
    };
    let executable = std::env::current_exe()?;
    let inputs = FlowInputs {
        policy,
        prompts,
        tree,
        llm,
        ledger,
        events,
        executable,
        dry_run,
        routing: relay.as_ref().map(|r| r.control.clone()),
    };
    let mut flow = Flow::new(&spec, inputs)?;
    let outcome = flow.run();
    // Keep the historical exit status for application verdicts, but surface
    // provider account rejection as an operational failure to the caller.
    if let Some(reason) = outcome.aborted {
        eprintln!("octos arc run aborted: {reason}");
        if crate::llm::is_permanent_provider_error(&reason) {
            return Ok(1);
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn should_parse_a_runner_spec_and_default_optional_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runner-spec.json");
        std::fs::write(
            &path,
            json!({"requirement_path": "/tmp/task", "output_dir": "/tmp/out", "web_port": 3000,
                "model": {"model": "deepseek-v4-flash", "base_url": "https://api.arc-bench.com/v1"}})
            .to_string(),
        )
        .unwrap();
        let spec = RunnerSpec::read(&path).unwrap();
        assert_eq!(spec.schema_version, 1);
        assert_eq!(spec.model.api_key_env, "OPENAI_API_KEY");
        assert_eq!(spec.model.provider, "openai");
        assert!(spec.tests_dir.is_none());
        assert!(spec.previous_requirements.is_none());
    }

    #[test]
    fn should_read_the_previous_requirement_snapshot_path_when_the_glue_provides_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runner-spec.json");
        std::fs::write(
            &path,
            json!({"requirement_path": "/tmp/task", "output_dir": "/tmp/out", "web_port": 3000,
                "previous_requirements": "/tmp/out/.arc/previous-requirements.json",
                "model": {"model": "deepseek-v4-flash", "base_url": "https://api.arc-bench.com/v1"}})
            .to_string(),
        )
        .unwrap();
        let spec = RunnerSpec::read(&path).unwrap();
        assert_eq!(
            spec.previous_requirements.as_deref(),
            Some(std::path::Path::new(
                "/tmp/out/.arc/previous-requirements.json"
            ))
        );
    }

    #[test]
    fn should_apply_command_line_overrides_last() {
        let (policy, applied) =
            resolve_policy(None, &["repair.rounds=1".into(), "mode.codegen=0".into()]).unwrap();
        assert_eq!(policy.repair.rounds, 1);
        assert!(!policy.mode.codegen);
        assert!(applied.iter().any(|a| a == "--set repair.rounds=1"));
        assert!(resolve_policy(None, &["nonsense".into()]).is_err());
    }

    #[test]
    fn should_resolve_prompt_dir_relative_to_the_policy_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("prompts")).unwrap();
        let policy_path = dir.path().join("arc-policy.toml");
        std::fs::write(&policy_path, "").unwrap();
        let policy = Policy::default();
        assert_eq!(
            prompts_dir(Some(&policy_path), &policy).unwrap(),
            dir.path().join("prompts")
        );
        assert!(prompts_dir(None, &policy).is_none());
    }
}
