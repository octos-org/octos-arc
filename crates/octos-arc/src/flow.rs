//! The harness main loop (`Flow` in `arc/main.py`): skeleton turn for large
//! trees, then per node implement → acceptance → repair ≤ K (commit on
//! improvement, roll back on regression), the full parallel suite with its
//! repair rounds, the final check for nodes without a local verdict, and the
//! startup rehearsal.
//!
//! Two turn shapes: single-request codegen (in-process model call, files
//! parsed from the reply) and tool mode (a kernel session spawned from this
//! executable, driven over stdio, watched by the guard).

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use eyre::{Result, bail};
use regex::Regex;
use serde_json::{Value, json};

use crate::acceptance::{self, AcceptanceRunner, AppServer, RunSummary, SpecMap};
use crate::budget::{self, Global};
use crate::codegen::{self, CodegenInputs};
use crate::driver::{self, Driver, KernelConfig, TurnSettings};
use crate::events::Events;
use crate::git::Git;
use crate::guard::{ProtectedTrees, TurnMonitor};
use crate::llm::{Completer, CompletionRequest, ModelRoute, ReasoningMode, SharedLedger};
use crate::plan::RunPlan;
use crate::policy::Policy;
use crate::prompts::Prompts;

fn regression_checkpoint_due(index: usize, total: usize, start: usize) -> bool {
    start > 0
        && index >= start
        && index < total
        && index % start == 0
        && (index / start == 1 || (index / start) % 2 == 0)
}

/// How a codegen turn is shaped (`main.codegen_turn` keyword arguments).
struct CodegenOptions<'a> {
    /// Prompt name of the system message; None = `codegen-system`.
    system: Option<&'a str>,
    /// Append the file-block format instructions.
    format: bool,
    /// Where a bare HTML reply is written (tiny tier).
    raw_target: Option<&'a str>,
}

impl Default for CodegenOptions<'_> {
    fn default() -> Self {
        Self {
            system: None,
            format: true,
            raw_target: None,
        }
    }
}
use crate::run::RunnerSpec;
use crate::tree;

static TEST_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^A-Za-z0-9._-]+").unwrap());
static JSON_FENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)```json\s*(\{.*?\})\s*```").unwrap());
static JSON_ANY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)(\{.*\})").unwrap());

/// The tools of the stdio/solo coding transport (the shell is registered as
/// `bash` in coding profiles, `shell` elsewhere).
const STDIO_TOOLS: &[&str] = &[
    "diff_edit",
    "edit_file",
    "glob",
    "grep",
    "list_dir",
    "read_file",
    "shell",
    "bash",
    "write_file",
    "ask_user_question",
    "check",
    "tool_search",
    "update_plan",
];
/// Tools the coding turns never need (`llm_proxy.DROP_TOOLS`).
const DROP_TOOLS: &[&str] = &[
    "spawn",
    "ask_user_question",
    "check",
    "tool_search",
    "update_plan",
    "exec_command",
];
const SHELL_TOOLS: &[&str] = &["bash", "shell", "exec_command"];
const WRITE_TOOL_FILTER: &[&str] = &[
    "write_file",
    "edit_file",
    "diff_edit",
    "apply_patch",
    "create_file",
    "append_file",
];

fn tail(text: &str, n: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(n)).collect()
}

fn head(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}

pub struct Flow {
    policy: Policy,
    prompts: Prompts,
    output_dir: PathBuf,
    req_dir: PathBuf,
    tests_dir: Option<PathBuf>,
    bundle_dir: Option<PathBuf>,
    web_port: u16,
    smoke_port: u16,
    executable: PathBuf,
    route: ModelRoute,
    events: Events,
    git: Git,
    llm: Box<dyn Completer>,
    ledger: SharedLedger,
    dry_run: bool,
    tree: Value,
    ordered: Vec<Value>,
    node_ids: Vec<String>,
    folder_children: BTreeMap<String, Vec<String>>,
    plan: RunPlan,
    budget: Global,
    spec_map: SpecMap,
    all_specs: Vec<String>,
    extra_ports: Vec<u16>,
    runner: Option<AcceptanceRunner>,
    mem_limit: Option<u64>,
    private_playwright: Option<PathBuf>,
    driver: Option<Driver>,
    kernel_dirs: Vec<tempfile::TempDir>,
    kernel_events: Option<File>,
    protected: Option<ProtectedTrees>,
    designs: BTreeMap<String, Value>,
    test_verdict: BTreeMap<String, Option<bool>>,
    checkpoint_regressions: BTreeSet<String>,
    impl_failed: Vec<String>,
    pending_corrections: Vec<String>,
    codegen_blocked: bool,
    unchanged: BTreeSet<String>,
    probe_summaries: BTreeMap<String, RunSummary>,
    /// Nodes actually probed against an existing app (Evolution).
    probe_count: usize,
    /// Model turns so far (tool and codegen), for the global guardrail.
    turns: u32,
    /// Global guardrail tripped: one implement per node, one full suite, no repairs.
    degraded: bool,
    /// Effective cost-guard limits (tokens, turns, absolute tokens); 0 = off.
    guard_tokens: u64,
    guard_turns: u32,
    guard_abs: u64,
    /// Kills our own processes that bind the grading port while we generate.
    watchdog: Option<crate::reap::PortWatchdog>,
    /// Size of the spec text the current node must satisfy (codegen effort by spec size).
    current_spec_chars: usize,
    permanent_provider_error: Option<String>,
    routing: Option<crate::routing::Control>,
}

/// How a node is rebuilt when round 0 passes nothing.
struct Rebuild {
    /// The compact codegen prompt (codegen mode).
    codegen_prompt: Option<String>,
    /// The tool-mode implement prompt.
    tool_prompt: String,
}

pub struct RunOutcome {
    pub failed_nodes: Vec<String>,
    pub aborted: Option<String>,
}

pub struct FlowInputs {
    pub policy: Policy,
    pub prompts: Prompts,
    pub tree: Value,
    pub llm: Box<dyn Completer>,
    pub ledger: SharedLedger,
    pub events: Events,
    pub executable: PathBuf,
    pub dry_run: bool,
    pub routing: Option<crate::routing::Control>,
}

impl Flow {
    pub fn new(spec: &RunnerSpec, inputs: FlowInputs) -> Result<Self> {
        let FlowInputs {
            policy,
            prompts,
            tree,
            llm,
            ledger,
            events,
            executable,
            dry_run,
            routing,
        } = inputs;
        let ordered = tree::topo_order(&tree);
        if ordered.is_empty() {
            bail!("no ATOMIC requirement nodes found");
        }
        let node_ids: Vec<String> = ordered.iter().map(tree::node_id).collect();
        let output_dir = spec.output_dir.clone();
        let evolution = has_app(&output_dir);
        let unchanged = if evolution {
            tree::unchanged_node_ids(
                &ordered,
                &previous_requirement_records(&output_dir, spec.previous_requirements.as_deref()),
            )
        } else {
            BTreeSet::new()
        };
        let nodes_to_implement = node_ids
            .iter()
            .filter(|id| !unchanged.contains(*id))
            .count();
        let plan = RunPlan::new(&policy, &tree, ordered.len(), nodes_to_implement, evolution)?;
        // Cost guard defaults scale with the tree and sit ≈3× above a healthy run (calibration:
        // cloud keep 2224a9013528, 32 nodes, 26M platform tokens, ~1.1 turns per node).
        let n_nodes = ordered.len() as u64;
        let guard_tokens: u64 = match policy.budget.max_total_tokens {
            v if v < 0 => 6_000_000u64.max(2_500_000 * n_nodes),
            v => v as u64,
        };
        let guard_turns: u32 = match policy.budget.max_turns {
            v if v < 0 => 24u32.max(4 * ordered.len() as u32),
            v => v as u32,
        };
        let guard_abs = policy.budget.max_total_tokens_abs;
        let budget = Global::new(plan.time_budget_seconds);
        let mut smoke_port = policy.ports.smoke_port;
        if smoke_port == spec.web_port {
            smoke_port += 1;
        }
        let tests_dir = spec.tests_dir.clone().filter(|d| d.is_dir());
        let (all_specs, spec_map, extra_ports) = match &tests_dir {
            Some(dir) => {
                let specs = acceptance::list_specs(dir);
                let map = acceptance::map_specs_to_nodes(&specs, &node_ids);
                let extra: Vec<u16> = if policy.ports.extra_port_contract {
                    acceptance::spec_base_ports(dir)
                        .into_iter()
                        .filter(|p| *p != spec.web_port)
                        .collect()
                } else {
                    Vec::new()
                };
                (specs, map, extra)
            }
            None => (
                Vec::new(),
                acceptance::map_specs_to_nodes(&[], &node_ids),
                Vec::new(),
            ),
        };
        let folder_children = tree::folder_descendants(&tree);
        Ok(Self {
            git: Git::new(&output_dir),
            policy,
            prompts,
            output_dir,
            req_dir: spec.requirement_path.clone(),
            tests_dir,
            bundle_dir: spec.bundle_dir.clone(),
            web_port: spec.web_port,
            smoke_port,
            executable,
            route: spec.model.clone(),
            events,
            llm,
            ledger,
            dry_run,
            tree,
            ordered,
            node_ids,
            folder_children,
            plan,
            budget,
            spec_map,
            all_specs,
            extra_ports,
            runner: None,
            mem_limit: None,
            private_playwright: None,
            driver: None,
            kernel_dirs: Vec::new(),
            kernel_events: None,
            protected: None,
            designs: BTreeMap::new(),
            test_verdict: BTreeMap::new(),
            checkpoint_regressions: BTreeSet::new(),
            impl_failed: Vec::new(),
            pending_corrections: Vec::new(),
            codegen_blocked: false,
            unchanged,
            probe_summaries: BTreeMap::new(),
            probe_count: 0,
            turns: 0,
            degraded: false,
            guard_tokens,
            guard_turns,
            guard_abs,
            watchdog: None,
            current_spec_chars: 0,
            permanent_provider_error: None,
            routing,
        })
    }

    // -- helpers ----------------------------------------------------------

    fn log(&mut self, line: impl AsRef<str>) {
        self.events.log(line);
    }

    fn mark(&mut self, kind: &str, node_id: &str, message: Option<&str>) {
        let (phase, status) = match kind {
            "design_started" => ("design", "running"),
            "design_done" => ("design", "completed"),
            "design_failed" => ("design", "failed"),
            "implementation_started" => ("implement", "running"),
            "implementation_done" => ("implement", "completed"),
            "implementation_failed" => ("implement", "failed"),
            "test_passed" => ("test", "passed"),
            "test_failed" => ("test", "failed"),
            other => panic!("unknown mark {other}"),
        };
        let aliases = if self.policy.acceptance.alias_spec_ids {
            self.spec_map.aliases_for(node_id)
        } else {
            Vec::new()
        };
        self.events
            .requirement_state(node_id, phase, status, message, &aliases);
    }

    fn has_app(&self) -> bool {
        has_app(&self.output_dir)
    }

    fn remaining(&self) -> f64 {
        self.budget.remaining()
    }

    /// Count a model turn and trip the global guardrail when the run has spent
    /// more tokens or turns than the policy allows (`budget.max_total_*`).
    fn note_turn(&mut self) {
        self.turns += 1;
        if self.degraded {
            return;
        }
        let totals = self.ledger.lock().map(|l| l.totals()).unwrap_or_default();
        let tokens = totals["total_tokens"].as_u64().unwrap_or(0);
        let (max_tokens, max_turns, abs) = (self.guard_tokens, self.guard_turns, self.guard_abs);
        let over = (max_tokens > 0 && tokens >= max_tokens)
            || (max_turns > 0 && self.turns >= max_turns)
            || (abs > 0 && tokens >= abs);
        if over {
            self.degraded = true;
            self.log(format!(
                "[guard] cost guard tripped: {tokens} billable tokens, {} turns (limits {max_tokens} / {max_turns} / abs {abs}); no further repair turns",
                self.turns
            ));
            self.events.emit(
                "guardrail",
                json!({"tokens": tokens, "turns": self.turns, "max_total_tokens": max_tokens, "max_turns": max_turns, "max_total_tokens_abs": abs}),
            );
        }
    }

    fn time_up(&self) -> bool {
        self.permanent_provider_error.is_some() || self.budget.time_up()
    }

    fn check_provider(&self) -> Result<()> {
        if let Some(error) = &self.permanent_provider_error {
            bail!("{error}");
        }
        Ok(())
    }

    fn record_provider_failure(&mut self, text: &str) {
        if crate::llm::is_permanent_provider_error(text) && self.permanent_provider_error.is_none()
        {
            self.permanent_provider_error = Some(text.to_owned());
            self.events
                .emit("provider_rejected", json!({"message": text}));
        }
    }

    fn corrections_text(&mut self) -> String {
        if self.pending_corrections.is_empty() {
            return String::new();
        }
        let items: Vec<String> = self
            .pending_corrections
            .drain(..)
            .map(|c| format!("- {c}"))
            .collect();
        self.prompts
            .render("corrections-header", &[("items", &items.join("\n"))])
            .unwrap_or_default()
    }

    fn correction(&self, key: &str, vars: &[(&str, &str)]) -> String {
        self.prompts
            .correction(key, vars)
            .unwrap_or_else(|_| key.to_string())
    }

    fn perf_text(&self) -> String {
        if self.policy.prompts.perf_contract && self.plan.needs_session {
            self.prompts.get("performance-contract").to_string()
        } else {
            String::new()
        }
    }

    fn ui_contract(&self) -> String {
        let mut blocks = vec![self.prompts.get("ui-contract-core")];
        if self.plan.needs_data {
            blocks.push(self.prompts.get("ui-contract-data"));
        }
        if self.plan.needs_session {
            blocks.push(self.prompts.get("ui-contract-session"));
        }
        blocks.concat()
    }

    fn verify_text(&self) -> String {
        if self.plan.minimal_verify {
            self.prompts.get("verify-minimal").to_string()
        } else {
            self.prompts
                .render("verify-full", &[("smoke", &self.smoke_port.to_string())])
                .unwrap_or_default()
        }
    }

    fn port_rules(&self) -> String {
        self.prompts
            .render(
                "port-rules",
                &[
                    ("smoke", &self.smoke_port.to_string()),
                    ("port", &self.web_port.to_string()),
                ],
            )
            .unwrap_or_default()
    }

    fn architecture_contract(&self) -> String {
        self.prompts
            .render(
                "architecture-contract",
                &[("port", &self.web_port.to_string())],
            )
            .unwrap_or_default()
    }

    fn sources_text(&self) -> String {
        let limit = self.policy.prompts.inline_source_chars;
        if limit == 0 {
            return String::new();
        }
        format!(
            "{}\n",
            codegen::inline_sources(&self.prompts, &self.output_dir, limit, false)
        )
    }

    fn codegen_mode(&self) -> bool {
        self.plan.codegen && !self.codegen_blocked
    }

    fn protected_prefixes(&self) -> Vec<String> {
        let mut prefixes = vec![
            ".arc/".to_string(),
            self.output_dir.join(".arc").to_string_lossy().into_owned(),
            "requirements/".to_string(),
            self.req_dir.to_string_lossy().into_owned(),
        ];
        if let Some(tests) = &self.tests_dir {
            prefixes.push(tests.to_string_lossy().into_owned());
        }
        prefixes
    }

    /// Just the spec file contents for a node (codegen prompts): the node's
    /// specs, then every helper file.
    fn spec_bodies(&self, node_id: &str) -> String {
        let Some(tests_dir) = &self.tests_dir else {
            return "(none)".into();
        };
        let mut files: Vec<String> = self.spec_map.specs_for(node_id).to_vec();
        for helper in acceptance::support_files(tests_dir) {
            if !files.contains(&helper) {
                files.push(helper);
            }
        }
        let mut parts = Vec::new();
        for rel in &files {
            let Ok(text) = std::fs::read_to_string(tests_dir.join(rel)) else {
                continue;
            };
            let text = text.trim();
            parts.push(if files.len() == 1 {
                text.to_string()
            } else {
                format!("--- {rel} ---\n{text}")
            });
        }
        if parts.is_empty() {
            "(none)".into()
        } else {
            parts.join("\n")
        }
    }

    /// Quote spec + helper files into the prompt (bounded). Each read the
    /// model would otherwise issue is a full-context round trip.
    fn inline_spec_text(&self, files: &[String]) -> String {
        let Some(tests_dir) = &self.tests_dir else {
            return String::new();
        };
        let max_chars = self.policy.prompts.inline_spec_chars;
        let mut parts = String::new();
        let mut total = 0usize;
        for rel in files {
            let Ok(text) = std::fs::read_to_string(tests_dir.join(rel)) else {
                continue;
            };
            let chars = text.chars().count();
            if total + chars > max_chars {
                return String::new(); // too big to inline; let the model read selectively
            }
            total += chars;
            parts.push_str(&format!("--- {rel} ---\n{}\n", text.trim_end()));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("{}{parts}", self.prompts.get("inline-spec-header"))
        }
    }

    fn port_contract_text(&self) -> String {
        if self.extra_ports.is_empty() {
            return String::new();
        }
        let ports = self
            .extra_ports
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        self.prompts
            .render(
                "port-contract",
                &[("ports", &ports), ("port", &self.web_port.to_string())],
            )
            .unwrap_or_default()
    }

    fn acceptance_tests_prompt(&self, files: &[String], inline: bool) -> String {
        let Some(tests_dir) = &self.tests_dir else {
            return String::new();
        };
        let listed: Vec<&str> = files.iter().take(40).map(String::as_str).collect();
        let files_text = if listed.is_empty() {
            "(none)".to_string()
        } else {
            listed.join(", ")
        };
        let mut text = self
            .prompts
            .render(
                "acceptance-tests",
                &[
                    ("tests_dir", &tests_dir.to_string_lossy()),
                    ("files", &files_text),
                ],
            )
            .unwrap_or_default();
        if inline {
            text.push_str(&self.inline_spec_text(files));
        }
        text.push_str(&self.port_contract_text());
        text
    }

    /// Tests paragraph for a node's turns; `None` lists every spec (final
    /// check), `skeleton` only points at the helpers.
    fn tests_prompt_for(&self, node_id: Option<&str>, skeleton: bool) -> String {
        let Some(tests_dir) = &self.tests_dir else {
            return String::new();
        };
        let support = acceptance::support_files(tests_dir);
        if skeleton {
            let listed: Vec<&str> = support.iter().take(10).map(String::as_str).collect();
            let support_text = if listed.is_empty() {
                "none".to_string()
            } else {
                listed.join(", ")
            };
            let intro = self
                .prompts
                .render(
                    "skeleton-tests",
                    &[
                        ("n_specs", &self.all_specs.len().to_string()),
                        ("tests_dir", &tests_dir.to_string_lossy()),
                        ("support", &support_text),
                    ],
                )
                .unwrap_or_default();
            // The one-line tests paragraph is dropped; only the port contract that follows it stays.
            let rest = self.acceptance_tests_prompt(&[], false);
            let after_first_line = rest.split_once('\n').map(|(_, r)| r).unwrap_or("");
            return format!("{intro}{after_first_line}");
        }
        let mut files: Vec<String> = node_id
            .map(|n| self.spec_map.specs_for(n).to_vec())
            .unwrap_or_default();
        if files.is_empty() {
            files = self.all_specs.clone();
        }
        files.extend(support);
        self.acceptance_tests_prompt(&files, self.policy.prompts.inline_specs)
    }

    fn ancestors_text(&self, node_id: &str) -> String {
        let ancestors = tree::ancestors_of(node_id, &self.ordered);
        if ancestors.is_empty() {
            return String::new();
        }
        let parts: Vec<String> = ancestors
            .iter()
            .map(|dep| match self.designs.get(dep) {
                Some(design) => {
                    let mut slim = serde_json::Map::new();
                    for key in ["routes", "pages", "data_model"] {
                        if let Some(value) = design.get(key).filter(|v| !v.is_null()) {
                            slim.insert(key.into(), value.clone());
                        }
                    }
                    format!("{dep}: {}", head(&Value::Object(slim).to_string(), 1500))
                }
                None => format!("{dep}: implemented (see code)"),
            })
            .collect();
        self.prompts
            .render("ancestors-note", &[("ancestors", &parts.join("\n"))])
            .unwrap_or_default()
    }

    /// The tool-mode implement prompt (`NODE_PROMPT`); corrections are consumed here.
    fn node_prompt(
        &mut self,
        node: &Value,
        node_id: &str,
        inline_design: bool,
        design: Option<&Value>,
    ) -> String {
        let mut design_text = match design {
            Some(design) => self
                .prompts
                .render(
                    "design-contract-note",
                    &[("design", &head(&design.to_string(), 4000))],
                )
                .unwrap_or_default(),
            None => String::new(),
        };
        if inline_design {
            design_text = self
                .prompts
                .render("inline-design-note", &[("node_id", node_id)])
                .unwrap_or_default();
        }
        if self.plan.evolution {
            let listing = codegen::source_listing(&self.output_dir, 60);
            design_text = format!(
                "{}{design_text}",
                self.prompts
                    .render("evolution-note", &[("listing", &listing)])
                    .unwrap_or_default()
            );
        } else if self.has_app() {
            let listing = codegen::source_listing(&self.output_dir, 60);
            design_text = format!(
                "{}{design_text}",
                self.prompts
                    .render("current-files-note", &[("listing", &listing)])
                    .unwrap_or_default()
            );
        }
        let preamble = if self.has_app() {
            self.prompts
                .render("node-preamble-extend", &[("node_id", node_id)])
                .unwrap_or_default()
        } else {
            let architecture = self.architecture_contract();
            self.prompts
                .render(
                    "node-preamble-create",
                    &[
                        ("node_id", node_id),
                        ("req_dir", &self.req_dir.to_string_lossy()),
                        ("port", &self.web_port.to_string()),
                        ("architecture_contract", &architecture),
                    ],
                )
                .unwrap_or_default()
        };
        self.prompts
            .render(
                "node",
                &[
                    ("preamble", &preamble),
                    ("node_spec", &tree::describe_node(node)),
                    ("design", &design_text),
                    ("ancestors", &self.ancestors_text(node_id)),
                    ("tests", &self.tests_prompt_for(Some(node_id), false)),
                    ("ui", &self.ui_contract()),
                    ("performance", &self.perf_text()),
                    ("verify", &self.verify_text()),
                    ("port_rules", &self.port_rules()),
                ],
            )
            .unwrap_or_default()
    }

    fn repair_prompt(
        &mut self,
        node_label: &str,
        counts: (usize, usize),
        failures: &str,
        extra_corrections: &str,
        slow: &str,
        specs: &[String],
    ) -> String {
        let (passed, total) = counts;
        let corrections = format!("{}{extra_corrections}", self.corrections_text());
        let original: Vec<String> = self
            .ordered
            .iter()
            .filter(|node| specs.is_empty() || tree::node_id(node) == node_label)
            .map(tree::describe_node)
            .collect();
        let requirements = if original.is_empty() {
            String::new()
        } else {
            format!(
                "Original requirements (read-only; preserve details even when acceptance does not assert them):\n{}\n",
                original.join("\n\n")
            )
        };
        let sources = format!("{requirements}{}", self.sources_text());
        let port_rules = self.port_rules();
        let mut test_location = self.tests_dir.as_ref().map(|path| {
            let path = path.canonicalize().unwrap_or_else(|_| path.clone());
            format!("Application directory: {}. Read-only acceptance directory: {}. Relative spec paths in failure reports refer to this directory. Read relevant specs and helpers here when needed.\n", self.output_dir.display(), path.display())
        }).unwrap_or_default();
        if let (Some(runner), Some(bundle), Some(tests)) =
            (&self.runner, &self.bundle_dir, &self.tests_dir)
        {
            let helper = bundle.join("verify_app.py");
            let binary = runner.root.join("node_modules/.bin/playwright");
            if helper.is_file() && binary.is_file() {
                let quote = |text: &str| format!("'{}'", text.replace('\'', "'\"'\"'"));
                let mut args = vec!["env".to_string()];
                for (key, value) in &runner.env_extra {
                    if key == "PLAYWRIGHT_BROWSERS_PATH" {
                        args.push(format!("{key}={value}"));
                    }
                }
                args.extend([
                    "python3".into(),
                    helper.display().to_string(),
                    "--app".into(),
                    self.output_dir.display().to_string(),
                    "--tests".into(),
                    tests.display().to_string(),
                    "--playwright".into(),
                    runner.root.display().to_string(),
                ]);
                let workers = if specs.is_empty() {
                    acceptance::workers_for_memory(
                        self.mem_limit,
                        self.policy.acceptance.final_workers,
                        self.policy.acceptance.final_memory_per_worker_mib,
                    )
                } else {
                    runner.workers
                };
                args.extend(["--workers".into(), workers.to_string()]);
                for spec in specs {
                    args.extend(["--spec".into(), spec.clone()]);
                }
                let command = args
                    .iter()
                    .map(|arg| quote(arg))
                    .collect::<Vec<_>>()
                    .join(" ");
                test_location.push_str(&format!("Isolated acceptance entry (builds and starts a disposable application copy):\n```sh\n{command}\n```\nUse this command for acceptance checks so test writes do not alter the source application's data. Edit the source application, not the disposable copy or read-only tests. The command prints failures and a report path. The harness re-runs acceptance after your edits.\n"));
            }
        }
        let failures_text = if failures.is_empty() {
            "(no detail)"
        } else {
            failures
        };
        self.prompts
            .render(
                "repair",
                &[
                    ("node_id", node_label),
                    ("passed", &passed.to_string()),
                    ("total", &total.to_string()),
                    ("failures", failures_text),
                    ("test_location", &test_location),
                    ("corrections", &corrections),
                    ("slow", slow),
                    ("sources", &sources),
                    ("port_rules", &port_rules),
                ],
            )
            .unwrap_or_default()
    }

    // -- acceptance -------------------------------------------------------

    /// Prefer the Playwright already on the machine (the runner image ships
    /// one). A private install is the last resort and never touches shared
    /// state: own npm cache, own browser dir, pinned version, removed at exit.
    fn setup_playwright(&mut self) {
        let Some(tests_dir) = self.tests_dir.clone() else {
            return;
        };
        let mut notes = Vec::new();
        let explicit = (!self.policy.acceptance.playwright_root.is_empty())
            .then(|| PathBuf::from(&self.policy.acceptance.playwright_root));
        let candidates = acceptance::playwright_candidates(
            explicit.as_deref(),
            self.bundle_dir.as_deref(),
            Some(&tests_dir),
            &self.output_dir,
        );
        let mut root = acceptance::find_playwright_root(&candidates);
        if root.is_none() {
            root = acceptance::find_playwright_by_search(&mut notes, 6, Duration::from_secs(25));
        }
        let mut env_extra = Vec::new();
        if root.is_none() && self.policy.acceptance.install_playwright {
            let version = acceptance::playwright_version_hint(
                Some(&tests_dir),
                &self.policy.acceptance.playwright_fallback_version,
            );
            notes.push(format!("[acceptance] no preinstalled Playwright found; private install of @playwright/test@{version}"));
            let private =
                std::env::temp_dir().join(format!("octos-arc-playwright-{}", std::process::id()));
            self.private_playwright = Some(private.clone());
            if let Some((found, extra)) = acceptance::ensure_playwright(
                &private,
                &mut notes,
                Duration::from_secs(self.policy.acceptance.install_timeout_seconds),
                &version,
            ) {
                root = Some(found);
                env_extra = extra;
            }
        }
        for note in notes {
            self.log(note);
        }
        let Some(root) = root else {
            self.log(
                "[acceptance] Playwright unavailable; nodes will be judged by the final check only",
            );
            return;
        };
        let limit = acceptance::container_memory_limit();
        self.mem_limit = limit;
        let workers = acceptance::workers_for_memory(
            limit,
            self.policy.acceptance.workers,
            self.policy.acceptance.memory_per_worker_mib,
        );
        let work_dir = acceptance::acceptance_work_dir(&root);
        self.runner = Some(AcceptanceRunner {
            root: root.clone(),
            tests_dir: tests_dir.clone(),
            work_dir,
            timeout_ms: self.policy.acceptance.test_timeout_ms,
            workers,
            fully_parallel: self.policy.acceptance.fully_parallel,
            env_extra,
            wall_timeout: Duration::from_secs(self.policy.acceptance.run_wall_timeout_seconds),
        });
        let limit_text = limit
            .map(|l| format!(" (container memory limit {} MiB)", l / (1024 * 1024)))
            .unwrap_or_default();
        self.log(format!(
            "[acceptance] using Playwright at {}; workers={workers}{limit_text}",
            root.display()
        ));
    }

    fn cleanup_playwright(&mut self) {
        if let Some(private) = self.private_playwright.take()
            && private.exists()
        {
            let _ = std::fs::remove_dir_all(&private);
            self.log(format!(
                "[acceptance] removed private Playwright install {}",
                private.display()
            ));
        }
    }

    fn app_server(&self, grader_like: bool) -> AppServer {
        let mut server = AppServer::new(
            &self.output_dir,
            self.smoke_port,
            grader_like,
            self.extra_ports.clone(),
        );
        server.build_timeout = Duration::from_secs(self.policy.acceptance.build_timeout_seconds);
        server.start_wait = Duration::from_secs(self.policy.acceptance.start_wait_seconds);
        server
    }

    /// Build, start, run the specs, then undo whatever the test run mutated
    /// (tests mutate persisted state; only data the requirement says persists
    /// across sessions may end up committed, so the worktree is restored).
    /// `grader_like` starts the backend with only PORT set, as the platform does.
    fn run_specs(
        &mut self,
        specs: &[String],
        workers: Option<u32>,
        grader_like: bool,
    ) -> RunSummary {
        self.git.snapshot_worktree();
        let started = Instant::now();
        let mut server = self.app_server(grader_like);
        let summary = match server.build().or_else(|| server.start()) {
            Some(error) => RunSummary::error(error),
            None => match &self.runner {
                Some(runner) => runner.run(
                    specs,
                    &format!("http://127.0.0.1:{}", self.smoke_port),
                    workers,
                ),
                None => RunSummary::error("no Playwright runner"),
            },
        };
        server.stop();
        self.git.restore_worktree();
        if summary.error.is_none() {
            self.log(format!(
                "[acceptance] {}/{} passed in {}s ({})",
                summary.passed,
                summary.total,
                started.elapsed().as_secs(),
                specs.join(", ")
            ));
        } else if let Some(error) = &summary.error
            && error.starts_with("Playwright collected 0 tests")
        {
            self.log(format!("[acceptance] {}", tail(error, 300)));
        }
        summary
    }

    fn record_tests(&mut self, node_id: &str, summary: &RunSummary) {
        for r in &summary.results {
            let test_id: String = head(&TEST_ID.replace_all(&r.title, "-"), 120);
            self.events.emit(
                "test_result",
                json!({"node_id": node_id, "test_id": test_id, "title": r.title, "file": r.file, "ok": r.ok, "type": "e2e"}),
            );
        }
    }

    fn failures_of(&self, summary: &RunSummary) -> String {
        format!(
            "{}{}",
            acceptance::failure_summaries(summary, 8, 900, self.policy.acceptance.test_timeout_ms),
            acceptance::failure_source_context(summary, self.tests_dir.as_deref(), 4000)
        )
    }

    fn startup_failure_digest(&self, error: &str, grader: bool) -> String {
        let limited = head(error, if grader { 700 } else { 600 });
        self.prompts
            .render(
                if grader {
                    "startup-failure-grader"
                } else {
                    "startup-failure"
                },
                &[("error", &limited)],
            )
            .unwrap_or_else(|_| format!("- Feature: app startup\n  Observation: {limited}"))
    }

    fn log_failure_lines(&mut self, failures: &str) {
        for line in failures.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("Failed at:") || trimmed.starts_with("Observation:") {
                let squashed: String = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
                self.log(format!("[acceptance]   {}", head(&squashed, 360)));
            }
        }
    }

    // -- kernel session (tool mode) ------------------------------------------

    fn hooks(&self) -> Vec<Value> {
        let mut dirs: Vec<PathBuf> = Vec::new();
        if let Some(tests) = &self.tests_dir {
            dirs.push(tests.clone());
        }
        if self.req_dir.is_dir() {
            dirs.push(self.req_dir.clone());
        }
        if dirs.is_empty() {
            return Vec::new();
        }
        let mut command: Vec<String> = vec![
            self.executable.to_string_lossy().into_owned(),
            "arc".into(),
            "deny-protected".into(),
        ];
        command.extend(dirs.iter().map(|d| d.to_string_lossy().into_owned()));
        vec![
            json!({"event": "before_tool_call", "command": command, "timeout_ms": 4000, "tool_filter": WRITE_TOOL_FILTER}),
        ]
    }

    fn tool_allowlist(&self) -> Option<Vec<String>> {
        let trim = self.policy.reasoning.trim_prompt;
        let drop_shell = self.plan.minimal_verify && self.policy.reasoning.drop_shell;
        if !trim && !drop_shell {
            return None;
        }
        Some(
            STDIO_TOOLS
                .iter()
                .filter(|name| !(trim && DROP_TOOLS.contains(name)))
                .filter(|name| !(drop_shell && SHELL_TOOLS.contains(name)))
                .map(|name| name.to_string())
                .collect(),
        )
    }

    fn driver(&mut self) -> Result<&mut Driver> {
        if self.driver.is_none() {
            let config_dir = tempfile::Builder::new().prefix("octos-config-").tempdir()?;
            let data_dir = tempfile::Builder::new().prefix("octos-data-").tempdir()?;
            let hooks = self.hooks();
            let (env, family_key) = driver::kernel_env(
                config_dir.path(),
                &self.route.provider,
                &self.route.api_key_env,
                self.smoke_port,
                self.policy.reasoning.destream,
            );
            let mut config = json!({
                "provider": self.route.provider,
                "model": self.route.model,
                "sandbox": {"allow_network": true},
                "memory": {"refresh": {"enabled": false}},
                "gateway": {"max_output_tokens": 65536},
                "hooks": hooks,
            });
            if !matches!(
                self.route.provider.as_str(),
                "openai" | "deepseek" | "anthropic"
            ) && !self.route.base_url.is_empty()
            {
                config["base_url"] = json!(self.route.base_url);
            }
            std::fs::write(
                config_dir.path().join("config.json"),
                serde_json::to_string_pretty(&config)?,
            )?;
            let kernel = KernelConfig {
                executable: self.executable.clone(),
                cwd: self.output_dir.clone(),
                data_dir: data_dir.path().to_path_buf(),
                env,
                danger_full_access: true,
                provider: self.route.provider.clone(),
                model: self.route.model.clone(),
                base_url: (!self.route.base_url.is_empty()).then(|| self.route.base_url.clone()),
                api_key_env: Some(family_key),
                hooks,
                max_output_tokens: self.policy.reasoning.max_tokens_min,
            };
            self.kernel_dirs.push(config_dir);
            self.kernel_dirs.push(data_dir);
            self.kernel_events = OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.output_dir.join(".arc/octos-events.jsonl"))
                .ok();
            self.driver = Some(Driver::new(
                kernel,
                &self.policy.session.scope,
                self.policy.reasoning.transient_retries,
                Duration::from_secs(self.policy.reasoning.transient_backoff_seconds),
            ));
        }
        Ok(self.driver.as_mut().expect("driver just created"))
    }

    fn end_scope(&mut self, scope: &str) {
        if let Some(driver) = self.driver.as_mut() {
            driver.end_scope(scope);
        }
    }

    fn close_driver(&mut self) {
        if let Some(driver) = self.driver.as_mut() {
            driver.close();
        }
    }

    /// One tool-mode turn: a kernel session turn watched by the guard, with
    /// the reasoning level, request cap and tool surface of this turn shape.
    fn turn(
        &mut self,
        prompt: &str,
        timeout: Duration,
        label: &str,
        expect_verification: bool,
        request_budget: Option<u32>,
    ) -> (bool, String) {
        if let Some(error) = &self.permanent_provider_error {
            return (false, error.clone());
        }
        // A model turn may invalidate every cached pre-generation verdict.
        self.probe_summaries.clear();
        if self.dry_run {
            self.note_turn();
            let writes = self.policy.debug.dry_run_tool_files
                && ["implement", "skeleton", "nudge", "repair", "rewrite"]
                    .iter()
                    .any(|k| label.contains(k));
            if writes {
                // Deep dry run: behave like a turn that wrote the placeholder app.
                let files = codegen::parse_file_blocks(crate::llm::DRYRUN_FILES);
                let _ = codegen::write_manifests(&self.output_dir);
                match codegen::write_files(&self.output_dir, &files) {
                    Ok(written) => self.log(format!(
                        "[flow] {label}: dry run, wrote the placeholder app ({} files)",
                        written.len()
                    )),
                    Err(error) => {
                        self.log(format!("[flow] {label}: dry run write failed: {error}"))
                    }
                }
                return (
                    true,
                    format!("dry run: placeholder app written for {label}"),
                );
            }
            self.log(format!("[flow] {label}: dry run, tool turn skipped"));
            return (true, format!("dry run: {label}"));
        }
        if let Some(routing) = &self.routing {
            routing.set_label(label);
        }
        self.note_turn();
        let mode = self.plan.reasoning_for(label);
        let budget = request_budget.unwrap_or_else(|| {
            if label.contains("repair") {
                self.policy.requests.repair
            } else if self.plan.minimal_verify {
                self.policy.requests.implement
            } else {
                self.policy.requests.implement_large
            }
        });
        let settings = TurnSettings {
            reasoning: mode,
            max_iterations: (budget > 0).then_some(budget),
            tools: self.tool_allowlist(),
        };
        let mut monitor = TurnMonitor::new(
            self.protected_prefixes(),
            vec![
                ".arc/design/".into(),
                self.output_dir
                    .join(".arc/design")
                    .to_string_lossy()
                    .into_owned(),
            ],
            expect_verification,
        );
        let started = Instant::now();
        let timeout = timeout.max(Duration::from_secs(60));
        let outcome = match self.driver() {
            Ok(_) => {
                let mut driver = self.driver.take().expect("driver");
                let events = &mut self.events;
                let kernel_events = self.kernel_events.as_mut();
                let mut observer = |method: &str, params: &Value| match method {
                    "harness/heartbeat" => events.log(format!(
                        "[flow] turn still running ({}s elapsed)",
                        params.get("elapsed_s").and_then(Value::as_u64).unwrap_or(0)
                    )),
                    "harness/retry" => events.log(format!(
                        "[driver] transient error, retry {}/{} after {}s: {}",
                        params.get("attempt").and_then(Value::as_u64).unwrap_or(0),
                        params.get("of").and_then(Value::as_u64).unwrap_or(0),
                        params.get("wait_s").and_then(Value::as_u64).unwrap_or(0),
                        params.get("error").and_then(Value::as_str).unwrap_or("")
                    )),
                    "core/marker" => events.log(format!(
                        "[core-mod] {}",
                        params.get("line").and_then(Value::as_str).unwrap_or("")
                    )),
                    _ => {
                        monitor.observe(method, params);
                        if let Some(file) = kernel_events.as_deref() {
                            let _ =
                                writeln!(&*file, "{}", json!({"method": method, "params": params}));
                        }
                    }
                };
                let outcome = driver.run(prompt, timeout, &settings, &mut observer);
                self.driver = Some(driver);
                outcome
            }
            Err(error) => driver::TurnOutcome {
                ok: false,
                text: format!("stdio driver error: {error:#}"),
                requests: 0,
                tokens_in: 0,
                tokens_out: 0,
                cache_hit: 0,
                tool_calls: 0,
            },
        };
        let mut ok = outcome.ok;
        let text = outcome.text;
        if !ok {
            self.record_provider_failure(&text);
        }
        monitor.finish(&text);
        let elapsed_ms = started.elapsed().as_millis() as u64;
        if outcome.requests > 0 || outcome.tokens_in > 0 {
            let usage = octos_llm::TokenUsage {
                input_tokens: outcome.tokens_in,
                output_tokens: outcome.tokens_out,
                cache_read_tokens: outcome.cache_hit,
                ..Default::default()
            };
            if let Ok(mut ledger) = self.ledger.lock() {
                ledger.record_turn(label, mode, outcome.requests, &usage, elapsed_ms);
            }
            self.events.emit(
                "usage",
                json!({"label": label, "mode": mode.label(), "elapsed_ms": elapsed_ms, "requests": outcome.requests,
                    "prompt_tokens": outcome.tokens_in + outcome.cache_hit, "completion_tokens": outcome.tokens_out,
                    "cache_hit_tokens": outcome.cache_hit, "tool_calls": outcome.tool_calls}),
            );
        }
        // The request cap ends the turn with an error; the files written so far are what count.
        if !ok
            && self.permanent_provider_error.is_none()
            && budget > 0
            && (text.contains("budget") || text.contains("iteration"))
        {
            self.log(format!(
                "[guard] {label}: request budget {budget} hit; turn forced to finish"
            ));
            ok = true;
        }
        self.log(format!(
            "[flow] {label} {} in {}s (tools={} wrote={} verified={}): {:?}",
            if ok { "ok" } else { "FAILED" },
            elapsed_ms / 1000,
            monitor.tool_calls,
            monitor.wrote_files,
            monitor.verified,
            tail(&text, 240)
        ));
        for correction in monitor.corrections(&self.prompts) {
            self.log(format!("[guard] {label}: {}", head(&correction, 160)));
            if self.policy.prompts.guard {
                self.pending_corrections.push(correction);
            }
        }
        let restored = self
            .protected
            .as_ref()
            .map(|p| p.restore())
            .unwrap_or_default();
        if !restored.is_empty() {
            self.log(format!(
                "[guard] restored {} protected file(s): {:?}",
                restored.len(),
                restored.iter().take(5).collect::<Vec<_>>()
            ));
            let files: Vec<&str> = restored.iter().take(5).map(String::as_str).collect();
            let correction = self.correction("protected_restored", &[("files", &files.join(", "))]);
            self.pending_corrections.push(correction);
        }
        (ok, text)
    }

    /// Reasoning effort for a codegen turn, derived from the size of the spec it
    /// must satisfy (`main.codegen_reasoning`): small specs are generated
    /// correctly without reasoning; large ones keep the base mode. Only when the
    /// policy mode is auto.
    fn codegen_reasoning(&self, spec_chars: usize) -> Option<ReasoningMode> {
        if self.policy.reasoning.mode != "auto" {
            return None;
        }
        (spec_chars > 0 && spec_chars < self.policy.reasoning.codegen_reasoning_chars)
            .then_some(ReasoningMode::Disabled)
    }

    fn codegen_repair_prompt(&self, node_id: &str, prompt: &str) -> Option<String> {
        let spec = self.spec_bodies(node_id);
        if spec.is_empty() || spec == "(none)" {
            return None;
        }
        let suffix = self
            .prompts
            .render("codegen-repair-suffix", &[("spec", &spec)])
            .ok()?;
        // Requote source against the total allowance; the complete prompt below
        // includes requirements, acceptance, headings and format instructions.
        let current_sources = self.sources_text();
        let prompt = if !current_sources.trim().is_empty() && prompt.contains(&current_sources) {
            let sources = codegen::inline_sources(
                &self.prompts,
                &self.output_dir,
                self.policy.prompts.codegen_context_chars,
                false,
            );
            if sources
                .lines()
                .any(|line| line.starts_with("--- ") && line.contains(" --- (omitted,"))
            {
                return None;
            }
            prompt.replacen(&current_sources, &sources, 1)
        } else {
            prompt.to_string()
        };
        let full = format!("{prompt}{suffix}");
        (full.chars().count() + self.prompts.get("codegen-format").chars().count()
            <= self.policy.prompts.codegen_context_chars)
            .then_some(full)
    }

    fn codegen_context_fits(&self, spec_chars: usize) -> bool {
        let limit = self.policy.prompts.codegen_context_chars;
        (spec_chars as f64) < limit as f64 * 0.6
            && codegen::sources_fit(&self.output_dir, limit.saturating_sub(spec_chars).max(8000))
    }

    /// `main.all_specs_tiny`: every node that has specs falls in the tiny tier (and at least one does).
    fn all_specs_tiny(&self) -> bool {
        let sizes: Vec<usize> = self
            .node_ids
            .iter()
            .filter(|n| !self.spec_map.specs_for(n).is_empty())
            .map(|n| self.spec_bodies(n).chars().count())
            .collect();
        !sizes.is_empty() && sizes.iter().all(|s| self.tiny_mode(*s))
    }

    /// `main.tiny_mode`: the tiny tier applies to specs below the size threshold.
    fn tiny_mode(&self, spec_chars: usize) -> bool {
        self.policy.mode.tiny && spec_chars > 0 && spec_chars < self.policy.mode.tiny_spec_chars
    }

    /// Tiny-spec tier (`main.tiny_turn`): the harness writes the manifests and a
    /// fixed static server, the model returns one index.html for the spec's
    /// statements. True only when the node's specs pass right away; otherwise
    /// the caller falls back to the compact tier.
    fn tiny_turn(
        &mut self,
        node_id: &str,
        specs: &[String],
        timeout: Duration,
        requirement: &Value,
    ) -> bool {
        match codegen::write_manifests(&self.output_dir) {
            Ok(written) if !written.is_empty() => {
                self.log(format!("[codegen] wrote manifests {written:?}"))
            }
            Ok(_) => {}
            Err(error) => self.log(format!("[codegen] could not write manifests: {error}")),
        }
        let server = self.output_dir.join("backend/server.js");
        if !server.exists() {
            match codegen::tiny_server_js(&self.prompts, self.web_port, &self.extra_ports) {
                Ok(js) => {
                    let _ = std::fs::create_dir_all(server.parent().unwrap());
                    if let Err(error) = std::fs::write(&server, js) {
                        self.log(format!(
                            "[codegen] could not write the tiny server: {error}"
                        ));
                    }
                }
                Err(error) => self.log(format!("[codegen] tiny server template: {error}")),
            }
        }
        let spec = format!(
            "Requirement: {requirement}\nPublic example:\n{}",
            self.spec_bodies(node_id)
        );
        let page = self.output_dir.join("frontend/src/index.html");
        let prompt = if page.is_file() {
            let current = std::fs::read_to_string(&page).unwrap_or_default();
            self.prompts.render(
                "tiny-prompt-evolution",
                &[("page", current.trim()), ("spec", &spec)],
            )
        } else {
            self.prompts.render("tiny-prompt", &[("spec", &spec)])
        };
        let prompt = match prompt {
            Ok(text) => text,
            Err(error) => {
                self.log(format!("[flow] {node_id}: tiny prompt: {error}"));
                return false;
            }
        };
        self.current_spec_chars = spec.chars().count();
        let (ok, _) = self.codegen_turn_with(
            &prompt,
            timeout,
            &format!("{node_id} implement (tiny)"),
            CodegenOptions {
                system: Some("tiny-system"),
                format: false,
                raw_target: Some("frontend/src/index.html"),
            },
        );
        if !ok || !page.is_file() || self.runner.is_none() || specs.is_empty() {
            self.log(format!(
                "[flow] {node_id}: tiny tier produced no page; compact tier next"
            ));
            return false;
        }
        let summary = self.run_specs(specs, None, false);
        if summary.error.is_some() {
            self.log(format!(
                "[flow] {node_id}: tiny tier could not run specs: {}",
                summary.error.as_deref().unwrap_or("unknown error")
            ));
            return false;
        }
        let passed = summary.total > 0 && summary.passed == summary.total;
        self.log(format!(
            "[flow] {node_id}: tiny tier {} its specs ({}/{})",
            if passed { "passed" } else { "failed" },
            summary.passed,
            summary.total
        ));
        if !passed {
            self.log(format!(
                "[acceptance] {node_id} first-attempt failure: {}",
                self.failures_of(&summary)
            ));
        }
        passed
    }

    /// Run a tool-less turn; parse and write the file blocks from the reply.
    /// Returns (ok, text) like the Python `codegen_turn`.
    fn codegen_turn(&mut self, prompt: &str, timeout: Duration, label: &str) -> (bool, String) {
        self.codegen_turn_with(prompt, timeout, label, CodegenOptions::default())
    }

    fn codegen_turn_with(
        &mut self,
        prompt: &str,
        timeout: Duration,
        label: &str,
        options: CodegenOptions<'_>,
    ) -> (bool, String) {
        if let Some(error) = &self.permanent_provider_error {
            return (false, error.clone());
        }
        // A model turn may invalidate every cached pre-generation verdict.
        self.probe_summaries.clear();
        let user = if options.format {
            codegen::with_format(&self.prompts, prompt)
        } else {
            prompt.trim().to_string()
        };
        self.note_turn();
        let mode = self
            .codegen_reasoning(self.current_spec_chars)
            .unwrap_or_else(|| self.plan.reasoning_for(label));
        let started = Instant::now();
        let request = CompletionRequest {
            label,
            system: self.prompts.get(options.system.unwrap_or("codegen-system")),
            user: &user,
            mode,
            timeout: timeout.max(Duration::from_secs(60)),
        };
        if let Some(routing) = &self.routing {
            routing.set_label(label);
        }
        let result = self.llm.complete(&request);
        let (ok, text) = match result {
            Ok(completion) => {
                self.events.emit(
                    "usage",
                    json!({"label": label, "mode": mode.label(), "elapsed_ms": completion.elapsed_ms, "attempts": completion.attempts, "requests": 1,
                        "prompt_tokens": u64::from(completion.usage.input_tokens) + u64::from(completion.usage.cache_read_tokens),
                        "completion_tokens": completion.usage.output_tokens, "reasoning_tokens": completion.usage.reasoning_tokens,
                        "cache_hit_tokens": completion.usage.cache_read_tokens, "truncated": completion.truncated}),
                );
                let mut files = codegen::parse_file_blocks(&completion.text);
                if files.is_empty()
                    && let Some(target) = options.raw_target
                {
                    // Tiny tier: the reply is a bare HTML document (code fences tolerated).
                    let html = codegen::strip_code_fences(&completion.text);
                    if codegen::looks_like_markup(&html) {
                        files.insert(target.to_string(), html);
                    }
                }
                if files.is_empty() {
                    // Keep the reply for diagnosis: a codegen answer without file
                    // blocks is otherwise invisible (the platform keeps the
                    // workspace, not our stdout).
                    let dump = self
                        .output_dir
                        .join(".arc/codegen")
                        .join(format!("{}.reply.txt", TEST_ID.replace_all(label, "-")));
                    let _ = std::fs::create_dir_all(dump.parent().unwrap());
                    let _ = std::fs::write(&dump, &completion.text);
                    if completion.truncated {
                        (
                            false,
                            "output truncated by the model's max_tokens; no complete file block"
                                .to_string(),
                        )
                    } else {
                        self.log(format!("[codegen] {label}: reply contained no file blocks"));
                        (
                            false,
                            "codegen reply contained no <<<FILE>>> blocks".to_string(),
                        )
                    }
                } else {
                    match codegen::write_files(&self.output_dir, &files) {
                        Ok(written) => {
                            let shown: Vec<&String> = written.iter().take(8).collect();
                            self.log(format!(
                                "[codegen] {label}: wrote {} file(s): {shown:?}",
                                written.len()
                            ));
                            let deduped = codegen::dedupe_nav_links(&self.output_dir);
                            if !deduped.is_empty() {
                                self.log(format!("[codegen] {label}: removed static nav links duplicating the NAV placeholder in {deduped:?}"));
                            }
                            if completion.truncated {
                                self.log(format!("[codegen] {label}: reply was truncated by max_tokens; testing the files that arrived"));
                            }
                            (true, completion.text)
                        }
                        Err(error) => (false, format!("could not write files: {error}")),
                    }
                }
            }
            Err(error) => {
                let text = format!("{error:#}");
                self.record_provider_failure(&text);
                (false, text)
            }
        };
        self.log(format!(
            "[flow] {label} {} in {}s: {:?}",
            if ok { "ok" } else { "FAILED" },
            started.elapsed().as_secs(),
            tail(&text, 240)
        ));
        (ok, text)
    }

    /// Copy the app sources the next repair will overwrite into
    /// `.arc/codegen/<node>-r<attempt>/` (the platform keeps the workspace but
    /// not our git history).
    fn snapshot_sources(&mut self, node_id: &str, attempt: u32) {
        let dest = self
            .output_dir
            .join(".arc/codegen")
            .join(format!("{node_id}-r{attempt}"));
        let _ = std::fs::remove_dir_all(&dest);
        let mut count = 0;
        for rel in ["frontend/src", "backend"] {
            let src = self.output_dir.join(rel);
            if !src.is_dir() {
                continue;
            }
            let mut stack = vec![src.clone()];
            while let Some(dir) = stack.pop() {
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        if entry.file_name() != "node_modules" {
                            stack.push(path);
                        }
                        continue;
                    }
                    let keep = path
                        .extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| matches!(e, "html" | "js" | "json" | "css"));
                    if !keep {
                        continue;
                    }
                    let target = dest.join(path.strip_prefix(&self.output_dir).unwrap_or(&path));
                    if let Some(parent) = target.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if std::fs::copy(&path, &target).is_ok() {
                        count += 1;
                    }
                }
            }
        }
        self.log(format!("[flow] {node_id}: {count} source file(s) snapshotted to .arc/codegen/{node_id}-r{attempt}"));
    }

    /// `main.discard_template`: move frontend/ and backend/ of a non-working
    /// existing app to `.arc/template-discarded/` so the fresh build starts from
    /// our own layout.
    fn discard_template(&mut self) {
        let dest = self.output_dir.join(".arc/template-discarded");
        let _ = std::fs::remove_dir_all(&dest);
        if let Err(error) = std::fs::create_dir_all(&dest) {
            self.log(format!(
                "[flow] could not set the existing app aside: {error}"
            ));
            return;
        }
        let mut moved = Vec::new();
        for name in ["frontend", "backend"] {
            let src = self.output_dir.join(name);
            if src.exists() {
                match std::fs::rename(&src, dest.join(name)) {
                    Ok(()) => moved.push(name),
                    Err(error) => self.log(format!("[flow] could not set {name}/ aside: {error}")),
                }
            }
        }
        self.log(format!(
            "[flow] existing app passes no spec; moved {moved:?} to .arc/template-discarded and building fresh"
        ));
    }

    fn restore_app(&mut self, sha: &str) {
        self.git.restore_app(sha);
        self.log(format!(
            "[flow] restored frontend/ and backend/ to best commit {}",
            &sha[..sha.len().min(8)]
        ));
    }

    fn commit(&mut self, message: &str) {
        match self.git.commit(message) {
            Ok(true) => {
                let sha = self.git.head().unwrap_or_default();
                self.events
                    .emit("commit", json!({"message": message, "sha": sha}));
            }
            Ok(false) => {}
            Err(error) => self.log(format!("[git] commit failed: {error}")),
        }
    }

    /// Returns Some(true/false) for a real verdict, None when no local run
    /// happened. `rebuild` yields a full re-implementation prompt; it is used
    /// once when round 0 passes nothing — rewriting beats patching a
    /// structurally broken first attempt.
    fn can_rewrite_from_scratch(&self) -> bool {
        !self.test_verdict.values().any(|v| *v == Some(true))
            && !self.probe_summaries.values().any(|r| r.passed > 0)
    }

    fn acceptance_loop(
        &mut self,
        node_id: &str,
        specs: &[String],
        deadline: Instant,
        rebuild: Option<&Rebuild>,
    ) -> Option<bool> {
        if self.runner.is_none() || specs.is_empty() {
            return None;
        }
        let repair_rounds = if self.plan.n_nodes > self.policy.repair.large_tree_nodes {
            self.policy.repair.rounds_large_tree
        } else {
            self.policy.repair.rounds
        };
        let mut best_passed: i64 = -1;
        let mut best_sha = self.git.head();
        let mut regressions = 0u32;
        let mut stalls = 0u32;
        let mut rewrite_used = false;
        let mut previous_failures = None;
        self.codegen_blocked = false;
        for attempt in 0..=repair_rounds {
            let mut summary = self.run_specs(specs, None, false);
            if summary.error.is_some() && summary.killed {
                self.log(format!(
                    "[acceptance] {node_id}: test runner killed ({}); no verdict from this round",
                    head(summary.error.as_deref().unwrap_or(""), 120)
                ));
                return None;
            }
            let (passed, failures) = if let Some(error) = summary.error.clone() {
                self.log(format!(
                    "[acceptance] {node_id} infrastructure error: {}",
                    head(&error, 300)
                ));
                let digest = self.startup_failure_digest(&error, false);
                summary = RunSummary {
                    passed: 0,
                    total: specs.len().max(1),
                    ..Default::default()
                };
                (0usize, digest)
            } else {
                self.record_tests(node_id, &summary);
                (summary.passed, self.failures_of(&summary))
            };
            self.log(format!(
                "[acceptance] {node_id} round {attempt}: {passed}/{}",
                summary.total
            ));
            let was_codegen = self.codegen_mode();
            let normalized = if summary.results.is_empty() {
                BTreeSet::from([vec![failures.clone()]])
            } else {
                acceptance::failure_signature(&summary)
            };
            if !normalized.is_empty() && previous_failures.as_ref() == Some(&normalized) {
                self.codegen_blocked = true;
                let correction = self.correction("identical_failure", &[]);
                self.pending_corrections.push(correction);
                self.log(format!(
                    "[flow] {node_id}: identical failure twice; switching repairs to tool mode"
                ));
            }
            previous_failures = Some(normalized);
            if attempt >= self.policy.repair.codegen_repairs
                && passed < summary.total
                && self.codegen_mode()
            {
                // Repeated codegen repairs re-emit the same files; one cheap codegen repair is allowed, then tools.
                self.codegen_blocked = true;
                self.log(format!("[flow] {node_id}: codegen attempt {attempt} still failing; repairs use tool mode"));
            }
            self.log_failure_lines(&failures);
            if summary.total > 0 && passed == summary.total {
                self.commit(&format!(
                    "{node_id} (accepted): {passed}/{} acceptance tests pass",
                    summary.total
                ));
                return Some(true);
            }
            let passed_i = passed as i64;
            if passed_i > best_passed {
                if best_passed >= 0 {
                    self.commit(&format!(
                        "{node_id} (repair {attempt}): {passed}/{} pass",
                        summary.total
                    ));
                }
                best_passed = passed_i;
                best_sha = self.git.head();
                regressions = 0;
                stalls = 0;
            } else if passed_i == best_passed && attempt > 0 {
                stalls += 1;
                // A newly selected strategy gets one attempt within the existing budgets.
                if stalls >= self.policy.repair.stall_limit
                    && !(was_codegen && self.codegen_blocked)
                {
                    self.log(format!(
                        "[flow] {node_id}: no improvement for two repairs; keeping the best state"
                    ));
                    break;
                }
            } else if passed_i < best_passed {
                regressions += 1;
                if regressions >= self.policy.repair.regression_limit
                    && let Some(sha) = best_sha.clone()
                {
                    self.restore_app(&sha);
                    let correction = self.correction(
                        "regressions_restored",
                        &[
                            ("best", &best_passed.to_string()),
                            ("total", &summary.total.to_string()),
                        ],
                    );
                    self.pending_corrections.push(correction);
                    regressions = 0;
                }
            }
            if attempt == repair_rounds {
                break;
            }
            let left = deadline
                .saturating_duration_since(Instant::now())
                .as_secs_f64();
            if left < self.policy.budget.min_repair_seconds as f64 || self.time_up() {
                self.log(format!("[flow] {node_id}: {left:.0}s left, below the {}s a repair needs; keeping the best state", self.policy.budget.min_repair_seconds));
                break;
            }
            if self.degraded {
                self.log(format!(
                    "[flow] {node_id}: guardrail active; no repair turns, keeping the best state"
                ));
                break;
            }
            self.snapshot_sources(node_id, attempt);
            let slow = summary.slow(self.policy.acceptance.slow_ms);
            let slow_text = if slow.is_empty() {
                String::new()
            } else {
                let perf = self.perf_text();
                self.prompts
                    .render(
                        "slow-tests",
                        &[("slow", &slow.join("; ")), ("performance", &perf)],
                    )
                    .unwrap_or_default()
            };
            let turn_timeout =
                Duration::from_secs_f64(left.min(self.policy.budget.node_timeout_seconds as f64));
            if passed == 0
                && !rewrite_used
                && self.policy.repair.rewrite_on_zero
                && best_passed <= 0
                && self.can_rewrite_from_scratch()
                && let Some(rebuild) = rebuild
            {
                rewrite_used = true;
                self.log(format!(
                    "[flow] {node_id}: nothing passed; one full rewrite turn instead of a patch"
                ));
                let failures_text = if failures.is_empty() {
                    "(no detail)".to_string()
                } else {
                    failures.clone()
                };
                let label = format!("{node_id} rewrite (repair {})", attempt + 1);
                if self.codegen_mode()
                    && let Some(codegen_prompt) = &rebuild.codegen_prompt
                {
                    let spec_text = self.spec_bodies(node_id);
                    match codegen::rewrite_prompt(
                        &self.prompts,
                        codegen_prompt,
                        &failures_text,
                        &self.output_dir,
                        &spec_text,
                        self.policy.prompts.codegen_context_chars,
                    ) {
                        Ok(prompt) => {
                            self.codegen_turn(&prompt, turn_timeout, &label);
                        }
                        Err(error) => self.log(format!(
                            "[flow] {node_id}: could not build the rewrite prompt: {error}"
                        )),
                    }
                } else {
                    let sources = self.sources_text();
                    let prompt = self
                        .prompts
                        .render(
                            "rewrite",
                            &[
                                ("prompt", &rebuild.tool_prompt),
                                ("failures", &failures_text),
                                ("sources", &sources),
                            ],
                        )
                        .unwrap_or_default();
                    let budget = self.policy.requests.implement;
                    self.turn(&prompt, turn_timeout, &label, true, Some(budget));
                }
                continue;
            }
            let prompt = self.repair_prompt(
                node_id,
                (passed, summary.total),
                &failures,
                "",
                &slow_text,
                specs,
            );
            let label = format!("{node_id} repair {}/{repair_rounds}", attempt + 1);
            let compact = if self.codegen_mode() {
                self.codegen_repair_prompt(node_id, &prompt)
            } else {
                None
            };
            if let Some(compact) = compact {
                self.codegen_turn(&compact, turn_timeout, &label);
            } else {
                if self.codegen_mode() {
                    self.codegen_blocked = true;
                    self.log(format!("[flow] {node_id}: complete repair evidence unavailable within codegen budget; using tools"));
                }
                self.turn(&prompt, turn_timeout, &label, true, None);
            }
        }
        // A failed repair can change files without changing HEAD.
        if best_passed > 0
            && let Some(sha) = best_sha
        {
            self.restore_app(&sha);
            self.commit(&format!(
                "{node_id}: keep best acceptance state {best_passed}"
            ));
        }
        Some(false)
    }

    // -- per node ---------------------------------------------------------

    /// Separate design turn: read the specs and the code, write ONE JSON
    /// contract to .arc/design/<node>.json.
    fn design(&mut self, node: &Value, node_id: &str, deadline: Instant) -> Option<Value> {
        let prompt = self
            .prompts
            .render(
                "design",
                &[
                    ("node_id", node_id),
                    ("node_spec", &tree::describe_node(node)),
                    ("ancestors", &self.ancestors_text(node_id)),
                    ("tests", &self.tests_prompt_for(Some(node_id), false)),
                ],
            )
            .unwrap_or_default();
        let timeout = Duration::from_secs(self.policy.budget.design_timeout_seconds)
            .min(deadline.saturating_duration_since(Instant::now()));
        let (ok, text) = self.turn(&prompt, timeout, &format!("{node_id} design"), false, None);
        let mut design: Option<Value> = None;
        if ok {
            let candidate = JSON_FENCE
                .captures(&text)
                .or_else(|| JSON_ANY.captures(&text))
                .map(|c| c[1].to_string());
            design = candidate
                .and_then(|c| serde_json::from_str::<Value>(&c).ok())
                .filter(Value::is_object);
        }
        if design.is_none() {
            let written = self
                .output_dir
                .join(".arc/design")
                .join(format!("{node_id}.json"));
            if let Ok(text) = std::fs::read_to_string(&written)
                && let Ok(value) = serde_json::from_str::<Value>(&text)
                && value.is_object()
            {
                self.log(format!(
                    "[flow] {node_id}: design read from .arc/design/{node_id}.json"
                ));
                design = Some(value);
            }
        }
        if design.is_none() {
            self.log(format!(
                "[flow] {node_id}: design turn produced no JSON; continuing with prose design"
            ));
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                design = Some(json!({"notes": tail(trimmed, 1500)}));
            }
        }
        design
    }

    fn save_design(&mut self, node_id: &str, design: &Value) {
        let dir = self.output_dir.join(".arc/design");
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(
            dir.join(format!("{node_id}.json")),
            serde_json::to_string_pretty(design).unwrap_or_default(),
        );
        self.designs.insert(node_id.to_string(), design.clone());
        self.events
            .emit("design", json!({"node_id": node_id, "design": design}));
    }

    fn node_cycle(&mut self, node: &Value, index: usize, total: usize) {
        let node_id = tree::node_id(node);
        let specs: Vec<String> = self.spec_map.specs_for(&node_id).to_vec();
        self.codegen_blocked = false;
        if index > 1 {
            let killed = crate::reap::sweep_workspace(&self.output_dir);
            if !killed.is_empty() {
                self.log(format!(
                    "[reap] killed {} leftover process(es) inside frontend/ or backend/",
                    killed.len()
                ));
            }
        }
        let nodes_left = total - index + 1;
        let node_budget = budget::node_budget_seconds(
            self.policy.budget.node_time_budget_seconds,
            self.policy.budget.node_time_floor_seconds,
            self.remaining(),
            nodes_left,
        );
        let deadline = Instant::now() + Duration::from_secs_f64(node_budget.max(1.0));
        self.log(format!("[flow] node {index}/{total} {node_id} starting (budget {node_budget:.0}s, specs={specs:?})"));

        self.mark("design_started", &node_id, None);
        let design_wanted = self.plan.design_enabled;
        let inline_design = design_wanted && self.plan.design_inline;
        let mut design: Option<Value> = None;
        if design_wanted && !inline_design {
            design = self.design(node, &node_id, deadline);
        }
        if let Some(design) = &design {
            self.save_design(&node_id, design);
            self.mark(
                "design_done",
                &node_id,
                Some(&format!(
                    "design JSON written to .arc/design/{node_id}.json"
                )),
            );
        } else if !inline_design {
            self.mark(
                "design_done",
                &node_id,
                Some("design folded into the implementation prompt"),
            );
        }

        self.mark("implementation_started", &node_id, None);
        let corrections = self.corrections_text();
        let tool_prompt = format!(
            "{corrections}{}",
            self.node_prompt(node, &node_id, inline_design, design.as_ref())
        );
        let mut codegen_prompt: Option<String> = None;
        let implement_timeout = (self.policy.budget.node_timeout_seconds as f64)
            .min(self.policy.budget.implement_fraction * node_budget)
            .min(
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_secs_f64(),
            );
        let implement_timeout = Duration::from_secs_f64(implement_timeout.max(1.0));
        let spec_text = self.spec_bodies(&node_id);
        let spec_chars = spec_text.chars().count();
        let mut tiny_ok = false;
        if corrections.is_empty() && self.codegen_mode() && self.tiny_mode(spec_chars) {
            tiny_ok = self.tiny_turn(&node_id, &specs, implement_timeout, node);
            self.current_spec_chars = spec_chars;
        }
        let (mut ok, mut text) = if tiny_ok {
            (true, "tiny tier: specs pass".to_string())
        } else if self.codegen_mode() && self.codegen_context_fits(spec_chars) {
            let description = node
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            self.current_spec_chars = spec_chars;
            // Small specs (by size, an input-derived measure) get the compact rule; the
            // multi-page mechanisms only apply when the spec is large enough to need
            // sessions/navigation.
            let small = self.codegen_reasoning(spec_chars) == Some(ReasoningMode::Disabled);
            let existing = self.has_app();
            let inputs = CodegenInputs {
                node_id: &node_id,
                description: &description,
                spec: &spec_text,
                web_port: self.web_port,
                extra_ports: &self.extra_ports,
                n_nodes: self.plan.n_nodes,
                small_rule: small,
                existing_app: existing.then_some(self.output_dir.as_path()),
                context_chars: self.policy.prompts.codegen_context_chars,
            };
            let compact = match codegen::implement_prompt(&self.prompts, &inputs) {
                Ok(prompt) => format!("{corrections}{prompt}"),
                Err(error) => {
                    let message = format!("could not build the codegen prompt: {error}");
                    self.mark("implementation_failed", &node_id, Some(&message));
                    self.impl_failed.push(node_id);
                    return;
                }
            };
            match codegen::write_manifests(&self.output_dir) {
                Ok(written) if !written.is_empty() => {
                    self.log(format!("[codegen] wrote manifests {written:?}"))
                }
                Ok(_) => {}
                Err(error) => self.log(format!("[codegen] could not write manifests: {error}")),
            }
            let mut result =
                self.codegen_turn(&compact, implement_timeout, &format!("{node_id} implement"));
            if !result.0 && result.1.contains("no <<<FILE>>> blocks") {
                // A reply without file blocks writes nothing; one more request with the
                // format reminder is far cheaper than skipping the node (a skipped node
                // takes every dependent node down with it).
                self.log(format!(
                    "[flow] {node_id}: codegen reply had no file blocks; retrying once with the format reminder"
                ));
                let reminder = self.correction("codegen_no_blocks", &[]);
                let retry = format!("{compact}\n{reminder}");
                let left = deadline
                    .saturating_duration_since(Instant::now())
                    .min(implement_timeout);
                result = self.codegen_turn(&retry, left, &format!("{node_id} implement (retry)"));
            }
            codegen_prompt = Some(compact);
            result
        } else {
            if self.codegen_mode() {
                self.log(format!(
                    "[flow] {node_id}: spec or existing source exceeds one-request allowance ({spec_chars} spec chars); tool mode"
                ));
            }
            self.turn(
                &tool_prompt,
                implement_timeout,
                &format!("{node_id} implement"),
                true,
                None,
            )
        };
        if !ok && text.to_lowercase().contains("truncated") && !self.codegen_mode() {
            // Output cut by max_tokens, nothing written. Retry once, one file per response (fresh session, same prompt).
            self.log(format!(
                "[flow] {node_id}: output truncated; retrying with one file per response"
            ));
            self.close_driver();
            let retry = self
                .prompts
                .render("truncated-retry", &[("prompt", &tool_prompt)])
                .unwrap_or_default();
            let left = deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_secs(self.policy.budget.node_timeout_seconds));
            (ok, text) = self.turn(
                &retry,
                left,
                &format!("{node_id} implement (retry)"),
                true,
                None,
            );
        }
        let timed_out = !ok && text.to_lowercase().contains("timed out");
        if ok && !self.has_app() {
            self.log(format!("[flow] {node_id}: app layout incomplete after the turn; acceptance loop will drive the repair"));
            let correction = self.correction("layout_incomplete", &[]);
            self.pending_corrections.push(correction);
        }
        let can_verify_existing = self.has_app()
            && self.runner.is_some()
            && !specs.is_empty()
            && self.permanent_provider_error.is_none();
        if !ok && !timed_out && !can_verify_existing {
            self.mark("implementation_failed", &node_id, Some(&tail(&text, 500)));
            self.impl_failed.push(node_id);
            return;
        }
        if !ok && !timed_out {
            self.log(format!(
                "[flow] {node_id}: generation did not complete; testing the existing app"
            ));
            self.pending_corrections.push(
                "The implementation turn did not complete. Judge the existing files using acceptance results; preserve working behavior and repair only failures supported by those results.".into());
        }
        if timed_out {
            // The files written so far stay on disk; let the acceptance loop judge them.
            self.log(format!(
                "[flow] {node_id}: implement turn hit its {}s cap; testing what exists",
                implement_timeout.as_secs()
            ));
            self.close_driver();
            let correction = self.correction("implement_timed_out", &[]);
            self.pending_corrections.push(correction);
        }
        if inline_design {
            let written = self
                .output_dir
                .join(".arc/design")
                .join(format!("{node_id}.json"));
            let parsed = std::fs::read_to_string(&written)
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .filter(Value::is_object);
            match parsed {
                Some(design) => {
                    self.save_design(&node_id, &design);
                    self.mark(
                        "design_done",
                        &node_id,
                        Some(&format!(
                            "design JSON written inline to .arc/design/{node_id}.json"
                        )),
                    );
                }
                None => self.mark(
                    "design_done",
                    &node_id,
                    Some("design folded into the implementation turn (no JSON file)"),
                ),
            }
        }
        let done_message = if ok {
            tail(&text, 500)
        } else {
            "implementation incomplete; existing code awaiting acceptance".to_string()
        };
        self.mark("implementation_done", &node_id, Some(&done_message));
        let name = node.get("name").and_then(Value::as_str).unwrap_or("");
        self.commit(&format!("{node_id} (implement): {name}"));

        let rebuild = Rebuild {
            codegen_prompt,
            tool_prompt,
        };
        let verdict = self.acceptance_loop(&node_id, &specs, deadline, Some(&rebuild));
        self.test_verdict.insert(node_id.clone(), verdict);
        match verdict {
            Some(true) => {
                let message = format!("{} acceptance spec file(s) pass locally", specs.len());
                self.mark("test_passed", &node_id, Some(&message));
            }
            Some(false) => self.mark(
                "test_failed",
                &node_id,
                Some("acceptance specs still failing after repair rounds"),
            ),
            None => {}
        }
    }

    /// Evolution probe: run each candidate node's specs against the existing
    /// app (no model); nodes that fully pass need no implementation turn.
    fn already_passing_nodes(&mut self, candidates: &[String]) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for node_id in candidates {
            let specs: Vec<String> = self.spec_map.specs_for(node_id).to_vec();
            if specs.is_empty() {
                continue;
            }
            let summary = self.run_specs(&specs, None, false);
            self.probe_count += 1;
            if let Some(error) = &summary.error {
                self.log(format!(
                    "[acceptance] probe {node_id}: existing app does not build/start/serve ({})",
                    head(error, 160)
                ));
                continue;
            }
            if summary.total == 0 {
                continue;
            }
            self.log(format!(
                "[acceptance] probe {node_id}: {}/{} against the existing app",
                summary.passed, summary.total
            ));
            if summary.all_passed() {
                out.insert(node_id.clone());
                self.probe_summaries.insert(node_id.clone(), summary);
            }
        }
        out
    }

    /// Evolution: unchanged node — carry the design/impl over, re-run its specs.
    fn regression_cycle(&mut self, node: &Value) {
        let node_id = tree::node_id(node);
        let specs: Vec<String> = self.spec_map.specs_for(&node_id).to_vec();
        self.mark("design_started", &node_id, None);
        self.mark(
            "design_done",
            &node_id,
            Some("unchanged since the previous requirement version; carried over"),
        );
        self.mark("implementation_started", &node_id, None);
        self.mark(
            "implementation_done",
            &node_id,
            Some("carried over from the template application"),
        );
        let mut verdict = None;
        if self.runner.is_some() && !specs.is_empty() {
            let summary = match self.probe_summaries.remove(&node_id) {
                Some(summary) => summary,
                None => self.run_specs(&specs, None, false),
            };
            if let Some(error) = &summary.error {
                self.log(format!(
                    "[acceptance] regression {node_id} infrastructure error: {}",
                    head(error, 300)
                ));
            } else {
                self.record_tests(&node_id, &summary);
                verdict = Some(summary.all_passed());
                self.log(format!(
                    "[acceptance] regression {node_id}: {}/{}",
                    summary.passed, summary.total
                ));
                if verdict == Some(false) {
                    let seconds = budget::node_budget_seconds(
                        self.policy.budget.node_time_budget_seconds,
                        self.policy.budget.node_time_floor_seconds,
                        self.remaining(),
                        2,
                    );
                    let deadline = Instant::now() + Duration::from_secs_f64(seconds.max(1.0));
                    let correction = self.correction("evolution_regression", &[]);
                    self.pending_corrections.push(correction);
                    verdict = self.acceptance_loop(&node_id, &specs, deadline, None);
                }
            }
        }
        self.test_verdict.insert(node_id.clone(), verdict);
        match verdict {
            Some(true) => self.mark(
                "test_passed",
                &node_id,
                Some("regression specs pass locally"),
            ),
            Some(false) => self.mark(
                "test_failed",
                &node_id,
                Some("regression specs fail after repair rounds"),
            ),
            None => {}
        }
    }

    /// Run EVERY spec file together against one server with the configured workers.
    /// Per-node runs cannot see cross-node interference through shared server
    /// state; this pass can, and it repairs the nodes whose tests fail.
    fn checkpoint_specs(&self) -> Vec<String> {
        let mut specs: Vec<String> = self
            .test_verdict
            .iter()
            .filter(|(node, verdict)| {
                **verdict == Some(true) || self.checkpoint_regressions.contains(*node)
            })
            .flat_map(|(node, _)| self.spec_map.specs_for(node))
            .cloned()
            .collect();
        specs.sort();
        specs.dedup();
        specs
    }

    fn record_checkpoint(&mut self, index: usize, summary: &RunSummary) {
        if summary.error.is_some() || summary.killed {
            self.log(format!(
                "[acceptance] checkpoint {index}: no reliable verdict; {:?}",
                summary.error
            ));
            return;
        }
        let mut verified = self.spec_map.clone();
        verified.by_node.retain(|node, _| {
            self.test_verdict.get(node) == Some(&Some(true))
                || self.checkpoint_regressions.contains(node)
        });
        let grouped = acceptance::nodes_for_failures(&summary.results, &verified);
        self.log(format!(
            "[acceptance] checkpoint {index}: {}/{}; regressed nodes {:?}",
            summary.passed,
            summary.total,
            grouped.keys().collect::<Vec<_>>()
        ));
        for node in grouped.keys().flatten() {
            if verified.by_node.contains_key(node) {
                self.checkpoint_regressions.insert(node.clone());
                self.test_verdict.insert(node.clone(), Some(false));
                self.mark(
                    "test_failed",
                    node,
                    Some("previously passing behavior failed a regression checkpoint"),
                );
            }
        }
        let recovered: Vec<String> = self
            .checkpoint_regressions
            .iter()
            .filter(|node| {
                let paths = verified.specs_for(node);
                !paths.is_empty()
                    && paths.iter().all(|path| {
                        let rows: Vec<_> = summary
                            .results
                            .iter()
                            .filter(|r| {
                                Path::new(&r.file).file_name() == Path::new(path).file_name()
                            })
                            .collect();
                        !rows.is_empty() && rows.iter().all(|r| r.ok)
                    })
            })
            .cloned()
            .collect();
        for node in recovered {
            self.checkpoint_regressions.remove(&node);
            self.test_verdict.insert(node.clone(), Some(true));
            self.mark(
                "test_passed",
                &node,
                Some("previously regressed behavior passed its checkpoint specs"),
            );
        }
        if !grouped.is_empty() {
            self.pending_corrections.push(format!(
                "Previously passing behavior failed when checked together after recent changes. Repair the observed failures while preserving other working behavior. Tests ran together against one server; use this evidence when implementing the next node.\n{}",
                head(&self.failures_of(summary), 8000)));
        }
    }

    fn regression_checkpoint(&mut self, index: usize, total: usize) {
        if !regression_checkpoint_due(
            index,
            total,
            self.policy.acceptance.regression_checkpoint_nodes,
        ) || self.runner.is_none()
            || self.tests_dir.is_none()
            || self.remaining() < self.policy.budget.min_repair_seconds as f64
        {
            return;
        }
        let specs = self.checkpoint_specs();
        if specs.len() < 2 {
            return;
        }
        let workers = acceptance::workers_for_memory(
            self.mem_limit,
            self.policy.acceptance.final_workers,
            self.policy.acceptance.final_memory_per_worker_mib,
        );
        let summary = self.run_specs(&specs, Some(workers), true);
        self.record_checkpoint(index, &summary);
    }

    fn final_acceptance(&mut self) {
        if self.runner.is_none() || self.tests_dir.is_none() {
            return;
        }
        let all_specs = self.all_specs.clone();
        let mut unverified: Vec<String> = self
            .test_verdict
            .iter()
            .filter(|(_, v)| **v != Some(true))
            .map(|(n, _)| n.clone())
            .collect();
        if unverified.is_empty() {
            unverified = self
                .node_ids
                .iter()
                .filter(|n| {
                    !self.spec_map.specs_for(n).is_empty() && !self.test_verdict.contains_key(*n)
                })
                .cloned()
                .collect();
        }
        if all_specs.len() < 2 && unverified.is_empty() {
            return; // single spec already judged by the node run
        }
        let rounds = if self.degraded {
            0
        } else {
            self.policy.repair.final_rounds
        };
        let workers = acceptance::workers_for_memory(
            self.mem_limit,
            self.policy.acceptance.final_workers,
            self.policy.acceptance.final_memory_per_worker_mib,
        );
        let mut previous_failing = None;
        // Best full-suite state seen so far: (passed, commit, results). A repair
        // turn that loses tests is rolled back to it when the loop ends, exactly
        // like the per-node loop keeps its best snapshot.
        let mut best: Option<(usize, String, Vec<acceptance::TestOutcome>)> = None;
        let mut last_passed: Option<usize> = None;
        for attempt in 0..=rounds {
            let summary = self.run_specs(&all_specs, Some(workers), true);
            if summary.error.is_some() && summary.killed {
                // The runner was OOM-killed under the cgroup; repair rounds on a non-failure would be wasted.
                self.log(format!(
                    "[acceptance] full suite could not run ({}); keeping per-node verdicts",
                    head(summary.error.as_deref().unwrap_or(""), 120)
                ));
                return;
            }
            let (grouped, failures, summary) = if let Some(error) = summary.error.clone() {
                // The app does not even start the way the grader starts it: every node fails.
                self.log(format!(
                    "[acceptance] full suite (grader-like start) failed: {}",
                    head(&error, 300)
                ));
                for node_id in self.node_ids.clone() {
                    self.test_verdict.insert(node_id, Some(false));
                }
                let mut grouped: BTreeMap<Option<String>, Vec<acceptance::TestOutcome>> =
                    BTreeMap::new();
                grouped.insert(None, Vec::new());
                let digest = self.startup_failure_digest(&error, true);
                (
                    grouped,
                    digest,
                    RunSummary {
                        passed: 0,
                        total: all_specs.len(),
                        ..Default::default()
                    },
                )
            } else {
                let grouped = acceptance::nodes_for_failures(&summary.results, &self.spec_map);
                let flat: Vec<acceptance::TestOutcome> =
                    grouped.values().flatten().cloned().collect();
                let failures = self.failures_of(&RunSummary::from_results(flat));
                (grouped, failures, summary)
            };
            let failing_nodes: Vec<String> = grouped.keys().flatten().cloned().collect();
            let failing_text = if !failing_nodes.is_empty() {
                format!("{failing_nodes:?}")
            } else if grouped.contains_key(&None) && summary.results.is_empty() {
                "all".to_string()
            } else {
                "[]".to_string()
            };
            self.log(format!(
                "[acceptance] full suite round {attempt}: {}/{}; failing nodes {failing_text}",
                summary.passed, summary.total
            ));
            if !summary.results.is_empty() {
                for node_id in self.node_ids.clone() {
                    let specs = self.spec_map.specs_for(&node_id).to_vec();
                    if specs.is_empty() {
                        continue;
                    }
                    let names: BTreeSet<String> = specs
                        .iter()
                        .filter_map(|p| {
                            Path::new(p)
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                        })
                        .collect();
                    let subset: Vec<acceptance::TestOutcome> = summary
                        .results
                        .iter()
                        .filter(|r| names.contains(&r.file))
                        .cloned()
                        .collect();
                    self.record_tests(&node_id, &RunSummary::from_results(subset));
                    self.test_verdict.insert(
                        node_id.clone(),
                        Some(!grouped.contains_key(&Some(node_id.clone()))),
                    );
                }
            }
            if grouped.is_empty() {
                self.commit(&format!(
                    "chore: full acceptance suite {}/{} pass (full suite)",
                    summary.passed, summary.total
                ));
                return;
            }
            last_passed = Some(summary.passed);
            if !summary.results.is_empty()
                && best
                    .as_ref()
                    .is_none_or(|(passed, _, _)| summary.passed > *passed)
            {
                self.commit(&format!(
                    "chore: full acceptance suite {}/{} (best so far)",
                    summary.passed, summary.total
                ));
                if let Some(sha) = self.git.head() {
                    best = Some((summary.passed, sha, summary.results.clone()));
                }
            }
            self.log_failure_lines(&failures);
            let failing_evidence = acceptance::failure_signature(&RunSummary::from_results(
                grouped.values().flatten().cloned().collect(),
            ));
            if previous_failing.as_ref() == Some(&failing_evidence) {
                self.log("[acceptance] full suite: same failures as the previous round; stopping repairs");
                break;
            }
            previous_failing = Some(failing_evidence);
            if attempt == rounds || self.remaining() < 240.0 {
                break;
            }
            let failing = if failing_nodes.is_empty() {
                "all nodes".to_string()
            } else {
                failing_nodes.join(", ")
            };
            let parallel = format!("{}\n", self.correction("parallel_suite", &[]));
            let prompt = self.repair_prompt(
                &failing,
                (summary.passed, summary.total),
                &failures,
                &parallel,
                "",
                &[],
            );
            let timeout = Duration::from_secs_f64(
                (self.policy.budget.node_timeout_seconds as f64)
                    .min((self.remaining() - 200.0).max(120.0)),
            );
            self.turn(
                &prompt,
                timeout,
                &format!("full-suite repair {}/{rounds}", attempt + 1),
                true,
                None,
            );
            self.commit(&format!("fix: full-suite repair {}", attempt + 1));
        }
        if let (Some((best_passed, sha, results)), Some(last)) = (best, last_passed)
            && last < best_passed
        {
            self.log(format!(
                "[acceptance] full suite: last repair left {last} passing, best was {best_passed}; restoring the best state"
            ));
            self.restore_app(&sha);
            self.events.emit(
                "full_suite_restored",
                json!({"best": best_passed, "last": last, "sha": sha}),
            );
            let best_summary = RunSummary::from_results(results);
            let grouped = acceptance::nodes_for_failures(&best_summary.results, &self.spec_map);
            for node_id in self.node_ids.clone() {
                let specs = self.spec_map.specs_for(&node_id).to_vec();
                if specs.is_empty() {
                    continue;
                }
                let names: BTreeSet<String> = specs
                    .iter()
                    .filter_map(|p| {
                        Path::new(p)
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                    })
                    .collect();
                let subset: Vec<acceptance::TestOutcome> = best_summary
                    .results
                    .iter()
                    .filter(|r| names.contains(&r.file))
                    .cloned()
                    .collect();
                self.record_tests(&node_id, &RunSummary::from_results(subset));
                self.test_verdict.insert(
                    node_id.clone(),
                    Some(!grouped.contains_key(&Some(node_id.clone()))),
                );
            }
        }
    }

    /// Build and start exactly like the grader (only PORT set); a failure goes
    /// to a repair turn, at most three attempts.
    fn rehearsal(&mut self) -> bool {
        for attempt in 1..=3 {
            self.log(format!(
                "[rehearsal] startup rehearsal {attempt}/3 (smoke port {}, grader-like env)",
                self.smoke_port
            ));
            let mut server = self.app_server(true);
            let error = server.build().or_else(|| server.start());
            server.stop();
            let Some(error) = error else {
                self.log("[rehearsal] app builds and starts cleanly");
                return true;
            };
            self.log(format!(
                "[rehearsal] FAILED: {}",
                head(error.lines().next().unwrap_or(""), 200)
            ));
            if attempt == 3 || self.remaining() < -600.0 || self.degraded {
                self.log("[rehearsal] giving up; submitting as-is");
                return false;
            }
            let prompt = self
                .prompts
                .render(
                    "rehearsal-repair",
                    &[
                        ("error", &tail(&error, 1200)),
                        ("port", &self.web_port.to_string()),
                        ("smoke", &self.smoke_port.to_string()),
                    ],
                )
                .unwrap_or_default();
            self.turn(
                &prompt,
                Duration::from_secs(self.policy.budget.node_timeout_seconds),
                &format!("rehearsal repair {attempt}"),
                true,
                None,
            );
            self.commit("fix: startup rehearsal repair");
        }
        false
    }

    /// Skeleton turn for large trees: frontend/ and backend/ with a home page
    /// and a health endpoint; nudges when the model only planned.
    fn skeleton(&mut self) -> Result<()> {
        self.log("[flow] skeleton turn starting");
        let architecture = self.architecture_contract();
        let port_rules = self.port_rules();
        let prompt = self.prompts.render(
            "skeleton",
            &[
                ("req_dir", &self.req_dir.to_string_lossy()),
                ("port", &self.web_port.to_string()),
                ("smoke", &self.smoke_port.to_string()),
                ("tests", &self.tests_prompt_for(None, true)),
                ("architecture_contract", &architecture),
                ("port_rules", &port_rules),
            ],
        )?;
        let node_timeout = Duration::from_secs(self.policy.budget.node_timeout_seconds);
        for attempt in 1..=4 {
            if self.time_up() {
                bail!("time budget exhausted before the skeleton existed");
            }
            let (ok, _) = self.turn(
                &prompt,
                node_timeout,
                &format!("skeleton attempt {attempt}"),
                true,
                None,
            );
            if ok && !self.has_app() {
                self.log("[flow] skeleton turn wrote no frontend/backend; nudging");
                let nudge = self.prompts.get("nudge").to_string();
                for n in 1..=2 {
                    self.turn(
                        &nudge,
                        Duration::from_secs(600),
                        &format!("nudge {n}/2"),
                        true,
                        None,
                    );
                    if self.has_app() {
                        break;
                    }
                }
            }
            if self.has_app() {
                self.commit("chore: scaffold web application skeleton");
                return Ok(());
            }
            if self.dry_run {
                break;
            }
            std::thread::sleep(Duration::from_secs(30));
        }
        bail!("skeleton scaffolding failed: no frontend/ and backend/ after 4 attempts")
    }

    /// The platform counts FOLDER nodes as requirements too; derive their
    /// state from their atomic descendants.
    fn mark_folders(&mut self) {
        let folders = self.folder_children.clone();
        for (folder_id, leaves) in folders {
            if leaves.is_empty() {
                continue;
            }
            let verdicts: Vec<Option<bool>> = leaves
                .iter()
                .map(|l| self.test_verdict.get(l).copied().flatten())
                .collect();
            self.events
                .requirement_state(&folder_id, "design", "running", None, &[]);
            self.events.requirement_state(
                &folder_id,
                "design",
                "completed",
                Some(&format!("{} atomic children designed", leaves.len())),
                &[],
            );
            self.events
                .requirement_state(&folder_id, "implement", "running", None, &[]);
            let any_failed = leaves.iter().any(|l| self.impl_failed.contains(l));
            if verdicts.iter().all(|v| v.is_some()) || any_failed {
                let done: Vec<&String> = leaves
                    .iter()
                    .filter(|l| !self.impl_failed.contains(l))
                    .collect();
                if done.is_empty() {
                    self.events.requirement_state(
                        &folder_id,
                        "implement",
                        "failed",
                        Some("no atomic child implemented"),
                        &[],
                    );
                } else {
                    self.events.requirement_state(
                        &folder_id,
                        "implement",
                        "completed",
                        Some(&format!(
                            "{}/{} atomic children implemented",
                            done.len(),
                            leaves.len()
                        )),
                        &[],
                    );
                }
            }
            if verdicts.iter().all(|v| *v == Some(true)) {
                self.events.requirement_state(
                    &folder_id,
                    "test",
                    "passed",
                    Some(&format!("all {} atomic children pass", leaves.len())),
                    &[],
                );
            } else {
                let failing: Vec<&str> = leaves
                    .iter()
                    .zip(&verdicts)
                    .filter(|(_, v)| **v != Some(true))
                    .map(|(l, _)| l.as_str())
                    .collect();
                self.events.requirement_state(
                    &folder_id,
                    "test",
                    "failed",
                    Some(&format!("children not verified: {}", failing.join(", "))),
                    &[],
                );
            }
        }
    }

    // -- run --------------------------------------------------------------

    pub fn run(&mut self) -> RunOutcome {
        self.events.emit(
            "run_started",
            json!({"nodes": self.node_ids, "time_budget_seconds": self.plan.time_budget_seconds, "codegen": self.plan.codegen,
                "evolution": self.plan.evolution, "web_port": self.web_port, "smoke_port": self.smoke_port,
                "reasoning": self.plan.base_reasoning.label(), "tests_dir": self.tests_dir, "dry_run": self.dry_run}),
        );
        let outcome = self.run_inner();
        self.close_driver();
        match outcome {
            Ok(()) => {
                for node_id in self.node_ids.clone() {
                    match self.test_verdict.get(&node_id).copied().flatten() {
                        Some(true) => self.mark(
                            "test_passed",
                            &node_id,
                            Some("acceptance specs pass (node run and full parallel suite)"),
                        ),
                        Some(false) => {
                            self.mark("test_failed", &node_id, Some("acceptance specs failing"))
                        }
                        None => {}
                    }
                }
                self.mark_folders();
                let failed: Vec<String> = self
                    .node_ids
                    .iter()
                    .filter(|n| self.test_verdict.get(*n).copied().flatten() != Some(true))
                    .cloned()
                    .collect();
                let message = if failed.is_empty() {
                    "all requirement nodes implemented and verified".to_string()
                } else {
                    format!("completed; nodes not verified: {}", failed.join(", "))
                };
                self.finish("run_completed", &message);
                RunOutcome {
                    failed_nodes: failed,
                    aborted: None,
                }
            }
            Err(error) => {
                let text = format!("{error:#}");
                self.log(format!("[flow] aborted: {text}"));
                // Like `main.Flow.run`'s except path: undecided nodes get a test_failed
                // mark but no local verdict, so folder rows derive from what was decided.
                for node_id in self.node_ids.clone() {
                    if !self.test_verdict.contains_key(&node_id) {
                        self.mark(
                            "test_failed",
                            &node_id,
                            Some(&format!("run aborted: {}", head(&text, 200))),
                        );
                    }
                }
                self.mark_folders();
                self.finish("run_failed", &head(&text, 1000));
                RunOutcome {
                    failed_nodes: self.node_ids.clone(),
                    aborted: Some(text),
                }
            }
        }
    }

    fn finish(&mut self, kind: &str, message: &str) {
        if let Some(mut dog) = self.watchdog.take() {
            dog.stop();
        }
        for line in crate::reap::report() {
            self.log(format!("[reap:postflight] {line}"));
        }
        let killed = crate::reap::sweep_all(&self.output_dir);
        self.log(format!(
            "[reap:postflight] {}",
            if killed.is_empty() {
                "nothing to kill".to_string()
            } else {
                format!("killed {killed:?}")
            }
        ));
        self.cleanup_playwright();
        let totals = self
            .ledger
            .lock()
            .map(|l| l.totals())
            .unwrap_or_else(|_| json!({}));
        self.log(format!("[usage] provider totals: {totals}"));
        self.events.emit("usage_total", totals);
        self.events.emit(
            kind,
            json!({"message": message, "elapsed_s": self.budget.elapsed().as_secs()}),
        );
    }

    fn run_inner(&mut self) -> Result<()> {
        self.check_provider()?;
        let ids = self.node_ids.clone();
        self.log(format!(
            "[flow] {} atomic nodes in dependency order: {ids:?}; time budget {}s",
            ids.len(),
            self.plan.time_budget_seconds
        ));
        self.log(format!(
            "[guard] cost guard: {} tokens / {} turns{}",
            self.guard_tokens,
            self.guard_turns,
            if self.guard_abs > 0 {
                format!(" / absolute {}", self.guard_abs)
            } else {
                String::new()
            }
        ));
        if self.plan.evolution {
            let to_implement: Vec<&String> = ids
                .iter()
                .filter(|i| !self.unchanged.contains(*i))
                .collect();
            self.log(format!("[flow] evolution mode: existing app detected; unchanged nodes {:?}, to implement {to_implement:?}", self.unchanged));
        }
        match self.tests_dir.clone() {
            Some(dir) => {
                let mapping: BTreeMap<&String, &Vec<String>> = self
                    .spec_map
                    .by_node
                    .iter()
                    .filter(|(_, v)| !v.is_empty())
                    .collect();
                self.log(format!(
                    "[tests] {} spec files at {}; mapping {mapping:?}; aliases {:?}",
                    self.all_specs.len(),
                    dir.display(),
                    self.spec_map.aliases
                ));
            }
            None => {
                self.log("[tests] no acceptance specs found; building from requirement text only")
            }
        }
        self.git.ensure_repo()?;
        self.setup_playwright();
        self.watchdog = Some(crate::reap::PortWatchdog::start(
            self.web_port,
            &self.output_dir,
            Duration::from_secs(5),
        ));
        if self.plan.evolution && self.runner.is_some() {
            // The platform's template app carries no traceability records, so fingerprints cannot tell
            // what is new. A node whose specs already pass against the existing app is unchanged.
            let candidates: Vec<String> = ids
                .iter()
                .filter(|i| !self.unchanged.contains(*i))
                .cloned()
                .collect();
            let passing = self.already_passing_nodes(&candidates);
            self.unchanged.extend(passing);
            let to_implement: Vec<String> = ids
                .iter()
                .filter(|i| !self.unchanged.contains(*i))
                .cloned()
                .collect();
            let policy = self.policy.clone();
            if self.probe_count > 0 && self.unchanged.is_empty() {
                // Nothing of the existing app satisfies any spec (a scaffold/placeholder
                // template, or an app the new specs no longer accept): it is not a usable
                // base. Set it aside and build the task fresh (cloud c30b29eab45b/10b04d36f704:
                // implement-then-rewrite on a placeholder cost 40-80x the fresh build).
                self.discard_template();
                self.plan = RunPlan::new(
                    &policy,
                    &self.tree,
                    self.plan.n_nodes,
                    to_implement.len(),
                    false,
                )?;
            } else {
                self.plan
                    .set_nodes_to_implement(&policy, to_implement.len())?;
            }
            self.log(format!(
                "[flow] {} after probing the existing app: unchanged {:?}, to implement {to_implement:?}",
                if self.plan.evolution { "evolution mode" } else { "fresh build" },
                self.unchanged
            ));
            if self.plan.wants_skeleton {
                self.skeleton()?;
                self.check_provider()?;
                self.end_scope("node");
            }
        }
        // Probe policy (round 34): none in dry runs; none when the whole task is tiny-tier (the
        // first real request doubles as the probe); otherwise the token-free GET /models probe.
        if self.dry_run {
            self.log("[probe] skipped (dry run)");
        } else if self.all_specs_tiny() {
            self.log(
                "[probe] skipped (tiny-tier task: the first real request doubles as the probe)",
            );
        } else {
            let patience = Duration::from_secs(self.policy.reasoning.probe_patience_seconds);
            for line in self.llm.probe(patience) {
                self.log(line);
            }
        }
        let mut protected_dirs: Vec<PathBuf> = Vec::new();
        if let Some(tests) = &self.tests_dir {
            protected_dirs.push(tests.clone());
        }
        if self.req_dir.is_dir() {
            protected_dirs.push(self.req_dir.clone());
        }
        match ProtectedTrees::snapshot(&protected_dirs) {
            Ok(trees) => self.protected = Some(trees),
            Err(error) => self.log(format!(
                "[guard] could not snapshot the protected directories: {error}"
            )),
        }
        if self.plan.wants_skeleton {
            self.skeleton()?;
            self.check_provider()?;
            self.end_scope("node");
        } else if !self.plan.evolution && self.plan.codegen {
            self.log(format!(
                "[flow] {}-node tree: codegen mode, harness manifests replace the skeleton turn",
                ids.len()
            ));
        } else if !self.plan.evolution {
            self.log(format!(
                "[flow] {}-node tree: skeleton folded into the first node turn",
                ids.len()
            ));
        }
        let total = self.ordered.len();
        let ordered = self.ordered.clone();
        for (index, node) in ordered.iter().enumerate() {
            self.check_provider()?;
            let node_id = tree::node_id(node);
            if self.time_up() {
                self.log(format!("[flow] time budget exhausted; skipping {node_id}"));
                self.mark("implementation_started", &node_id, None);
                self.mark(
                    "implementation_failed",
                    &node_id,
                    Some("skipped: time budget exhausted"),
                );
                self.impl_failed.push(node_id);
                continue;
            }
            if self.unchanged.contains(&node_id) {
                self.regression_cycle(node);
            } else {
                self.node_cycle(node, index + 1, total);
            }
            self.check_provider()?;
            self.regression_checkpoint(index + 1, total);
            self.end_scope("node");
        }
        if !self.time_up() {
            self.final_acceptance();
            self.check_provider()?;
            self.end_scope("node");
        }
        let undecided: Vec<String> = ids
            .iter()
            .filter(|i| {
                self.test_verdict.get(*i).copied().flatten().is_none()
                    && !self.impl_failed.contains(*i)
            })
            .cloned()
            .collect();
        let mut final_ok: Option<bool> = None;
        if !undecided.is_empty() && !self.time_up() {
            self.log(format!(
                "[flow] final check turn for nodes without a local verdict: {undecided:?}"
            ));
            let prompt = self.prompts.render(
                "final-check",
                &[
                    ("smoke", &self.smoke_port.to_string()),
                    ("port", &self.web_port.to_string()),
                    ("tests", &self.tests_prompt_for(None, false)),
                    ("performance", &self.perf_text()),
                    ("ui", &self.ui_contract()),
                    ("port_rules", &self.port_rules()),
                ],
            )?;
            let (ok, _) = self.turn(
                &prompt,
                Duration::from_secs(self.policy.budget.node_timeout_seconds),
                "final check",
                true,
                None,
            );
            self.check_provider()?;
            final_ok = Some(ok);
            self.commit("chore: final verification pass");
        }
        let rehearsed = self.rehearsal();
        for node_id in undecided {
            if rehearsed && final_ok != Some(false) {
                self.mark(
                    "test_passed",
                    &node_id,
                    Some("final check and startup rehearsal passed"),
                );
                self.test_verdict.insert(node_id, Some(true));
            } else {
                self.mark(
                    "test_failed",
                    &node_id,
                    Some("final check or startup rehearsal failed"),
                );
                self.test_verdict.insert(node_id, Some(false));
            }
        }
        let _ = &self.tree;
        Ok(())
    }
}

pub fn has_app(output_dir: &Path) -> bool {
    output_dir.join("frontend/package.json").is_file()
        && output_dir.join("backend/package.json").is_file()
}

/// The previous run's requirement table (committed with the template). The glue
/// copies it aside (possibly empty) before the platform runtime stores the new
/// tree over the traceability file; when a snapshot path is given it is the only
/// source, because by then the traceability file already holds the new tree.
fn previous_requirement_records(
    output_dir: &Path,
    snapshot: Option<&Path>,
) -> BTreeMap<String, Value> {
    let path = match snapshot {
        Some(copy) => copy.to_path_buf(),
        None => output_dir.join(".arc/traceability/requirements.json"),
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&text) else {
        return BTreeMap::new();
    };
    map.into_iter().filter(|(_, v)| v.is_object()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn should_bound_regression_checkpoint_gaps() {
        assert_eq!(
            (1..33)
                .filter(|n| regression_checkpoint_due(*n, 32, 4))
                .collect::<Vec<_>>(),
            vec![4, 8, 16, 24]
        );
        assert_eq!(
            (1..20)
                .filter(|n| regression_checkpoint_due(*n, 20, 3))
                .collect::<Vec<_>>(),
            vec![3, 6, 12, 18]
        );
        assert!(!regression_checkpoint_due(4, 20, 0));
        let mut points = vec![0];
        points.extend((1..121).filter(|n| regression_checkpoint_due(*n, 121, 4)));
        points.push(121);
        assert!(points.windows(2).all(|pair| pair[1] - pair[0] <= 8));
    }

    struct RejectedProvider(Arc<AtomicUsize>, &'static str);
    impl Completer for RejectedProvider {
        fn complete(&mut self, _: &CompletionRequest<'_>) -> Result<crate::llm::Completion> {
            self.0.fetch_add(1, Ordering::SeqCst);
            bail!(self.1)
        }
    }

    fn rejected_flow(error: &'static str) -> (Flow, Arc<AtomicUsize>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let spec: crate::run::RunnerSpec = serde_json::from_value(json!({
            "requirement_path": dir.path(), "output_dir": dir.path(), "web_port": 43219,
            "model": {"model": "test-model", "base_url": "http://127.0.0.1:1/v1"}
        }))
        .unwrap();
        let arc_dir = dir.path().join(".arc");
        let flow = Flow::new(
            &spec,
            FlowInputs {
                policy: Policy::default(),
                prompts: Prompts::builtin(),
                tree: json!({"id":"generic-node", "type":"ATOMIC", "description":"Build an app"}),
                llm: Box::new(RejectedProvider(calls.clone(), error)),
                ledger: crate::llm::UsageLedger::shared(&arc_dir, "test-model", "openai"),
                events: Events::open(&arc_dir).unwrap().quiet(),
                executable: dir.path().join("must-not-start"),
                dry_run: false,
                routing: None,
            },
        )
        .unwrap();
        (flow, calls, dir)
    }

    #[test]
    fn checkpoints_only_recheck_verified_specs_and_ignore_infrastructure_failures() {
        let (mut flow, _, _dir) = rejected_flow("unused");
        for (node, verdict) in [
            ("old", Some(true)),
            ("new", Some(true)),
            ("future", None),
            ("broken", Some(false)),
        ] {
            flow.test_verdict.insert(node.into(), verdict);
            flow.spec_map
                .by_node
                .insert(node.into(), vec![format!("{node}.spec.ts")]);
        }
        assert_eq!(flow.checkpoint_specs(), vec!["new.spec.ts", "old.spec.ts"]);
        let mut summary = RunSummary {
            passed: 1,
            total: 2,
            results: vec![acceptance::TestOutcome {
                file: "old.spec.ts".into(),
                title: "old behavior".into(),
                message: "handler undefined".into(),
                status: "failed".into(),
                ..Default::default()
            }],
            error: Some("runner unavailable".into()),
            ..Default::default()
        };
        flow.record_checkpoint(4, &summary);
        assert_eq!(flow.test_verdict.get("old"), Some(&Some(true)));
        assert!(flow.pending_corrections.is_empty());
        summary.error = None;
        summary.killed = true;
        flow.record_checkpoint(4, &summary);
        assert_eq!(flow.test_verdict.get("old"), Some(&Some(true)));
        summary.killed = false;
        flow.record_checkpoint(4, &summary);
        assert_eq!(flow.test_verdict.get("old"), Some(&Some(false)));
        assert_eq!(flow.test_verdict.get("new"), Some(&Some(true)));
        assert_eq!(flow.test_verdict.get("future"), Some(&None));
        assert!(flow.corrections_text().contains("handler undefined"));
        assert!(flow.checkpoint_specs().contains(&"old.spec.ts".to_string()));
        summary.results.clear();
        flow.record_checkpoint(8, &summary);
        assert_eq!(flow.test_verdict.get("old"), Some(&Some(false)));
        summary.results.push(acceptance::TestOutcome {
            file: "old.spec.ts".into(),
            ok: true,
            status: "passed".into(),
            ..Default::default()
        });
        flow.record_checkpoint(16, &summary);
        assert_eq!(flow.test_verdict.get("old"), Some(&Some(true)));
        assert!(
            !flow
                .checkpoint_specs()
                .contains(&"broken.spec.ts".to_string())
        );
    }

    #[test]
    fn codegen_requires_existing_sources_to_fit() {
        let (flow, _, _dir) = rejected_flow("unused");
        assert!(flow.codegen_context_fits(12000));
        let backend = flow.output_dir.join("backend");
        let frontend = flow.output_dir.join("frontend");
        std::fs::create_dir_all(&backend).unwrap();
        std::fs::create_dir_all(&frontend).unwrap();
        std::fs::write(backend.join("server.js"), "b".repeat(26000)).unwrap();
        std::fs::write(frontend.join("index.html"), "p".repeat(55000)).unwrap();
        assert!(!flow.codegen_context_fits(12000));
        std::fs::write(frontend.join("index.html"), "p".repeat(50000)).unwrap();
        assert!(flow.codegen_context_fits(12000));
    }

    #[test]
    fn codegen_repairs_need_complete_acceptance_evidence_within_budget() {
        let (mut flow, _, dir) = rejected_flow("unused");
        let tests = dir.path().join("tests");
        std::fs::create_dir_all(&tests).unwrap();
        std::fs::write(tests.join("feature.spec.ts"), "complete acceptance example").unwrap();
        std::fs::write(tests.join("helpers.ts"), "complete shared helper").unwrap();
        flow.tests_dir = Some(tests);
        flow.spec_map
            .by_node
            .insert("feature".into(), vec!["feature.spec.ts".into()]);
        let prompt = flow
            .codegen_repair_prompt("feature", "failure and sources")
            .unwrap();
        assert!(prompt.contains("complete acceptance example"));
        assert!(prompt.contains("complete shared helper"));
        flow.policy.prompts.codegen_context_chars = 10;
        assert!(
            flow.codegen_repair_prompt("feature", "failure and sources")
                .is_none()
        );
        flow.tests_dir = None;
        assert!(
            flow.codegen_repair_prompt("feature", "failure and sources")
                .is_none()
        );
    }

    #[test]
    fn repair_uses_spare_context_for_omitted_source() {
        let (mut flow, _, dir) = rejected_flow("unused");
        let frontend = flow.output_dir.join("frontend");
        std::fs::create_dir_all(&frontend).unwrap();
        let page = format!("unique page content {}", "x".repeat(45000));
        std::fs::write(frontend.join("index.html"), &page).unwrap();
        let tests = dir.path().join("tests");
        std::fs::create_dir_all(&tests).unwrap();
        std::fs::write(tests.join("feature.spec.ts"), "acceptance").unwrap();
        flow.tests_dir = Some(tests);
        flow.spec_map
            .by_node
            .insert("feature".into(), vec!["feature.spec.ts".into()]);
        let prompt = format!("failure evidence\n{}preserve behavior", flow.sources_text());
        let full = flow.codegen_repair_prompt("feature", &prompt).unwrap();
        assert!(full.contains(&page));
        assert!(full.contains("preserve behavior"));
        assert!(!full.contains("(omitted,"));
        std::fs::write(frontend.join("index.html"), "x".repeat(100000)).unwrap();
        let prompt = format!("failure evidence\n{}", flow.sources_text());
        assert!(flow.codegen_repair_prompt("feature", &prompt).is_none());
    }

    #[test]
    fn repairs_keep_original_requirements_beyond_test_assertions() {
        let (mut flow, _, _dir) = rejected_flow("unused");
        flow.ordered = vec![
            json!({"id":"feature", "description":"Preserve initial state beyond visible assertions"}),
            json!({"id":"other", "description":"Save the full text without truncation"}),
        ];
        let single = flow.repair_prompt(
            "feature",
            (0, 1),
            "missing control",
            "",
            "",
            &["feature.spec.ts".into()],
        );
        assert!(single.contains("Preserve initial state beyond visible assertions"));
        assert!(!single.contains("Save the full text without truncation"));
        let full = flow.repair_prompt("feature, other", (0, 2), "missing control", "", "", &[]);
        assert!(full.contains("Preserve initial state beyond visible assertions"));
        assert!(full.contains("Save the full text without truncation"));
    }

    #[test]
    fn should_preserve_verified_behavior_instead_of_rewriting_from_scratch() {
        let (mut flow, _, _dir) = rejected_flow("unused");
        flow.test_verdict.insert("new-feature".into(), Some(false));
        assert!(flow.can_rewrite_from_scratch());
        flow.test_verdict
            .insert("existing-feature".into(), Some(true));
        assert!(!flow.can_rewrite_from_scratch());
        flow.test_verdict.clear();
        flow.probe_summaries.insert(
            "template-feature".into(),
            RunSummary {
                passed: 1,
                total: 2,
                ..Default::default()
            },
        );
        assert!(!flow.can_rewrite_from_scratch());
    }

    #[test]
    fn should_run_acceptance_on_existing_app_after_no_file_reply() {
        struct NoChanges(Arc<std::sync::Mutex<Vec<String>>>);
        impl Completer for NoChanges {
            fn complete(
                &mut self,
                request: &CompletionRequest<'_>,
            ) -> Result<crate::llm::Completion> {
                self.0.lock().unwrap().push(request.user.to_string());
                Ok(crate::llm::Completion {
                    text: "Existing capability needs no changes".into(),
                    truncated: false,
                    usage: Default::default(),
                    elapsed_ms: 0,
                    attempts: 1,
                })
            }
        }
        let (mut flow, _, dir) = rejected_flow("unused");
        let prompts = Arc::new(std::sync::Mutex::new(Vec::new()));
        flow.llm = Box::new(NoChanges(prompts.clone()));
        let correction = "Restore the previously verified navigation behavior";
        flow.pending_corrections.push(correction.into());
        flow.policy.mode.tiny = true;
        flow.policy.repair.rounds = 0;
        flow.policy.repair.rounds_large_tree = 0;
        for folder in ["frontend", "backend"] {
            let path = dir.path().join(folder);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(
                path.join("package.json"),
                r#"{"scripts":{"build":"node -e \"process.exit(1)\"","start":"node missing.js"}}"#,
            )
            .unwrap();
        }
        flow.spec_map
            .by_node
            .insert("feature".into(), vec!["feature.spec.ts".into()]);
        flow.runner = Some(AcceptanceRunner {
            root: dir.path().into(),
            tests_dir: dir.path().join("tests"),
            work_dir: dir.path().join("prepared"),
            timeout_ms: 1000,
            workers: 1,
            fully_parallel: false,
            env_extra: vec![],
            wall_timeout: Duration::from_secs(5),
        });
        flow.node_cycle(
            &json!({"id":"feature", "type":"ATOMIC", "description":"Existing capability"}),
            1,
            1,
        );
        // A failed build is a real negative verdict, not an untested implementation failure.
        assert_eq!(flow.test_verdict.get("feature"), Some(&Some(false)));
        assert!(flow.impl_failed.is_empty());
        let prompts = prompts.lock().unwrap();
        assert!(!prompts.is_empty());
        for prompt in prompts.iter() {
            assert_eq!(prompt.matches(correction).count(), 1, "{prompt}");
        }
    }

    #[test]
    fn codegen_and_tool_turns_discard_pre_generation_verdicts() {
        let (mut flow, _, _dir) = rejected_flow("HTTP 402 insufficient_balance");
        flow.probe_summaries.insert(
            "unchanged".into(),
            RunSummary {
                passed: 1,
                total: 1,
                ..Default::default()
            },
        );
        flow.codegen_turn(
            "modify shared component",
            Duration::from_secs(1),
            "changed implement",
        );
        assert!(flow.probe_summaries.is_empty());
        let (mut flow, _, _dir) = rejected_flow("unused");
        flow.dry_run = true;
        flow.probe_summaries.insert(
            "unchanged".into(),
            RunSummary {
                passed: 1,
                total: 1,
                ..Default::default()
            },
        );
        flow.turn(
            "modify shared component",
            Duration::from_secs(1),
            "changed implement",
            false,
            None,
        );
        assert!(flow.probe_summaries.is_empty());
    }

    #[test]
    fn permanent_provider_failure_stops_codegen_repair_and_tool_fallback() {
        let (mut flow, calls, _dir) = rejected_flow("HTTP 402 insufficient_balance");
        let timeout = Duration::from_secs(60);
        let first = flow.codegen_turn("build", timeout, "implement");
        assert!(!first.0);
        let second = flow.codegen_turn("fix", timeout, "repair");
        assert_eq!(second, first);
        let fallback = flow.turn("build", timeout, "implement", false, None);
        assert_eq!(fallback, first);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(flow.driver.is_none());
        assert!(
            flow.run_inner()
                .unwrap_err()
                .to_string()
                .contains("insufficient_balance")
        );
    }

    #[test]
    fn temporary_provider_failure_allows_later_phase_attempts() {
        let (mut flow, calls, _dir) = rejected_flow("HTTP 503 temporarily unavailable");
        flow.codegen_turn("build", Duration::from_secs(60), "implement");
        flow.codegen_turn("fix", Duration::from_secs(60), "repair");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    #[cfg(unix)]
    fn repair_entry_executes_with_spaces_and_quotes_in_paths() {
        use std::os::unix::fs::PermissionsExt;
        let (mut flow, _, dir) = rejected_flow("unused");
        let root = dir.path().join("runner space ' quote");
        let work = root.join("prepared");
        std::fs::create_dir_all(&work).unwrap();
        let helper = root.join("verify_app.py");
        std::fs::write(&helper, "import sys; print('\\n'.join(sys.argv[1:]))").unwrap();
        flow.bundle_dir = Some(root.clone());
        let binary = root.join("node_modules/.bin/playwright");
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::write(
            &binary,
            "#!/bin/sh\nprintf '%s\\n' \"$E2E_BASE_URL\" \"$@\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        flow.tests_dir = Some(dir.path().join("original tests"));
        flow.runner = Some(AcceptanceRunner {
            root,
            tests_dir: flow.tests_dir.clone().unwrap(),
            work_dir: work,
            timeout_ms: 10000,
            workers: 1,
            fully_parallel: false,
            env_extra: vec![],
            wall_timeout: Duration::from_secs(60),
        });
        flow.spec_map
            .by_node
            .insert("node".into(), vec!["generic one's.spec.ts".into()]);
        let prompt = flow.repair_prompt(
            "node",
            (0, 1),
            "failed",
            "",
            "",
            &["generic one's.spec.ts".into()],
        );
        let command = prompt
            .split("```sh\n")
            .nth(1)
            .unwrap()
            .split("\n```")
            .next()
            .unwrap();
        let output = std::process::Command::new("sh")
            .args(["-c", command])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            vec![
                "--app".into(),
                flow.output_dir.display().to_string(),
                "--tests".into(),
                flow.tests_dir.as_ref().unwrap().display().to_string(),
                "--playwright".into(),
                flow.runner.as_ref().unwrap().root.display().to_string(),
                "--workers".into(),
                "1".into(),
                "--spec".into(),
                "generic one's.spec.ts".into()
            ]
        );
        flow.mem_limit = None;
        flow.policy.acceptance.final_workers = 4;
        let full = flow.repair_prompt("node", (0, 1), "failed", "", "", &[]);
        assert!(!full.contains("generic one's.spec.ts"));
        assert!(full.contains("'--workers' '4'"));
        flow.mem_limit = Some(512 * 1024 * 1024);
        assert!(
            flow.repair_prompt("node", (0, 1), "failed", "", "", &[])
                .contains("'--workers' '1'")
        );
        std::fs::remove_file(helper).unwrap();
        assert!(
            !flow
                .repair_prompt("node", (0, 1), "failed", "", "", &[])
                .contains("Isolated acceptance entry")
        );
    }

    #[test]
    fn repair_prompt_identifies_tests_outside_the_application() {
        let (mut flow, _, _dir) = rejected_flow("unused");
        let tests = tempfile::tempdir().unwrap();
        flow.tests_dir = Some(tests.path().to_path_buf());
        let prompt = flow.repair_prompt(
            "node",
            (0, 1),
            "example.spec.ts: missing element",
            "",
            "",
            &[],
        );
        assert!(prompt.contains(&tests.path().canonicalize().unwrap().display().to_string()));
    }

    #[test]
    fn both_codegen_and_tool_turns_set_the_shared_routing_phase() {
        let (mut flow, _, dir) = rejected_flow("HTTP 503 temporarily unavailable");
        let relay =
            crate::routing::Relay::start("http://127.0.0.1:1/v1", vec![], dir.path()).unwrap();
        flow.routing = Some(relay.control.clone());
        flow.codegen_turn("fix", Duration::from_secs(60), "repair");
        assert_eq!(relay.control.phase(), crate::routing::Phase::Repair);
        // The missing test executable fails locally, after turn selects its phase.
        flow.turn("check", Duration::from_secs(60), "final check", true, None);
        assert_eq!(relay.control.phase(), crate::routing::Phase::Verify);
        flow.codegen_turn("plan", Duration::from_secs(60), "design");
        assert_eq!(relay.control.phase(), crate::routing::Phase::Design);
    }
    #[test]
    fn should_prefer_the_previous_requirement_snapshot_over_the_traceability_table() {
        let dir = tempfile::tempdir().unwrap();
        let trace = dir.path().join(".arc/traceability");
        std::fs::create_dir_all(&trace).unwrap();
        std::fs::write(
            trace.join("requirements.json"),
            r#"{"REQ-1": {"id": "REQ-1", "description": "new"}, "REQ-2": {"id": "REQ-2"}}"#,
        )
        .unwrap();
        let snapshot = dir.path().join(".arc/previous-requirements.json");
        std::fs::write(
            &snapshot,
            r#"{"REQ-1": {"id": "REQ-1", "description": "old"}}"#,
        )
        .unwrap();
        let records = super::previous_requirement_records(dir.path(), Some(&snapshot));
        assert_eq!(records.len(), 1);
        assert_eq!(records["REQ-1"]["description"], "old");
        // A snapshot path that cannot be read means "no previous table" — never the
        // traceability file, which already holds the new tree by then.
        let missing = dir.path().join(".arc/nope.json");
        assert!(super::previous_requirement_records(dir.path(), Some(&missing)).is_empty());
        // Without a snapshot (kernel run directly), the traceability file is the source.
        assert_eq!(
            super::previous_requirement_records(dir.path(), None).len(),
            2
        );
    }
}
