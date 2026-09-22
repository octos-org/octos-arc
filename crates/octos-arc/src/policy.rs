//! The ARC policy file (`arc/arc-policy.toml`).
//!
//! Every tunable the Python adapter exposed as an `OCTOS_*` environment
//! variable is a field here with the same default. Precedence is
//! command line > environment variable > policy file > built-in default;
//! [`Policy::apply_env`] implements the environment layer and
//! [`ENV_OVERRIDES`] is the single table that maps variable names to fields
//! (so the coverage test can assert nothing was dropped in the port).

use std::path::Path;

use eyre::{Result, WrapErr, bail, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    pub mode: ModePolicy,
    pub budget: BudgetPolicy,
    pub repair: RepairPolicy,
    pub requests: RequestPolicy,
    pub reasoning: ReasoningPolicy,
    pub acceptance: AcceptancePolicy,
    pub ports: PortPolicy,
    pub prompts: PromptPolicy,
    pub session: SessionPolicy,
    pub debug: DebugPolicy,
}

/// Which turn shapes a tree gets (`Flow.codegen_mode`, `minimal_mode`,
/// skeleton/design gates in `arc/main.py`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModePolicy {
    /// One-request codegen turns for small trees (`OCTOS_ARC_CODEGEN`).
    pub codegen: bool,
    /// Trees up to this many ATOMIC nodes use codegen (`OCTOS_ARC_CODEGEN_MAX_NODES`).
    pub codegen_max_nodes: usize,
    /// Trees up to this size get the minimal self-verification text (`OCTOS_SMALL_TASK_NODES`).
    pub small_task_nodes: usize,
    /// auto | minimal | full (`OCTOS_VERIFY_MODE`).
    pub verify_mode: String,
    /// Separate skeleton turn only for trees with at least this many nodes (`OCTOS_SKELETON_MIN_NODES`).
    pub skeleton_min_nodes: usize,
    /// Always run the skeleton turn (`OCTOS_SKELETON_ALWAYS`).
    pub skeleton_always: bool,
    /// Design turn enabled (`OCTOS_DESIGN_TURN`).
    pub design_turn: bool,
    /// inline | separate (`OCTOS_DESIGN_MODE`).
    pub design_mode: String,
    /// Design only for trees with at least this many nodes (`OCTOS_DESIGN_MIN_NODES`).
    pub design_min_nodes: usize,
    /// Tiny-spec tier: spec statements only, bare HTML reply, harness-written static server (`OCTOS_ARC_TINY`).
    pub tiny: bool,
    /// A node whose spec text is shorter than this many characters uses the tiny tier (`OCTOS_ARC_TINY_SPEC_CHARS`).
    pub tiny_spec_chars: usize,
}

impl Default for ModePolicy {
    fn default() -> Self {
        Self {
            codegen: true,
            codegen_max_nodes: 999,
            small_task_nodes: 2,
            verify_mode: "auto".into(),
            skeleton_min_nodes: 3,
            skeleton_always: false,
            design_turn: true,
            design_mode: "inline".into(),
            design_min_nodes: 3,
            tiny: true,
            tiny_spec_chars: 1500,
        }
    }
}

/// Wall-clock budgets (`Flow.__init__`, `node_cycle`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BudgetPolicy {
    /// Whole-run budget in seconds; 0 = automatic: max(floor, seconds_per_node × nodes) (`OCTOS_TIME_BUDGET`).
    pub time_budget_seconds: u64,
    /// Floor used by the automatic budget.
    pub time_budget_floor_seconds: u64,
    /// Per-node allowance used by the automatic budget (`OCTOS_SECONDS_PER_NODE`).
    pub seconds_per_node: u64,
    /// Cap per node including repairs (`OCTOS_NODE_TIME_BUDGET`).
    pub node_time_budget_seconds: u64,
    /// Minimum node budget when the remaining time is short.
    pub node_time_floor_seconds: u64,
    /// Seconds per model turn (`OCTOS_NODE_TIMEOUT`).
    pub node_timeout_seconds: u64,
    /// Seconds per design turn (`OCTOS_DESIGN_TIMEOUT`).
    pub design_timeout_seconds: u64,
    /// The implement turn may use this fraction of the node budget (`OCTOS_IMPLEMENT_FRACTION`).
    pub implement_fraction: f64,
    /// Do not start a repair turn with less than this left (`OCTOS_MIN_REPAIR_SECONDS`).
    pub min_repair_seconds: u64,
    /// Tool-mode iterations per turn (`OCTOS_MAX_ITERATIONS`).
    pub max_iterations: u32,
    /// Run-wide cost guard (`OCTOS_ARC_MAX_TOTAL_TOKENS`): billable tokens (prompt + completion)
    /// after which no repair turn starts, remaining nodes get one implement turn and the final
    /// suite runs once. -1 = derived once the tree is known: max(6M, 2.5M × nodes), ≈3× the
    /// calibrated keep run (cloud 2224a9013528: 26M tokens for 32 nodes); 0 = off.
    pub max_total_tokens: i64,
    /// Same guard on model turns (`OCTOS_ARC_MAX_TURNS`): -1 = max(24, 4 × nodes); 0 = off.
    pub max_turns: i64,
    /// Optional absolute token ceiling for a per-run spend rule (`OCTOS_ARC_MAX_TOTAL_TOKENS_ABS`),
    /// 0 = off. A healthy 125-node tree costs more than ¥50 ≈ 75M tokens, so set it only when the
    /// spend rule outranks completion.
    pub max_total_tokens_abs: u64,
}

impl Default for BudgetPolicy {
    fn default() -> Self {
        Self {
            time_budget_seconds: 0,
            time_budget_floor_seconds: 3600,
            seconds_per_node: 1500,
            node_time_budget_seconds: 1500,
            node_time_floor_seconds: 240,
            node_timeout_seconds: 1200,
            design_timeout_seconds: 420,
            implement_fraction: 0.6,
            min_repair_seconds: 300,
            max_iterations: 500,
            max_total_tokens: -1,
            max_turns: -1,
            max_total_tokens_abs: 0,
        }
    }
}

/// Repair rounds (`Flow.acceptance_loop`, `final_acceptance`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RepairPolicy {
    /// K, acceptance repair rounds per node (`OCTOS_REPAIR_ROUNDS`; an explicit value also
    /// replaces `rounds_large_tree`).
    pub rounds: u32,
    /// Repair rounds for trees with more than `large_tree_nodes` ATOMIC nodes (wf-adapter-30:
    /// keep barely repaired, and the identical-failure / no-improvement stops make 5 rounds rare).
    pub rounds_large_tree: u32,
    pub large_tree_nodes: usize,
    /// Codegen repairs before falling back to tool mode (`OCTOS_ARC_CODEGEN_REPAIRS`).
    pub codegen_repairs: u32,
    /// One full rewrite turn when round 0 passes nothing (`OCTOS_ARC_REWRITE_ON_ZERO`).
    pub rewrite_on_zero: bool,
    /// Repair rounds after the full parallel suite (`OCTOS_FINAL_REPAIR_ROUNDS`).
    pub final_rounds: u32,
    /// Stop a node after this many repairs without improvement.
    pub stall_limit: u32,
    /// Roll back to the best commit after this many consecutive regressions.
    pub regression_limit: u32,
}

impl Default for RepairPolicy {
    fn default() -> Self {
        Self {
            rounds: 5,
            codegen_repairs: 2,
            rewrite_on_zero: true,
            final_rounds: 2,
            stall_limit: 2,
            regression_limit: 2,
            rounds_large_tree: 3,
            large_tree_nodes: 2,
        }
    }
}

/// Hard per-turn request caps (`llm_proxy.enforce_turn_budget`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RequestPolicy {
    /// Implement turns of small tasks (`OCTOS_ARC_IMPLEMENT_REQUESTS`; 0 = off).
    pub implement: u32,
    /// Implement turns of large trees (the adapter left those uncapped; 0 = off).
    pub implement_large: u32,
    /// Repair turns (`OCTOS_ARC_REPAIR_REQUESTS`; 0 = off).
    pub repair: u32,
    /// Codegen turns: attempts including transient retries (`OCTOS_ARC_CODEGEN_REQUESTS`).
    pub codegen: u32,
}

impl Default for RequestPolicy {
    fn default() -> Self {
        Self {
            implement: 20,
            implement_large: 0,
            repair: 10,
            codegen: 3,
        }
    }
}

/// Model-side controls that used to live in `llm_proxy.py`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReasoningPolicy {
    /// auto | low | medium | high | none | passthrough (`OCTOS_ARC_REASONING`).
    pub mode: String,
    /// Override for the first implement turn of small tasks; empty = base mode (`OCTOS_ARC_IMPLEMENT_REASONING`).
    pub implement_override: String,
    /// Minimum `max_tokens` on chat requests (`OCTOS_ARC_MAX_TOKENS`).
    pub max_tokens_min: u32,
    /// One JSON response upstream instead of SSE (`OCTOS_ARC_DESTREAM`).
    pub destream: bool,
    /// Drop ARC-irrelevant prompt sections and tool schemas in tool mode (`OCTOS_ARC_TRIM_PROMPT`).
    pub trim_prompt: bool,
    /// Remove shell tools in minimal-verification turns (`OCTOS_ARC_DROP_SHELL`).
    pub drop_shell: bool,
    /// Transient-error retries per turn and the base backoff in seconds.
    pub transient_retries: u32,
    pub transient_backoff_seconds: u64,
    /// Seconds to wait for the endpoint to answer the start-up probe.
    pub probe_patience_seconds: u64,
    /// Codegen turns for specs shorter than this many characters run with thinking off and the
    /// compact size rule; larger specs keep the base mode and the multi-page mechanisms
    /// (`OCTOS_ARC_CODEGEN_REASONING_CHARS`; only when `mode` is auto).
    pub codegen_reasoning_chars: usize,
}

impl Default for ReasoningPolicy {
    fn default() -> Self {
        Self {
            mode: "auto".into(),
            implement_override: String::new(),
            max_tokens_min: 32768,
            destream: true,
            trim_prompt: true,
            drop_shell: true,
            transient_retries: 3,
            transient_backoff_seconds: 30,
            probe_patience_seconds: 600,
            codegen_reasoning_chars: 5000,
        }
    }
}

/// Local Playwright acceptance (`acceptance.py`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AcceptancePolicy {
    /// Per-test timeout, same as the grader (`OCTOS_ARC_TEST_TIMEOUT_MS`).
    pub test_timeout_ms: u64,
    /// Tests slower than this are reported to the repair turn (`OCTOS_ARC_SLOW_MS`).
    pub slow_ms: u64,
    /// Workers for per-node runs (`OCTOS_ARC_TEST_WORKERS`).
    pub workers: u32,
    /// Workers for the full parallel suite (`OCTOS_ARC_FINAL_WORKERS`).
    pub final_workers: u32,
    /// Initial regression interval; subsequent gaps never exceed twice it; 0 disables (`OCTOS_ARC_REGRESSION_CHECKPOINT`).
    pub regression_checkpoint_nodes: usize,
    /// Container memory per Chromium worker in MiB (per-node runs).
    pub memory_per_worker_mib: u64,
    /// Memory per worker for the full parallel suite: the platform grades with 4 workers in
    /// 2 GiB, ≈450 MiB each, so the final suite mimics it and slow tests surface before grading.
    pub final_memory_per_worker_mib: u64,
    /// Parallelism inside a spec file too (`OCTOS_ARC_FULLY_PARALLEL`).
    pub fully_parallel: bool,
    /// Install a private Playwright when none is found (`OCTOS_ARC_INSTALL_PLAYWRIGHT`).
    pub install_playwright: bool,
    /// Directory holding node_modules/@playwright/test (`OCTOS_ARC_PLAYWRIGHT_ROOT`).
    pub playwright_root: String,
    /// Version installed when the tests declare none.
    pub playwright_fallback_version: String,
    /// Seconds allowed for a private install.
    pub install_timeout_seconds: u64,
    /// Seconds allowed for one Playwright run.
    pub run_wall_timeout_seconds: u64,
    /// Seconds to wait for the backend to bind its port.
    pub start_wait_seconds: u64,
    /// Seconds allowed for `npm run build` / `npm install`.
    pub build_timeout_seconds: u64,
    /// Mirror node states onto spec ids (`OCTOS_ARC_ALIAS_SPEC_IDS`).
    pub alias_spec_ids: bool,
}

impl Default for AcceptancePolicy {
    fn default() -> Self {
        Self {
            test_timeout_ms: 10000,
            slow_ms: 3000,
            workers: 2,
            final_workers: 4,
            regression_checkpoint_nodes: 4,
            memory_per_worker_mib: 700,
            final_memory_per_worker_mib: 450,
            fully_parallel: false,
            install_playwright: true,
            playwright_root: String::new(),
            playwright_fallback_version: "1.63.0".into(),
            install_timeout_seconds: 540,
            run_wall_timeout_seconds: 900,
            start_wait_seconds: 45,
            build_timeout_seconds: 600,
            alias_spec_ids: true,
        }
    }
}

/// Port contract (`Flow.smoke_port`, `spec_base_ports`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PortPolicy {
    /// Port for the harness's own runs; bumped when it equals the grading port (`OCTOS_SMOKE_PORT`).
    pub smoke_port: u16,
    /// Tell the model to also bind the ports the specs default to.
    pub extra_port_contract: bool,
}

impl Default for PortPolicy {
    fn default() -> Self {
        Self {
            smoke_port: 3100,
            extra_port_contract: true,
        }
    }
}

/// Prompt files and the keyword gates for optional contract blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PromptPolicy {
    /// Directory with `<name>.md` templates, relative to the policy file; missing files use the built-in text.
    pub dir: String,
    /// Quote the node's spec files into tool-mode prompts (`OCTOS_ARC_INLINE_SPECS`).
    pub inline_specs: bool,
    /// Budget for quoted specs (`OCTOS_ARC_INLINE_SPEC_CHARS`).
    pub inline_spec_chars: usize,
    /// Budget for quoting the app's sources into repair/rewrite prompts (`OCTOS_ARC_INLINE_SOURCE_CHARS`; 0 = off).
    pub inline_source_chars: usize,
    /// Codegen prompt budget in characters (`OCTOS_ARC_CODEGEN_CONTEXT_CHARS`, round 35): the spec
    /// must fit 60% of it for a node to take the single-request path; the existing sources are
    /// quoted into the rest, ranked by how many of the spec's terms they contain.
    pub codegen_context_chars: usize,
    /// Include the performance rules (`OCTOS_PERF_CONTRACT`).
    pub perf_contract: bool,
    /// Inject guard corrections into the next prompt (`OCTOS_GUARD`).
    pub guard: bool,
    /// Requirement keywords that enable the session/performance contract blocks.
    pub session_keywords: Vec<String>,
    /// Requirement keywords that enable the fixture-data contract block.
    pub data_keywords: Vec<String>,
}

impl Default for PromptPolicy {
    fn default() -> Self {
        Self {
            dir: "prompts".into(),
            inline_specs: true,
            inline_spec_chars: 24000,
            inline_source_chars: 40000,
            codegen_context_chars: 90000,
            perf_contract: true,
            guard: true,
            session_keywords: [
                "login", "log in", "sign in", "password", "session", "register", "注册", "登录",
                "密码", "会话",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            data_keywords: [
                "seed",
                "published",
                "fixture",
                "option",
                "select",
                "dropdown",
                "选项",
                "下拉",
                "预置",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
        }
    }
}

/// Tool-mode session shape (`OctosDriver`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionPolicy {
    /// turn | node | run — when a fresh kernel session starts (`OCTOS_SESSION_SCOPE`).
    pub scope: String,
    /// stdio | chat (`OCTOS_DRIVER`).
    pub driver: String,
    /// Profile for the chat fallback (`OCTOS_CHAT_PROFILE`).
    pub chat_profile: String,
}

impl Default for SessionPolicy {
    fn default() -> Self {
        Self {
            scope: "turn".into(),
            driver: "stdio".into(),
            chat_profile: "coding".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DebugPolicy {
    /// Keep the first request bodies under .arc/llm-requests (`OCTOS_ARC_PROXY_DUMP`).
    pub dump_requests: bool,
    /// Walk the whole flow without calling the model (`OCTOS_ARC_DRYRUN`).
    pub dry_run: bool,
    /// Dry run only: tool-mode implement/skeleton/repair turns write the placeholder app
    /// instead of returning a sentence, so multi-node trees walk every node, the final
    /// suite and the folder marking (`OCTOS_ARC_DRYRUN_FILES`; the Python dry run never
    /// writes files in tool mode, so leave this off for side-by-side structure checks).
    pub dry_run_tool_files: bool,
}

/// Environment variable → policy field. One row per variable the Python
/// adapter reads; the test below checks the list against the adapter's
/// inventory so nothing silently loses its override.
pub const ENV_OVERRIDES: &[(&str, &str)] = &[
    ("OCTOS_ARC_CODEGEN", "mode.codegen"),
    ("OCTOS_ARC_CODEGEN_MAX_NODES", "mode.codegen_max_nodes"),
    ("OCTOS_SMALL_TASK_NODES", "mode.small_task_nodes"),
    ("OCTOS_VERIFY_MODE", "mode.verify_mode"),
    ("OCTOS_SKELETON_MIN_NODES", "mode.skeleton_min_nodes"),
    ("OCTOS_SKELETON_ALWAYS", "mode.skeleton_always"),
    ("OCTOS_DESIGN_TURN", "mode.design_turn"),
    ("OCTOS_DESIGN_MODE", "mode.design_mode"),
    ("OCTOS_DESIGN_MIN_NODES", "mode.design_min_nodes"),
    ("OCTOS_ARC_TINY", "mode.tiny"),
    ("OCTOS_ARC_TINY_SPEC_CHARS", "mode.tiny_spec_chars"),
    ("OCTOS_TIME_BUDGET", "budget.time_budget_seconds"),
    ("OCTOS_SECONDS_PER_NODE", "budget.seconds_per_node"),
    ("OCTOS_NODE_TIME_BUDGET", "budget.node_time_budget_seconds"),
    ("OCTOS_NODE_TIMEOUT", "budget.node_timeout_seconds"),
    ("OCTOS_DESIGN_TIMEOUT", "budget.design_timeout_seconds"),
    ("OCTOS_IMPLEMENT_FRACTION", "budget.implement_fraction"),
    ("OCTOS_MIN_REPAIR_SECONDS", "budget.min_repair_seconds"),
    ("OCTOS_MAX_ITERATIONS", "budget.max_iterations"),
    ("OCTOS_ARC_MAX_TOTAL_TOKENS", "budget.max_total_tokens"),
    ("OCTOS_ARC_MAX_TURNS", "budget.max_turns"),
    (
        "OCTOS_ARC_MAX_TOTAL_TOKENS_ABS",
        "budget.max_total_tokens_abs",
    ),
    ("OCTOS_REPAIR_ROUNDS", "repair.rounds"),
    ("OCTOS_ARC_CODEGEN_REPAIRS", "repair.codegen_repairs"),
    ("OCTOS_ARC_REWRITE_ON_ZERO", "repair.rewrite_on_zero"),
    ("OCTOS_FINAL_REPAIR_ROUNDS", "repair.final_rounds"),
    ("OCTOS_ARC_IMPLEMENT_REQUESTS", "requests.implement"),
    ("OCTOS_ARC_REPAIR_REQUESTS", "requests.repair"),
    ("OCTOS_ARC_CODEGEN_REQUESTS", "requests.codegen"),
    ("OCTOS_ARC_REASONING", "reasoning.mode"),
    (
        "OCTOS_ARC_CODEGEN_REASONING_CHARS",
        "reasoning.codegen_reasoning_chars",
    ),
    (
        "OCTOS_ARC_IMPLEMENT_REASONING",
        "reasoning.implement_override",
    ),
    ("OCTOS_ARC_MAX_TOKENS", "reasoning.max_tokens_min"),
    ("OCTOS_ARC_DESTREAM", "reasoning.destream"),
    ("OCTOS_ARC_TRIM_PROMPT", "reasoning.trim_prompt"),
    ("OCTOS_ARC_DROP_SHELL", "reasoning.drop_shell"),
    ("OCTOS_ARC_TEST_TIMEOUT_MS", "acceptance.test_timeout_ms"),
    ("OCTOS_ARC_SLOW_MS", "acceptance.slow_ms"),
    ("OCTOS_ARC_TEST_WORKERS", "acceptance.workers"),
    ("OCTOS_ARC_FINAL_WORKERS", "acceptance.final_workers"),
    (
        "OCTOS_ARC_REGRESSION_CHECKPOINT",
        "acceptance.regression_checkpoint_nodes",
    ),
    ("OCTOS_ARC_FULLY_PARALLEL", "acceptance.fully_parallel"),
    (
        "OCTOS_ARC_INSTALL_PLAYWRIGHT",
        "acceptance.install_playwright",
    ),
    ("OCTOS_ARC_PLAYWRIGHT_ROOT", "acceptance.playwright_root"),
    ("OCTOS_ARC_ALIAS_SPEC_IDS", "acceptance.alias_spec_ids"),
    ("OCTOS_SMOKE_PORT", "ports.smoke_port"),
    ("OCTOS_ARC_INLINE_SPECS", "prompts.inline_specs"),
    ("OCTOS_ARC_INLINE_SPEC_CHARS", "prompts.inline_spec_chars"),
    (
        "OCTOS_ARC_INLINE_SOURCE_CHARS",
        "prompts.inline_source_chars",
    ),
    (
        "OCTOS_ARC_CODEGEN_CONTEXT_CHARS",
        "prompts.codegen_context_chars",
    ),
    ("OCTOS_PERF_CONTRACT", "prompts.perf_contract"),
    ("OCTOS_GUARD", "prompts.guard"),
    ("OCTOS_SESSION_SCOPE", "session.scope"),
    ("OCTOS_DRIVER", "session.driver"),
    ("OCTOS_CHAT_PROFILE", "session.chat_profile"),
    ("OCTOS_ARC_PROXY_DUMP", "debug.dump_requests"),
    ("OCTOS_ARC_DRYRUN", "debug.dry_run"),
    ("OCTOS_ARC_DRYRUN_FILES", "debug.dry_run_tool_files"),
];

fn parse_bool(raw: &str) -> Result<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" | "" => Ok(false),
        other => bail!("not a boolean: {other:?}"),
    }
}

fn parse<T: std::str::FromStr>(raw: &str, key: &str) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    raw.trim()
        .parse::<T>()
        .map_err(|error| eyre::eyre!("{key}: cannot parse {raw:?}: {error}"))
}

impl Policy {
    /// Read a policy file; a missing path means built-in defaults.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let Some(path) = path else {
            return Ok(Self::default());
        };
        let text = std::fs::read_to_string(path)
            .wrap_err_with(|| format!("reading policy {}", path.display()))?;
        Self::parse_toml(&text).wrap_err_with(|| format!("parsing policy {}", path.display()))
    }

    pub fn parse_toml(text: &str) -> Result<Self> {
        let policy: Self = toml::from_str(text)?;
        policy.validate()?;
        Ok(policy)
    }

    /// Apply the environment layer. `lookup` abstracts `std::env::var` so the
    /// precedence rules are testable without mutating process state. Returns
    /// the `KEY=value` overrides that were applied, for the run log.
    pub fn apply_env(&mut self, lookup: &dyn Fn(&str) -> Option<String>) -> Result<Vec<String>> {
        let mut applied = Vec::new();
        for (variable, key) in ENV_OVERRIDES {
            if let Some(raw) = lookup(variable) {
                self.set(key, &raw)
                    .wrap_err_with(|| format!("environment variable {variable}"))?;
                applied.push(format!("{variable}={raw}"));
            }
        }
        // Legacy alias: OCTOS_SESSION_PER_TURN=0 meant one session for the run.
        if lookup("OCTOS_SESSION_SCOPE").is_none()
            && lookup("OCTOS_SESSION_PER_TURN").as_deref() == Some("0")
        {
            self.session.scope = "run".into();
            applied.push("OCTOS_SESSION_PER_TURN=0".into());
        }
        self.validate()?;
        Ok(applied)
    }

    /// Set one field by its dotted key (the same keys `ENV_OVERRIDES` uses).
    pub fn set(&mut self, key: &str, raw: &str) -> Result<()> {
        match key {
            "mode.codegen" => self.mode.codegen = parse_bool(raw)?,
            "mode.codegen_max_nodes" => self.mode.codegen_max_nodes = parse(raw, key)?,
            "mode.small_task_nodes" => self.mode.small_task_nodes = parse(raw, key)?,
            "mode.verify_mode" => self.mode.verify_mode = raw.trim().into(),
            "mode.skeleton_min_nodes" => self.mode.skeleton_min_nodes = parse(raw, key)?,
            "mode.skeleton_always" => self.mode.skeleton_always = parse_bool(raw)?,
            "mode.design_turn" => self.mode.design_turn = parse_bool(raw)?,
            "mode.design_mode" => self.mode.design_mode = raw.trim().into(),
            "mode.design_min_nodes" => self.mode.design_min_nodes = parse(raw, key)?,
            "mode.tiny" => self.mode.tiny = parse_bool(raw)?,
            "mode.tiny_spec_chars" => self.mode.tiny_spec_chars = parse(raw, key)?,
            "budget.time_budget_seconds" => self.budget.time_budget_seconds = parse(raw, key)?,
            "budget.seconds_per_node" => self.budget.seconds_per_node = parse(raw, key)?,
            "budget.node_time_budget_seconds" => {
                self.budget.node_time_budget_seconds = parse(raw, key)?
            }
            "budget.node_timeout_seconds" => self.budget.node_timeout_seconds = parse(raw, key)?,
            "budget.design_timeout_seconds" => {
                self.budget.design_timeout_seconds = parse(raw, key)?
            }
            "budget.implement_fraction" => self.budget.implement_fraction = parse(raw, key)?,
            "budget.min_repair_seconds" => self.budget.min_repair_seconds = parse(raw, key)?,
            "budget.max_iterations" => self.budget.max_iterations = parse(raw, key)?,
            "budget.max_total_tokens" => self.budget.max_total_tokens = parse(raw, key)?,
            "budget.max_turns" => self.budget.max_turns = parse(raw, key)?,
            "budget.max_total_tokens_abs" => self.budget.max_total_tokens_abs = parse(raw, key)?,
            "repair.rounds" => {
                // An explicit K applies to every tree size, like OCTOS_REPAIR_ROUNDS in the adapter.
                self.repair.rounds = parse(raw, key)?;
                self.repair.rounds_large_tree = self.repair.rounds;
            }
            "repair.rounds_large_tree" => self.repair.rounds_large_tree = parse(raw, key)?,
            "repair.large_tree_nodes" => self.repair.large_tree_nodes = parse(raw, key)?,
            "repair.codegen_repairs" => self.repair.codegen_repairs = parse(raw, key)?,
            "repair.rewrite_on_zero" => self.repair.rewrite_on_zero = parse_bool(raw)?,
            "repair.final_rounds" => self.repair.final_rounds = parse(raw, key)?,
            "requests.implement" => self.requests.implement = parse(raw, key)?,
            "requests.implement_large" => self.requests.implement_large = parse(raw, key)?,
            "requests.repair" => self.requests.repair = parse(raw, key)?,
            "requests.codegen" => self.requests.codegen = parse(raw, key)?,
            "reasoning.mode" => self.reasoning.mode = raw.trim().into(),
            "reasoning.codegen_reasoning_chars" => {
                self.reasoning.codegen_reasoning_chars = parse(raw, key)?
            }
            "reasoning.implement_override" => self.reasoning.implement_override = raw.trim().into(),
            "reasoning.max_tokens_min" => self.reasoning.max_tokens_min = parse(raw, key)?,
            "reasoning.destream" => self.reasoning.destream = parse_bool(raw)?,
            "reasoning.trim_prompt" => self.reasoning.trim_prompt = parse_bool(raw)?,
            "reasoning.drop_shell" => self.reasoning.drop_shell = parse_bool(raw)?,
            "acceptance.test_timeout_ms" => self.acceptance.test_timeout_ms = parse(raw, key)?,
            "acceptance.slow_ms" => self.acceptance.slow_ms = parse(raw, key)?,
            "acceptance.workers" => self.acceptance.workers = parse(raw, key)?,
            "acceptance.final_workers" => self.acceptance.final_workers = parse(raw, key)?,
            "acceptance.regression_checkpoint_nodes" => {
                self.acceptance.regression_checkpoint_nodes = parse(raw, key)?
            }
            "acceptance.memory_per_worker_mib" => {
                self.acceptance.memory_per_worker_mib = parse(raw, key)?
            }
            "acceptance.final_memory_per_worker_mib" => {
                self.acceptance.final_memory_per_worker_mib = parse(raw, key)?
            }
            "acceptance.fully_parallel" => self.acceptance.fully_parallel = parse_bool(raw)?,
            "acceptance.install_playwright" => {
                self.acceptance.install_playwright = parse_bool(raw)?
            }
            "acceptance.playwright_root" => self.acceptance.playwright_root = raw.trim().into(),
            "acceptance.alias_spec_ids" => self.acceptance.alias_spec_ids = parse_bool(raw)?,
            "ports.smoke_port" => self.ports.smoke_port = parse(raw, key)?,
            "prompts.inline_specs" => self.prompts.inline_specs = parse_bool(raw)?,
            "prompts.inline_spec_chars" => self.prompts.inline_spec_chars = parse(raw, key)?,
            "prompts.inline_source_chars" => self.prompts.inline_source_chars = parse(raw, key)?,
            "prompts.codegen_context_chars" => {
                self.prompts.codegen_context_chars = parse(raw, key)?
            }
            "prompts.perf_contract" => self.prompts.perf_contract = parse_bool(raw)?,
            "prompts.guard" => self.prompts.guard = parse_bool(raw)?,
            "session.scope" => self.session.scope = raw.trim().into(),
            "session.driver" => self.session.driver = raw.trim().into(),
            "session.chat_profile" => self.session.chat_profile = raw.trim().into(),
            "debug.dump_requests" => self.debug.dump_requests = parse_bool(raw)?,
            "debug.dry_run" => self.debug.dry_run = parse_bool(raw)?,
            "debug.dry_run_tool_files" => self.debug.dry_run_tool_files = parse_bool(raw)?,
            other => bail!("unknown policy key {other}"),
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            matches!(self.mode.verify_mode.as_str(), "auto" | "minimal" | "full"),
            "mode.verify_mode must be auto, minimal or full"
        );
        ensure!(
            matches!(self.mode.design_mode.as_str(), "inline" | "separate"),
            "mode.design_mode must be inline or separate"
        );
        ensure!(
            matches!(
                self.reasoning.mode.as_str(),
                "auto" | "low" | "medium" | "high" | "none" | "passthrough"
            ),
            "reasoning.mode must be auto, low, medium, high, none or passthrough"
        );
        ensure!(
            self.reasoning.implement_override.is_empty()
                || matches!(
                    self.reasoning.implement_override.as_str(),
                    "low" | "medium" | "high" | "none" | "passthrough"
                ),
            "reasoning.implement_override must be empty, low, medium, high, none or passthrough"
        );
        ensure!(
            self.budget.implement_fraction > 0.0 && self.budget.implement_fraction <= 1.0,
            "budget.implement_fraction must be in (0, 1]"
        );
        ensure!(
            matches!(self.session.scope.as_str(), "turn" | "node" | "run"),
            "session.scope must be turn, node or run"
        );
        ensure!(
            matches!(self.session.driver.as_str(), "stdio" | "chat"),
            "session.driver must be stdio or chat"
        );
        ensure!(
            self.ports.smoke_port > 0,
            "ports.smoke_port must be positive"
        );
        ensure!(
            self.acceptance.workers >= 1,
            "acceptance.workers must be at least 1"
        );
        ensure!(
            self.acceptance.final_workers >= 1,
            "acceptance.final_workers must be at least 1"
        );
        Ok(())
    }

    /// Whole-run budget in seconds for a tree with `nodes` ATOMIC nodes
    /// (`Flow.run`: an explicit value is exact, otherwise scale by node count).
    pub fn time_budget_for(&self, nodes: usize) -> u64 {
        if self.budget.time_budget_seconds > 0 {
            self.budget.time_budget_seconds
        } else {
            self.budget
                .time_budget_floor_seconds
                .max(self.budget.seconds_per_node.saturating_mul(nodes as u64))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Every environment variable `arc/*.py` reads that is a harness tunable
    /// (platform inputs such as ARCBENCH_* and the binary locator stay in the
    /// Python glue). Regenerate with:
    /// grep -ohE 'os\.environ(\.get\(|\[)"[A-Z_]+"' arc/*.py | sort -u
    const PYTHON_TUNABLES: &[&str] = &[
        "OCTOS_ARC_CODEGEN_CONTEXT_CHARS",
        "OCTOS_ARC_MAX_TOTAL_TOKENS",
        "OCTOS_ARC_MAX_TURNS",
        "OCTOS_ARC_MAX_TOTAL_TOKENS_ABS",
        "OCTOS_ARC_TINY",
        "OCTOS_ARC_TINY_SPEC_CHARS",
        "OCTOS_ARC_CODEGEN_REASONING_CHARS",
        "OCTOS_ARC_ALIAS_SPEC_IDS",
        "OCTOS_ARC_CODEGEN",
        "OCTOS_ARC_CODEGEN_MAX_NODES",
        "OCTOS_ARC_CODEGEN_REPAIRS",
        "OCTOS_ARC_CODEGEN_REQUESTS",
        "OCTOS_ARC_DESTREAM",
        "OCTOS_ARC_DROP_SHELL",
        "OCTOS_ARC_FINAL_WORKERS",
        "OCTOS_ARC_REGRESSION_CHECKPOINT",
        "OCTOS_ARC_IMPLEMENT_REASONING",
        "OCTOS_ARC_IMPLEMENT_REQUESTS",
        "OCTOS_ARC_INLINE_SOURCE_CHARS",
        "OCTOS_ARC_INLINE_SPECS",
        "OCTOS_ARC_INLINE_SPEC_CHARS",
        "OCTOS_ARC_INSTALL_PLAYWRIGHT",
        "OCTOS_ARC_MAX_TOKENS",
        "OCTOS_ARC_PLAYWRIGHT_ROOT",
        "OCTOS_ARC_PROXY_DUMP",
        "OCTOS_ARC_REASONING",
        "OCTOS_ARC_REPAIR_REQUESTS",
        "OCTOS_ARC_REWRITE_ON_ZERO",
        "OCTOS_ARC_SLOW_MS",
        "OCTOS_ARC_TEST_TIMEOUT_MS",
        "OCTOS_ARC_TEST_WORKERS",
        "OCTOS_ARC_TRIM_PROMPT",
        "OCTOS_ARC_FULLY_PARALLEL",
        "OCTOS_CHAT_PROFILE",
        "OCTOS_DESIGN_MIN_NODES",
        "OCTOS_DESIGN_MODE",
        "OCTOS_DESIGN_TIMEOUT",
        "OCTOS_DESIGN_TURN",
        "OCTOS_DRIVER",
        "OCTOS_FINAL_REPAIR_ROUNDS",
        "OCTOS_GUARD",
        "OCTOS_IMPLEMENT_FRACTION",
        "OCTOS_MAX_ITERATIONS",
        "OCTOS_MIN_REPAIR_SECONDS",
        "OCTOS_NODE_TIMEOUT",
        "OCTOS_NODE_TIME_BUDGET",
        "OCTOS_PERF_CONTRACT",
        "OCTOS_REPAIR_ROUNDS",
        "OCTOS_SECONDS_PER_NODE",
        "OCTOS_SESSION_PER_TURN",
        "OCTOS_SESSION_SCOPE",
        "OCTOS_SKELETON_ALWAYS",
        "OCTOS_SKELETON_MIN_NODES",
        "OCTOS_SMALL_TASK_NODES",
        "OCTOS_SMOKE_PORT",
        "OCTOS_TIME_BUDGET",
        "OCTOS_VERIFY_MODE",
    ];

    #[test]
    fn should_cover_every_python_tunable_when_mapping_environment() {
        let mapped: Vec<&str> = ENV_OVERRIDES.iter().map(|(v, _)| *v).collect();
        for variable in PYTHON_TUNABLES {
            let covered = mapped.contains(variable) || *variable == "OCTOS_SESSION_PER_TURN";
            assert!(covered, "{variable} has no policy field");
        }
        // Every mapped key must be settable.
        let mut policy = Policy::default();
        for (_, key) in ENV_OVERRIDES {
            let sample = match *key {
                k if k.ends_with("mode") && k.starts_with("mode.verify") => "full",
                "mode.design_mode" => "separate",
                "reasoning.mode" => "low",
                "reasoning.implement_override" => "none",
                "session.scope" => "node",
                "session.driver" => "chat",
                "session.chat_profile" | "acceptance.playwright_root" => "x",
                "budget.implement_fraction" => "0.5",
                _ => "1",
            };
            policy
                .set(key, sample)
                .unwrap_or_else(|e| panic!("{key}: {e}"));
        }
    }

    #[test]
    fn should_match_python_defaults_when_no_policy_file() {
        let p = Policy::default();
        assert_eq!(p.budget.node_timeout_seconds, 1200);
        assert_eq!(p.budget.seconds_per_node, 1500);
        assert_eq!(p.budget.min_repair_seconds, 300);
        assert_eq!(p.budget.node_time_budget_seconds, 1500);
        assert_eq!(p.repair.rounds, 5);
        assert_eq!(p.repair.codegen_repairs, 2);
        assert_eq!(p.repair.final_rounds, 2);
        assert_eq!(p.mode.design_min_nodes, 3);
        assert_eq!(p.mode.skeleton_min_nodes, 3);
        assert_eq!(p.mode.small_task_nodes, 2);
        assert_eq!(p.mode.codegen_max_nodes, 999); // round 35: codegen for every tree size
        assert_eq!(p.mode.design_mode, "inline");
        assert_eq!(p.reasoning.mode, "auto");
        assert_eq!(p.reasoning.max_tokens_min, 32768);
        assert_eq!(p.reasoning.codegen_reasoning_chars, 5000);
        assert_eq!(p.budget.max_total_tokens, -1);
        assert_eq!(p.budget.max_turns, -1);
        assert_eq!(p.budget.max_total_tokens_abs, 0);
        assert_eq!(p.repair.rounds_large_tree, 3);
        assert_eq!(p.repair.large_tree_nodes, 2);
        assert_eq!(p.acceptance.final_memory_per_worker_mib, 450);
        assert!(p.mode.tiny);
        assert_eq!(p.mode.tiny_spec_chars, 1500);
        assert_eq!(p.requests.implement, 20);
        assert_eq!(p.requests.repair, 10);
        assert_eq!(p.requests.codegen, 3);
        assert_eq!(p.acceptance.test_timeout_ms, 10000);
        assert_eq!(p.acceptance.slow_ms, 3000);
        assert_eq!(p.acceptance.workers, 2);
        assert_eq!(p.acceptance.final_workers, 4);
        assert_eq!(p.ports.smoke_port, 3100);
        assert_eq!(p.prompts.inline_spec_chars, 24000);
        assert_eq!(p.prompts.inline_source_chars, 40000);
        assert_eq!(p.prompts.codegen_context_chars, 90000);
        assert_eq!(p.mode.codegen_max_nodes, 999);
        assert_eq!(p.session.scope, "turn");
        assert_eq!(p.time_budget_for(1), 3600);
        assert_eq!(p.time_budget_for(32), 48000);
    }

    #[test]
    fn should_let_environment_override_policy_file() {
        let mut policy =
            Policy::parse_toml("[repair]\nrounds = 2\n[reasoning]\nmode = \"low\"\n").unwrap();
        assert_eq!(policy.repair.rounds, 2);
        let env: HashMap<&str, &str> = HashMap::from([
            ("OCTOS_REPAIR_ROUNDS", "3"),
            ("OCTOS_ARC_CODEGEN", "0"),
            ("OCTOS_SESSION_PER_TURN", "0"),
            ("OCTOS_TIME_BUDGET", "900"),
        ]);
        let applied = policy
            .apply_env(&|name| env.get(name).map(|v| v.to_string()))
            .unwrap();
        assert_eq!(policy.repair.rounds, 3);
        assert!(!policy.mode.codegen);
        assert_eq!(policy.session.scope, "run");
        assert_eq!(policy.reasoning.mode, "low");
        assert_eq!(policy.time_budget_for(32), 900);
        assert_eq!(applied.len(), 4);
    }

    #[test]
    fn should_reject_unknown_fields_and_invalid_values() {
        assert!(Policy::parse_toml("[mode]\nbogus = 1\n").is_err());
        assert!(Policy::parse_toml("[mode]\nverify_mode = \"loud\"\n").is_err());
        let mut policy = Policy::default();
        assert!(policy.set("mode.codegen", "maybe").is_err());
        assert!(policy.set("nope", "1").is_err());
    }
}
