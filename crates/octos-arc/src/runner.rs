use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::{Args, ValueEnum};
use eyre::{Result, WrapErr, ensure, eyre};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::pin::{self, BinaryIdentity};
use crate::process::{self, ManagedChild, Output};
use crate::spec::Specification;
use crate::workspace::{directory, source_files, write_json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Create,
    Evolve,
}

#[derive(Debug, Args)]
pub struct ArcCommand {
    #[arg(
        value_name = "REQUIREMENT_PATH",
        help = "Official requirements file or directory"
    )]
    pub requirement_path: PathBuf,
    #[arg(
        long,
        help = "Existing generated project for evolve; empty directory for create"
    )]
    pub output_dir: PathBuf,
    #[arg(
        long,
        value_enum,
        help = "Explicitly choose create or evolve; never auto-rebuild a project"
    )]
    pub mode: Mode,
    #[arg(
        long,
        help = "Prior official requirements when importing a project without a saved Octos snapshot"
    )]
    pub previous_requirements: Option<PathBuf>,
    #[arg(long, default_value = "arc-runtime-lock.json")]
    pub runtime_lock: PathBuf,
    #[arg(
        long,
        help = "Validate inputs and write the delta without calling a model or changing source"
    )]
    pub prepare_only: bool,
    #[arg(
        long,
        help = "Exact model identifier; otherwise MODEL from the platform"
    )]
    pub model: Option<String>,
    #[arg(
        long,
        help = "OpenAI-compatible route; otherwise OPENAI_BASE_URL from the platform"
    )]
    pub base_url: Option<String>,
    #[arg(long, default_value_t = 1800, value_parser = clap::value_parser!(u64).range(1..=7200))]
    pub budget_seconds: u64,
    #[arg(long, default_value_t = 80, value_parser = clap::value_parser!(u32).range(1..=400))]
    pub max_iterations: u32,
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(0..=2))]
    pub repair_attempts: u8,
    #[arg(long, default_value_t = 3000, value_parser = clap::value_parser!(u16).range(1..))]
    pub web_port: u16,
    #[arg(
        long,
        help = "Optional explicit sampling temperature; otherwise the model default"
    )]
    pub temperature: Option<f32>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Snapshot {
    run_id: String,
    specification: Specification,
    source_files: BTreeMap<String, String>,
    validation: String,
}

fn regular_file(path: &Path) -> Result<()> {
    ensure!(
        !path.is_symlink() && path.is_file(),
        "Missing regular file: {}",
        path.display()
    );
    Ok(())
}

fn package(project: &Path, part: &str, script: &str) -> Result<Value> {
    let dir = project.join(part);
    ensure!(
        !dir.is_symlink() && dir.is_dir(),
        "Missing project directory: {part}"
    );
    let path = dir.join("package.json");
    regular_file(&path)?;
    let value: Value = serde_json::from_slice(&fs::read(path)?)?;
    ensure!(
        value
            .get("scripts")
            .and_then(|scripts| scripts.get(script))
            .and_then(Value::as_str)
            .is_some_and(|command| !command.trim().is_empty()),
        "{part}/package.json needs scripts.{script}"
    );
    Ok(value)
}

fn previous_specification(
    options: &ArcCommand,
    project: &Path,
    files: &BTreeMap<String, String>,
) -> Result<Option<Specification>> {
    match options.mode {
        Mode::Create => {
            ensure!(
                files.is_empty(),
                "Create refuses a nonempty project; use evolve to preserve existing source"
            );
            ensure!(
                options.previous_requirements.is_none(),
                "Create cannot receive previous requirements"
            );
            Ok(None)
        }
        Mode::Evolve => {
            package(project, "frontend", "build")?;
            package(project, "backend", "start")?;
            if let Some(path) = &options.previous_requirements {
                return Ok(Some(Specification::read(path)?));
            }
            let path = project.join(".arc/octos/spec.json");
            regular_file(&path).wrap_err("Evolution needs the previous generated project's snapshot or --previous-requirements")?;
            let previous: Snapshot = serde_json::from_slice(&fs::read(path)?)?;
            let rebuilt = Specification::from_tree(previous.specification.tree)?;
            ensure!(
                rebuilt.sha256 == previous.specification.sha256
                    && rebuilt.nodes == previous.specification.nodes,
                "Previous specification integrity mismatch"
            );
            Ok(Some(rebuilt))
        }
    }
}

fn event(project: &Path, state: &str, message: &str) -> Result<()> {
    directory(&project.join(".arc"))?;
    let path = project.join(".arc/runner-events.jsonl");
    ensure!(!path.is_symlink(), "Events file cannot be a symlink");
    let mut output = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(
        output,
        "{}",
        json!({"type":"runner_state","state":state,"timestamp":chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string(),"message":message})
    )?;
    Ok(())
}

fn clean_env(home: &Path) -> Vec<(OsString, OsString)> {
    vec![
        (
            "PATH".into(),
            std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into()),
        ),
        ("HOME".into(), home.as_os_str().to_owned()),
        ("TMPDIR".into(), home.as_os_str().to_owned()),
        ("LANG".into(), "en_US.UTF-8".into()),
        ("CI".into(), "true".into()),
    ]
}

fn configured_model(options: &ArcCommand) -> Result<(String, String, String)> {
    let model = options
        .model
        .clone()
        .or_else(|| std::env::var("MODEL").ok())
        .ok_or_else(|| eyre!("Set --model or MODEL"))?;
    ensure!(
        !model.trim().is_empty() && !model.to_ascii_lowercase().contains("latest"),
        "Model must be an explicit non-latest identifier"
    );
    let endpoint = options
        .base_url
        .clone()
        .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
        .ok_or_else(|| eyre!("Set --base-url or OPENAI_BASE_URL"))?;
    let url = reqwest::Url::parse(&endpoint)?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "Model route must be an HTTP URL without embedded credentials or query parameters"
    );
    let key = std::env::var("OPENAI_API_KEY").wrap_err("OPENAI_API_KEY is required")?;
    ensure!(!key.trim().is_empty(), "OPENAI_API_KEY is empty");
    if let Some(temperature) = options.temperature {
        ensure!(
            temperature.is_finite() && (0.0..=2.0).contains(&temperature),
            "Temperature must be between 0 and 2"
        );
    }
    Ok((model, endpoint, key))
}

fn checked_command(
    project: &Path,
    part: &str,
    args: &[&str],
    env: &[(OsString, OsString)],
    deadline: Instant,
    evidence: &mut Vec<Value>,
) -> Result<()> {
    let args: Vec<OsString> = args.iter().map(OsString::from).collect();
    let output = process::run(
        Path::new("npm"),
        &args,
        &project.join(part),
        env,
        deadline.min(Instant::now() + Duration::from_secs(180)),
    )?;
    let success = output.succeeded();
    evidence.push(json!({"kind":"local_command","directory":part,"command":args,"result":output}));
    ensure!(success, "Local command failed in {part}: npm {args:?}");
    Ok(())
}

fn validate(
    project: &Path,
    web_port: u16,
    env: &[(OsString, OsString)],
    deadline: Instant,
    evidence: &mut Vec<Value>,
) -> Result<()> {
    let frontend = package(project, "frontend", "build")?;
    let backend = package(project, "backend", "start")?;
    for part in ["frontend", "backend"] {
        let install = if project.join(part).join("package-lock.json").is_file() {
            "ci"
        } else {
            "install"
        };
        checked_command(
            project,
            part,
            &[install, "--no-audit", "--no-fund"],
            env,
            deadline,
            evidence,
        )?;
    }
    checked_command(
        project,
        "frontend",
        &["run", "build"],
        env,
        deadline,
        evidence,
    )?;
    for (part, manifest) in [("frontend", frontend), ("backend", backend)] {
        if manifest
            .get("scripts")
            .and_then(|scripts| scripts.get("test"))
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
        {
            checked_command(project, part, &["test"], env, deadline, evidence)?;
        } else {
            evidence.push(json!({"kind":"local_tests","directory":part,"status":"not_configured","official":false}));
        }
    }
    ensure!(
        Instant::now() < deadline,
        "Budget exhausted before HTTP startup check"
    );
    let port = loop {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        if port != web_port {
            break port;
        }
    };
    let mut server_env = env.to_vec();
    server_env.push(("PORT".into(), port.to_string().into()));
    let mut server = ManagedChild::spawn(
        Path::new("npm"),
        &["run".into(), "start".into()],
        &project.join("backend"),
        &server_env,
    )?;
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_millis(500))
        .build()?;
    let ready_deadline = deadline.min(Instant::now() + Duration::from_secs(30));
    let address = format!("http://127.0.0.1:{port}/");
    let mut status = None;
    while Instant::now() < ready_deadline && server.running()? {
        if let Ok(response) = client.get(&address).send() {
            if response.status().is_success() && server.running()? {
                status = Some(response.status().as_u16());
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    server.stop();
    let output = server.finish(Instant::now())?;
    let address_in_use =
        output.stderr.contains("EADDRINUSE") || output.stdout.contains("EADDRINUSE");
    evidence.push(json!({"kind":"local_http_startup","url":address,"http_status":status,"result":output,"official":false}));
    ensure!(
        status.is_some() && !address_in_use,
        "Backend did not pass the local HTTP startup check"
    );
    Ok(())
}

fn chat_args(
    options: &ArcCommand,
    project: &Path,
    data: &Path,
    config: &Path,
    prompt: String,
) -> Vec<OsString> {
    vec![
        "chat".into(),
        "--cwd".into(),
        project.into(),
        "--data-dir".into(),
        data.into(),
        "--config".into(),
        config.into(),
        "--profile".into(),
        "coding".into(),
        "--json".into(),
        "--no-retry".into(),
        "--sandbox".into(),
        "workspace-write".into(),
        "--ask-for-approval".into(),
        "never".into(),
        "--max-iterations".into(),
        options.max_iterations.to_string().into(),
        "--message".into(),
        prompt.into(),
    ]
}

fn redact(output: &mut Output, key: &str) {
    output.stdout = output.stdout.replace(key, "[REDACTED]");
    output.stderr = output.stderr.replace(key, "[REDACTED]");
}

pub fn execute(options: ArcCommand, identity: BinaryIdentity<'_>) -> Result<Value> {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(options.budget_seconds);
    let specification = Specification::read(&options.requirement_path)?;
    fs::create_dir_all(&options.output_dir)?;
    ensure!(
        !options.output_dir.is_symlink(),
        "Project root cannot be a symlink"
    );
    let project = options.output_dir.canonicalize()?;
    for path in [
        project.join(".arc"),
        project.join(".arc/octos"),
        project.join(".arc/octos/runs"),
    ] {
        directory(&path)?;
    }
    let lock_path = project.join(".arc/octos/run.lock");
    ensure!(!lock_path.is_symlink(), "Run lock cannot be a symlink");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    lock.try_lock_exclusive()
        .wrap_err("Another Octos ARC run owns this workspace")?;
    let before = source_files(&project)?;
    let previous = previous_specification(&options, &project, &before)?;
    let delta = specification.delta(previous.as_ref());
    let run_id = uuid::Uuid::new_v4().to_string();
    let run_dir = project.join(".arc/octos/runs").join(&run_id);
    directory(&run_dir)?;
    write_json(&run_dir.join("requirements.json"), &specification.tree)?;
    write_json(
        &run_dir.join("previous-requirements.json"),
        &previous.as_ref().map(|value| &value.tree),
    )?;
    write_json(&run_dir.join("delta.json"), &delta)?;
    let mut report = json!({"schema_version":1,"run_id":run_id,"mode":options.mode,"status":"prepared","requirements_sha256":specification.sha256,"previous_requirements_sha256":previous.as_ref().map(|value| &value.sha256),"delta":delta,"source_before":before,"official_evaluation":"not_run","score":null,"evidence":[],"started_at":chrono::Utc::now().to_rfc3339()});
    write_json(&run_dir.join("report.json"), &report)?;
    if options.prepare_only {
        return Ok(report);
    }
    event(
        &project,
        "running",
        "Octos ARC generation started; official evaluation has not run",
    )?;
    let mut evidence = Vec::new();
    let outcome = (|| -> Result<()> {
        let release = pin::verify(&options.runtime_lock, &identity)?;
        report["runtime"] = serde_json::to_value(release)?;
        let (model, endpoint, key) = configured_model(&options)?;
        report["model_configuration"] = json!({"requested_model":model,"base_url":endpoint,"api_type":"openai","temperature":options.temperature,"max_iterations":options.max_iterations,"budget_seconds":options.budget_seconds,"repair_attempts":options.repair_attempts,"profile":"coding","sandbox":"workspace-write","approval":"never"});
        report["status"] = json!("running");
        write_json(&run_dir.join("report.json"), &report)?;
        let temporary = tempfile::Builder::new().prefix("octos-arc-").tempdir()?;
        let home = temporary.path().join("home");
        directory(&home)?;
        let data = temporary.path().join("runtime");
        directory(&data)?;
        let config = temporary.path().join("config.json");
        write_json(
            &config,
            &json!({"provider":"openai","model":model,"base_url":endpoint,"api_type":"openai","api_key_env":"OPENAI_API_KEY","model_temperature":options.temperature}),
        )?;
        let base_env = clean_env(&home);
        let mut agent_env = base_env.clone();
        agent_env.push(("OPENAI_API_KEY".into(), key.clone().into()));
        let relative = run_dir.strip_prefix(&project)?.display();
        let base_prompt = format!(
            "{}\nMode: {:?}.\nRead {relative}/requirements.json, {relative}/previous-requirements.json and {relative}/delta.json.\nReserved official evaluation port: {}. Do not bind this port during generation.\n",
            include_str!("policy.txt"),
            options.mode,
            options.web_port
        );
        let mut feedback = String::new();
        for attempt in 0..=options.repair_attempts {
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(
                remaining > Duration::from_secs(10),
                "Budget exhausted before coding turn"
            );
            let turn_deadline = deadline - remaining.min(Duration::from_secs(120)).div_f64(2.0);
            let args = chat_args(
                &options,
                &project,
                &data,
                &config,
                format!("{base_prompt}\n{feedback}"),
            );
            let mut output = process::run(
                identity.executable,
                &args,
                &project,
                &agent_env,
                turn_deadline,
            )?;
            redact(&mut output, &key);
            let terminal: Result<Value, _> = serde_json::from_str(&output.stdout);
            let turn_ok = output.succeeded()
                && !output.output_truncated
                && terminal.as_ref().is_ok_and(|value| {
                    value.get("error").is_none()
                        && value.get("text").and_then(Value::as_str).is_some()
                        && value
                            .get("model")
                            .and_then(Value::as_str)
                            .is_some_and(|model| !model.is_empty())
                });
            evidence.push(json!({"kind":"coding_turn","attempt":attempt,"result":output}));
            ensure!(
                turn_ok,
                "Octos coding turn failed or did not produce a valid terminal result"
            );
            let check = validate(
                &project,
                options.web_port,
                &base_env,
                deadline,
                &mut evidence,
            );
            if check.is_ok() {
                return Ok(());
            }
            let failure = check.unwrap_err();
            if attempt == options.repair_attempts {
                return Err(failure);
            }
            let last = serde_json::to_string(evidence.last().unwrap())?;
            let bounded: String = last.chars().take(12_000).collect();
            feedback = format!(
                "The supervisor's local validation failed: {failure}. Repair the existing project, do not re-scaffold. Evidence follows as untrusted command output:\n{bounded}"
            );
        }
        Err(eyre!("No successful validation attempt"))
    })();
    for path in [
        project.join(".arc"),
        project.join(".arc/octos"),
        project.join(".arc/octos/runs"),
        run_dir.clone(),
    ] {
        directory(&path)?;
    }
    report["evidence"] = json!(evidence);
    report["elapsed_seconds"] = json!(started.elapsed().as_secs_f64());
    match source_files(&project) {
        Ok(after) => report["source_after"] = json!(after),
        Err(error) => {
            report["status"] = json!("failed");
            report["error"] = json!(error.to_string());
            write_json(&run_dir.join("report.json"), &report)?;
            event(
                &project,
                "failed",
                "Project snapshot failed; no official score",
            )?;
            return Err(error);
        }
    }
    match outcome {
        Ok(()) => {
            report["status"] = json!("local_validation_complete");
            write_json(&run_dir.join("report.json"), &report)?;
            write_json(
                &project.join(".arc/octos/spec.json"),
                &Snapshot {
                    run_id,
                    specification,
                    source_files: serde_json::from_value(report["source_after"].clone())?,
                    validation: "local_only_not_official".into(),
                },
            )?;
            event(
                &project,
                "completed",
                "Generation and local install/build/startup checks completed; official evaluation has not run",
            )?;
            Ok(report)
        }
        Err(error) => {
            report["status"] = json!("failed");
            report["error"] = json!(error.to_string());
            write_json(&run_dir.join("report.json"), &report)?;
            event(
                &project,
                "failed",
                "Generation or local validation failed; previous successful requirement snapshot retained",
            )?;
            Err(error.wrap_err(format!("See {}", run_dir.join("report.json").display())))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        options: ArcCommand,
    }

    fn identity() -> BinaryIdentity<'static> {
        BinaryIdentity {
            executable: Path::new("/nonexistent-test-runtime"),
            source_commit: "",
            target: "test",
            dirty: true,
        }
    }

    fn options(root: &Path, mode: &str) -> ArcCommand {
        TestCli::parse_from([
            OsString::from("test"),
            root.join("requirements.json").into_os_string(),
            "--output-dir".into(),
            root.join("project").into_os_string(),
            "--mode".into(),
            mode.into(),
            "--prepare-only".into(),
        ])
        .options
    }

    fn requirements(root: &Path, extra: bool) {
        let mut children = vec![
            json!({"id":"REQ-1","type":"ATOMIC","description":"Original behavior","dependencies":[]}),
        ];
        if extra {
            children.push(json!({"id":"REQ-2","type":"ATOMIC","description":"Additional behavior","dependencies":["REQ-1"]}));
        }
        write_json(
            &root.join("requirements.json"),
            &json!({"id":"ROOT","type":"FOLDER","children":children}),
        )
        .unwrap();
    }

    fn project(root: &Path) -> PathBuf {
        let project = root.join("project");
        fs::create_dir_all(project.join("frontend")).unwrap();
        fs::create_dir_all(project.join("backend")).unwrap();
        write_json(
            &project.join("frontend/package.json"),
            &json!({"scripts":{"build":"fixture-build"}}),
        )
        .unwrap();
        write_json(
            &project.join("backend/package.json"),
            &json!({"scripts":{"start":"fixture-start"}}),
        )
        .unwrap();
        fs::write(
            project.join("frontend/existing.txt"),
            "preserve this source",
        )
        .unwrap();
        project
    }

    #[test]
    fn preparation_neither_calls_runtime_nor_claims_completion() {
        let temp = tempfile::tempdir().unwrap();
        requirements(temp.path(), false);
        let report = execute(options(temp.path(), "create"), identity()).unwrap();
        assert_eq!(report["status"], "prepared");
        assert_eq!(report["official_evaluation"], "not_run");
        assert!(report["score"].is_null());
        assert!(
            source_files(&temp.path().join("project"))
                .unwrap()
                .is_empty()
        );
        assert!(!temp.path().join("project/.arc/octos/spec.json").exists());
        assert!(
            !temp
                .path()
                .join("project/.arc/runner-events.jsonl")
                .exists()
        );
    }

    #[test]
    fn create_refuses_existing_source() {
        let temp = tempfile::tempdir().unwrap();
        requirements(temp.path(), false);
        let project = project(temp.path());
        let before = source_files(&project).unwrap();
        assert!(
            execute(options(temp.path(), "create"), identity())
                .unwrap_err()
                .to_string()
                .contains("nonempty")
        );
        assert_eq!(before, source_files(&project).unwrap());
    }

    #[test]
    fn evolve_requires_prior_requirements() {
        let temp = tempfile::tempdir().unwrap();
        requirements(temp.path(), true);
        project(temp.path());
        assert!(execute(options(temp.path(), "evolve"), identity()).is_err());
    }

    #[test]
    fn evolve_preparation_preserves_project_and_computes_only_addition() {
        let temp = tempfile::tempdir().unwrap();
        requirements(temp.path(), false);
        fs::copy(
            temp.path().join("requirements.json"),
            temp.path().join("previous.json"),
        )
        .unwrap();
        requirements(temp.path(), true);
        let project = project(temp.path());
        let before = source_files(&project).unwrap();
        let mut args = options(temp.path(), "evolve");
        args.previous_requirements = Some(temp.path().join("previous.json"));
        let report = execute(args, identity()).unwrap();
        assert_eq!(report["delta"]["added"], json!(["REQ-2"]));
        assert_eq!(report["delta"]["unchanged"], json!(["REQ-1"]));
        assert_eq!(before, source_files(&project).unwrap());
    }

    #[test]
    fn missing_release_fails_without_replacing_previous_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        requirements(temp.path(), false);
        let prior = Specification::read(&temp.path().join("requirements.json")).unwrap();
        let project = project(temp.path());
        fs::create_dir_all(project.join(".arc/octos")).unwrap();
        let snapshot = project.join(".arc/octos/spec.json");
        write_json(
            &snapshot,
            &Snapshot {
                run_id: "prior".into(),
                specification: prior,
                source_files: source_files(&project).unwrap(),
                validation: "local_only_not_official".into(),
            },
        )
        .unwrap();
        let previous_bytes = fs::read(&snapshot).unwrap();
        requirements(temp.path(), true);
        let mut args = options(temp.path(), "evolve");
        args.prepare_only = false;
        args.runtime_lock = temp.path().join("lock.json");
        write_json(
            &args.runtime_lock,
            &json!({"schema_version":1,"runtime_release":null}),
        )
        .unwrap();
        assert!(execute(args, identity()).is_err());
        assert_eq!(fs::read(snapshot).unwrap(), previous_bytes);
        let events = fs::read_to_string(project.join(".arc/runner-events.jsonl")).unwrap();
        let events: Vec<Value> = events
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            events
                .iter()
                .map(|event| event["state"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["running", "failed"]
        );
        let run = fs::read_dir(project.join(".arc/octos/runs"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let report: Value =
            serde_json::from_slice(&fs::read(run.join("report.json")).unwrap()).unwrap();
        assert_eq!(report["status"], "failed");
        assert!(report["score"].is_null());
    }

    #[test]
    fn coding_uses_real_chat_entry_with_bounded_iterations() {
        let temp = tempfile::tempdir().unwrap();
        let options = options(temp.path(), "create");
        let args = chat_args(
            &options,
            Path::new("project"),
            Path::new("data"),
            Path::new("config"),
            "implement".into(),
        );
        assert_eq!(args[0], "chat");
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--max-iterations", "80"])
        );
        assert!(args.windows(2).any(|pair| pair == ["--profile", "coding"]));
        assert!(
            !args
                .iter()
                .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox")
        );
    }

    #[test]
    fn model_key_is_redacted_from_coding_evidence() {
        let mut output = Output {
            exit_code: Some(0),
            timed_out: false,
            stdout: "secret-value".into(),
            stderr: "error secret-value".into(),
            output_truncated: false,
        };
        redact(&mut output, "secret-value");
        assert_eq!(output.stdout, "[REDACTED]");
        assert_eq!(output.stderr, "error [REDACTED]");
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "HTTP server fixture launched only by the supervisor test"]
    fn http_server_fixture() {
        if std::env::var("OCTOS_ARC_TEST_SERVER").as_deref() != Ok("1") {
            return;
        }
        let port: u16 = std::env::var("PORT").unwrap().parse().unwrap();
        let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        }
    }

    #[cfg(unix)]
    #[test]
    fn http_supervision_checks_response_and_releases_owned_port() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let project = project(temp.path());
        let binary = temp.path().join("npm");
        fs::write(&binary, "#!/bin/sh\nif [ \"$2\" = start ]; then exec \"$OCTOS_ARC_TEST_EXE\" --exact runner::tests::http_server_fixture --ignored --nocapture; fi\nexit 0\n").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        let env = vec![
            (OsString::from("PATH"), temp.path().as_os_str().to_owned()),
            (
                "OCTOS_ARC_TEST_EXE".into(),
                std::env::current_exe().unwrap().into_os_string(),
            ),
            ("OCTOS_ARC_TEST_SERVER".into(), "1".into()),
        ];
        let mut evidence = Vec::new();
        validate(
            &project,
            3000,
            &env,
            Instant::now() + Duration::from_secs(5),
            &mut evidence,
        )
        .unwrap();
        let startup = evidence.last().unwrap();
        assert_eq!(startup["kind"], "local_http_startup");
        assert_eq!(startup["http_status"], 200);
        assert_eq!(startup["official"], false);
        let url = reqwest::Url::parse(startup["url"].as_str().unwrap()).unwrap();
        let port = url.port().unwrap();
        assert_ne!(port, 3000);
        assert!(TcpListener::bind(("127.0.0.1", port)).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn failed_build_is_recorded_and_never_reaches_startup() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let project = project(temp.path());
        let binary = temp.path().join("npm");
        fs::write(&binary, "#!/bin/sh\nif [ \"$1\" = install ]; then exit 0; fi\nprintf 'fixture build failure' >&2\nexit 9\n").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        let env = vec![(OsString::from("PATH"), temp.path().as_os_str().to_owned())];
        let mut evidence = Vec::new();
        assert!(
            validate(
                &project,
                3000,
                &env,
                Instant::now() + Duration::from_secs(5),
                &mut evidence
            )
            .is_err()
        );
        assert_eq!(evidence.len(), 3);
        assert_eq!(evidence[2]["result"]["exit_code"], 9);
        assert!(
            !evidence
                .iter()
                .any(|item| item["kind"] == "local_http_startup")
        );
    }
}
