//! Chat command: interactive multi-turn conversation with an agent.

#[cfg(any(feature = "api", test))]
use std::io::IsTerminal;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(feature = "api")]
use std::sync::atomic::{AtomicBool, Ordering};

use clap::{Args, ValueEnum};
use colored::Colorize;
use eyre::{Result, WrapErr, eyre};
#[cfg(any(feature = "api", test))]
use octos_agent::{ToolApprovalDecision, ToolApprovalRequest, ToolApprovalRequester};
#[cfg(feature = "api")]
use octos_agent::{UserQuestionOutcome, UserQuestionRequest, UserQuestionRequester};
#[cfg(feature = "api")]
use octos_core::ui_protocol::UserQuestionAnswer;
use octos_llm::{EmbeddingProvider, LlmProvider, OpenAIEmbedder};
#[cfg(feature = "api")]
use rustyline::DefaultEditor;

use super::Executable;
use crate::config::Config;

#[cfg(feature = "api")]
mod oup;

/// Interactive multi-turn chat with an agent.
///
/// `Serialize`/`Deserialize` back the layered startup config (see
/// [`crate::config_layer`]): non-explicit fields fall back to
/// `config.cli.chat`.
#[derive(Debug, Args, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ChatCommand {
    /// Working directory (defaults to current directory).
    #[arg(short, long)]
    pub cwd: Option<PathBuf>,

    /// Data directory for episodes, memory, sessions (defaults to $OCTOS_HOME or ~/.octos).
    #[arg(long)]
    pub data_dir: Option<PathBuf>,

    /// Path to config file.
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// LLM provider to use (overrides config).
    #[arg(long)]
    pub provider: Option<String>,

    /// Model to use (overrides config).
    #[arg(long)]
    pub model: Option<String>,

    /// Custom base URL for the API endpoint (overrides config).
    #[arg(long)]
    pub base_url: Option<String>,

    /// API wire protocol to speak to `--base-url` (overrides config's
    /// `api_type`): `anthropic`, `openai`, or `responses`. Use this for a
    /// custom endpoint that speaks a known protocol (e.g. a z.ai/GLM Anthropic
    /// endpoint) instead of overloading `--provider` with a vendor name.
    #[arg(long = "api-type", visible_alias = "api-style")]
    pub api_type: Option<String>,

    /// Maximum LLM-loop iterations per message. 0 (default) means unlimited;
    /// cancellation, idle detection, tool timeouts and convergence checkpoints
    /// remain active.
    #[arg(long, default_value = "0")]
    pub max_iterations: u32,

    /// Verbose output (show tool outputs).
    #[arg(short, long)]
    pub verbose: bool,

    /// Disable automatic retry on transient errors.
    #[arg(long)]
    pub no_retry: bool,

    /// Send a single message and exit (non-interactive mode).
    #[arg(short, long)]
    pub message: Option<String>,

    /// Emit the result as a single JSON object on stdout (logs + UI go to
    /// stderr); intended for scripting / one-shot `--message` use. Requires
    /// `--message`: interactive `--json` is rejected (a REPL cannot keep
    /// stdout pure). On any runtime error a `{"error": "..."}` object is
    /// printed to stdout and the process exits non-zero, so stdout is always
    /// parseable — for a valid invocation. (Argument errors and `--help` are
    /// handled by the arg parser and follow normal CLI conventions: usage/help
    /// text and a non-zero exit, not a JSON object.)
    #[arg(long)]
    pub json: bool,

    /// Runtime profile to apply at startup (M8.3). Accepts a built-in name
    /// (`coding`, `coding-full`, `swarm`), a user-dir id under
    /// `~/.octos/profiles/<id>/`, or an explicit path to a profile
    /// JSON/TOML file.
    ///
    /// If the id names a stored serve/onboarding profile (one created by
    /// `octos serve` or octoscode, saved as `~/.octos/profiles/<id>.json`),
    /// its LLM provider/model, route, and API key (`env_vars`) are reused too —
    /// so you don't re-enter a model or key that a profile already holds.
    /// `--config`, `--provider`, and `--model` still override.
    ///
    /// Defaults to `coding`, the lean core-coding tool surface (files,
    /// shell, search, memory, spawn, check, plan tracking, user questions,
    /// and tool_search). Use `coding-full` for the unfiltered pre-lean tool
    /// set (web, research, pipelines, bundled skills) that the allow list
    /// excludes.
    #[arg(long)]
    pub profile: Option<String>,

    /// FULL AUTONOMY ("yolo"): bypass all approvals AND the sandbox — the
    /// agent can edit any file and run any command with network access,
    /// without asking. Equivalent to `--sandbox danger-full-access`. Only
    /// safe on a local single-user box you trust; risks data loss.
    ///
    /// `octos chat` is inherently local single-user, so this resolves through
    /// the solo runtime mode. The `--yolo` alias is hidden for brevity.
    #[arg(
        long = "dangerously-bypass-approvals-and-sandbox",
        visible_alias = "yolo",
        default_value_t = false
    )]
    pub dangerously_bypass_approvals_and_sandbox: bool,

    /// Sandbox / filesystem reach (codex parity). One of `read-only`,
    /// `workspace-write` (default), or `danger-full-access`. Mutually
    /// exclusive with a non-danger combination of `--yolo`.
    #[arg(long, value_enum)]
    pub sandbox: Option<ChatSandboxMode>,

    /// When to ask for command approval (codex parity). `ask` (default)
    /// prompts for risky commands; `never` fails them closed at the tool
    /// boundary instead of prompting.
    #[arg(long, value_enum)]
    pub ask_for_approval: Option<ChatApprovalMode>,

    /// Reasoning effort for thinking models: `none`, `low`, `medium`, `high`,
    /// or `max` (claude/codex parity). `none` disables reasoning where
    /// supported. Overrides the config
    /// `gateway.reasoning_effort`; non-thinking models ignore it, and
    /// providers without a distinct `max` tier clamp it to `high`.
    #[arg(long, value_enum)]
    pub effort: Option<ChatEffort>,

    /// Do NOT persist this run as an episode (ephemeral). Mirrors
    /// `claude --no-session-persistence`: by default a completed turn is
    /// saved to the episode store for future recall; this skips that write.
    /// Chat history and context sidecars use a temporary runtime directory.
    /// Shared profile memory, tools and skills remain available; explicit
    /// memory, file or goal writes are not disabled by this flag.
    ///
    /// This also lets many `octos chat` agents run CONCURRENTLY against one
    /// `--data-dir` (hence one shared `--profile`): a normal run takes an
    /// exclusive lock on the data dir's episode DB, but an ephemeral run falls
    /// back to an in-memory episode handle when the lock is already held,
    /// instead of failing. Pass this to fan out parallel agents on a single
    /// profile — e.g. review agents over one repo, or edit agents each in
    /// their own `--cwd`.
    #[arg(long)]
    pub no_session_persistence: bool,

    /// One-shot PROMPT (positional): `octos chat "…"` runs a single turn and
    /// exits, matching `claude -p "…"`. Sugar for `--message`; supplying the
    /// prompt both positionally and via `--message` is an error.
    #[arg(value_name = "PROMPT")]
    pub prompt: Option<String>,
}

/// `--sandbox` choices, mirroring codex's sandbox modes and octos's
/// [`PermissionProfile`](octos_agent::PermissionProfile).
///
/// `rename_all = "kebab-case"` makes the serde encoding (`"workspace-write"`,
/// …) identical to clap's `ValueEnum` possible-value names, so the config
/// layering round-trips a `config.cli.chat.sandbox` value losslessly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChatSandboxMode {
    /// Read-only workspace access; write/edit tools fail.
    ReadOnly,
    /// Read/write inside the workspace (default).
    WorkspaceWrite,
    /// No sandbox, host filesystem, network on, approvals never ("yolo").
    DangerFullAccess,
}

/// `--ask-for-approval` choices, mirroring codex.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChatApprovalMode {
    /// Prompt for approval on risky commands (default).
    Ask,
    /// Never prompt; risky commands fail closed at the tool boundary.
    Never,
}

/// `--effort` choices, mirroring claude/codex reasoning-effort tiers. Maps
/// 1:1 to [`octos_llm::ReasoningEffort`]. For these single-word variants
/// clap's default `ValueEnum` naming and serde's `kebab-case` agree
/// (`none`/`low`/`medium`/`high`/`max`), so a `config.cli.chat.effort` round-trips
/// losslessly — matching [`ChatSandboxMode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChatEffort {
    None,
    Low,
    Medium,
    High,
    Max,
}

impl From<ChatEffort> for octos_llm::ReasoningEffort {
    fn from(effort: ChatEffort) -> Self {
        match effort {
            ChatEffort::None => octos_llm::ReasoningEffort::Disabled,
            ChatEffort::Low => octos_llm::ReasoningEffort::Low,
            ChatEffort::Medium => octos_llm::ReasoningEffort::Medium,
            ChatEffort::High => octos_llm::ReasoningEffort::High,
            ChatEffort::Max => octos_llm::ReasoningEffort::Max,
        }
    }
}

/// `claude -p "…"` parity: fold a positional PROMPT into `--message`. The two
/// are two spellings of the same one-shot prompt, so supplying both is
/// contradictory — fail closed. Returns the single effective one-shot message,
/// or `None` for interactive mode (neither given).
#[cfg(any(feature = "api", test))]
fn reconcile_one_shot_prompt(
    message: Option<String>,
    prompt: Option<String>,
) -> Result<Option<String>> {
    match (message, prompt) {
        (Some(_), Some(_)) => Err(eyre!(
            "provide the prompt positionally OR via --message/-m, not both"
        )),
        (Some(message), None) => Ok(Some(message)),
        (None, prompt) => Ok(prompt),
    }
}

/// Apply the "explicit `--provider` detaches the inherited route" rule.
///
/// When the CLI names a provider that DIFFERS from the one already resolved
/// into `config` (typically inherited from a `--profile`), the sibling route
/// fields carried by that config — `base_url`, `api_key_env`, `api_type` — are
/// cleared so the *new* provider's defaults (or an explicit `--base-url` /
/// `--api-type` / key env) apply instead of a stale, mismatched route. Without
/// this, `octos chat --profile p --provider anthropic` keeps `p`'s openai
/// key-env + endpoint + wire protocol and builds an incoherent client
/// (anthropic name talking to openai's URL with openai's key). Naming the SAME
/// provider — or naming none, or having no inherited provider to detach from —
/// leaves the route untouched.
///
/// The model is deliberately NOT cleared: some providers mark the model as
/// required (`create_provider_with_api_type` bails when it is missing), so
/// blanking it would turn a provider swap into a hard error. Callers complete a
/// cross-provider switch by also passing `--model`.
#[cfg(any(feature = "api", test))]
pub(crate) fn detach_route_on_provider_override(config: &mut Config, cli_provider: Option<&str>) {
    let detaches = matches!(
        (cli_provider, config.provider.as_deref()),
        (Some(cli), Some(cfg)) if cli != cfg
    );
    if detaches {
        config.base_url = None;
        config.api_key_env = None;
        config.api_type = None;
    }
}

/// Resolve the chat session's [`EffectivePermissions`] from the CLI flags.
///
/// yolo GAP #3 — codex parity. Precedence and conflict rules:
///   * `--yolo` (a.k.a. `--dangerously-bypass-approvals-and-sandbox`) and
///     `--sandbox danger-full-access` both select the dangerous profile.
///   * `--yolo` combined with an explicit NON-danger `--sandbox` is
///     contradictory and errors (fail closed).
///   * `--ask-for-approval` overrides the approval policy, EXCEPT it may not
///     contradict the dangerous profile (which is always `never`).
///
/// `octos chat` is inherently local single-user, so the dangerous profile is
/// resolved through [`RuntimeMode::Solo`](octos_agent::RuntimeMode) — a
/// legitimate use of solo here, unlike the multi-tenant `serve` path.
pub fn resolve_chat_permissions(
    yolo: bool,
    sandbox: Option<ChatSandboxMode>,
    approval: Option<ChatApprovalMode>,
) -> Result<octos_agent::EffectivePermissions> {
    use octos_agent::{ApprovalPolicy, EffectivePermissions, PermissionProfile, RuntimeMode};

    // Determine the requested profile, folding `--yolo` and `--sandbox`
    // together and rejecting contradictions.
    let profile = match (yolo, sandbox) {
        // `--yolo` alone, or `--yolo`/`--sandbox danger-full-access` agreeing.
        (true, None) | (true, Some(ChatSandboxMode::DangerFullAccess)) => {
            PermissionProfile::DangerFullAccess
        }
        // `--yolo` with a non-danger sandbox is contradictory.
        (true, Some(other)) => {
            return Err(eyre!(
                "--dangerously-bypass-approvals-and-sandbox (--yolo) conflicts with \
                 --sandbox {:?}: yolo implies danger-full-access",
                other
            ));
        }
        (false, Some(ChatSandboxMode::ReadOnly)) => PermissionProfile::ReadOnly,
        (false, Some(ChatSandboxMode::WorkspaceWrite)) | (false, None) => {
            PermissionProfile::WorkspaceWrite
        }
        (false, Some(ChatSandboxMode::DangerFullAccess)) => PermissionProfile::DangerFullAccess,
    };

    // `octos chat` is local single-user; the dangerous profile is legitimately
    // resolved via Solo. `for_runtime` still centralizes the danger gate.
    let mut permissions = EffectivePermissions::for_runtime(profile, RuntimeMode::Solo)
        .map_err(|err| eyre!("{err}"))?;

    // Apply an explicit `--ask-for-approval` override, guarding against a
    // contradiction with the always-`never` dangerous profile.
    if let Some(approval) = approval {
        let requested = match approval {
            ChatApprovalMode::Ask => ApprovalPolicy::Ask,
            ChatApprovalMode::Never => ApprovalPolicy::Never,
        };
        if permissions.is_dangerous() && requested != ApprovalPolicy::Never {
            return Err(eyre!(
                "--ask-for-approval ask conflicts with danger-full-access, \
                 which never asks for approval"
            ));
        }
        permissions = permissions.with_approval_policy(requested);
    }

    Ok(permissions)
}

/// Exit commands.
#[cfg(feature = "api")]
const EXIT_COMMANDS: &[&str] = &["exit", "quit", "/exit", "/quit", ":q"];

/// Serializes ALL interactive stdin prompts (approvals AND user questions):
/// if two prompt-raising tools run in the same turn, their stdin prints/reads
/// must not interleave — otherwise a single `y` or a picked number could land
/// on whichever request won the stdin race rather than the one the user
/// meant. One module-level lock shared by both requesters — a function-local
/// `static` would be a *distinct* mutex per function and not serialize an
/// approval against a question (codex review).
#[cfg(any(feature = "api", test))]
static CHAT_PROMPT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// What the user answered at the CLI approval prompt. `ApproveSession`
/// mirrors the TUI's `s` action / the serve `approval_scope: "session"`
/// (`ApprovalScopeKind::ApproveForSession`): every later approval-gated
/// request in this chat process auto-resolves without prompting.
#[cfg(any(feature = "api", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliApprovalAnswer {
    ApproveOnce,
    ApproveSession,
    Deny,
}

/// Parse the `[y/s/N]` answer line. Empty / unrecognized input denies —
/// same fail-closed default as the old `[y/N]` prompt.
#[cfg(any(feature = "api", test))]
fn parse_cli_approval_answer(line: &str) -> CliApprovalAnswer {
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => CliApprovalAnswer::ApproveOnce,
        "s" | "session" => CliApprovalAnswer::ApproveSession,
        _ => CliApprovalAnswer::Deny,
    }
}

#[cfg(any(feature = "api", test))]
#[derive(Default)]
struct CliApprovalRequester {
    /// Set once the user answers `s` — the CLI-chat equivalent of the serve
    /// scope table's `(ApproveForSession, MatchKey::Session)` entry, which
    /// auto-resolves every subsequent approval in the session. Scope lifetime
    /// is this chat process (serve evicts its entry on session close).
    session_approved: std::sync::atomic::AtomicBool,
}

#[cfg(any(feature = "api", test))]
impl CliApprovalRequester {
    fn session_approved(&self) -> bool {
        self.session_approved
            .load(std::sync::atomic::Ordering::Acquire)
    }
}

#[cfg(any(feature = "api", test))]
#[async_trait::async_trait]
impl ToolApprovalRequester for CliApprovalRequester {
    async fn request_approval(&self, request: ToolApprovalRequest) -> ToolApprovalDecision {
        // Fast path: a prior `s` answer auto-resolves without prompting
        // (mirrors serve's `approval_auto_resolved`). Print a note so the
        // grant stays visible instead of commands silently running.
        if self.session_approved() {
            eprintln!(
                "{} {}",
                "Auto-approved (session scope):".dimmed(),
                request
                    .command
                    .as_deref()
                    .unwrap_or(&request.title)
                    .dimmed()
            );
            return ToolApprovalDecision::Approve;
        }
        let _guard = CHAT_PROMPT_LOCK.lock().await;
        // Re-check after acquiring the lock: a parallel tool batch can queue
        // two prompts; if the first answer was `s`, the second must
        // auto-resolve instead of prompting again.
        if self.session_approved() {
            eprintln!(
                "{} {}",
                "Auto-approved (session scope):".dimmed(),
                request
                    .command
                    .as_deref()
                    .unwrap_or(&request.title)
                    .dimmed()
            );
            return ToolApprovalDecision::Approve;
        }
        let answer = tokio::task::spawn_blocking(move || prompt_for_cli_approval(request))
            .await
            .unwrap_or(CliApprovalAnswer::Deny);
        match answer {
            CliApprovalAnswer::ApproveOnce => ToolApprovalDecision::Approve,
            CliApprovalAnswer::ApproveSession => {
                self.session_approved
                    .store(true, std::sync::atomic::Ordering::Release);
                ToolApprovalDecision::Approve
            }
            CliApprovalAnswer::Deny => ToolApprovalDecision::Deny,
        }
    }
}

#[cfg(any(feature = "api", test))]
fn prompt_for_cli_approval(request: ToolApprovalRequest) -> CliApprovalAnswer {
    eprintln!();
    eprintln!("{}", "Approval required".yellow().bold());
    eprintln!("{}", request.title.bold());
    eprintln!("{}", request.body);
    if let Some(cwd) = request.cwd.as_deref() {
        eprintln!("cwd: {cwd}");
    }

    if !io::stdin().is_terminal() {
        eprintln!("No interactive terminal available; denying request.");
        return CliApprovalAnswer::Deny;
    }

    eprint!("Approve? [y]es once / [s]ession / [N]o ");
    let _ = io::stderr().flush();
    let mut answer = String::new();
    match io::stdin().read_line(&mut answer) {
        Ok(_) => parse_cli_approval_answer(&answer),
        Err(_) => CliApprovalAnswer::Deny,
    }
}

#[cfg(feature = "api")]
struct CliUserQuestionRequester;

#[cfg(feature = "api")]
#[async_trait::async_trait]
impl UserQuestionRequester for CliUserQuestionRequester {
    async fn request_user_question(&self, request: UserQuestionRequest) -> UserQuestionOutcome {
        // Shared CHAT_PROMPT_LOCK: an approval and a question in the same
        // turn must not interleave their stdin reads.
        let _guard = CHAT_PROMPT_LOCK.lock().await;
        tokio::task::spawn_blocking(move || prompt_for_cli_user_question(request))
            .await
            .unwrap_or(UserQuestionOutcome::Cancelled)
    }
}

#[cfg(feature = "api")]
fn prompt_for_cli_user_question(request: UserQuestionRequest) -> UserQuestionOutcome {
    // No terminal to prompt on → let the tool degrade to its structured
    // fallback (the model re-asks in plain text) rather than block forever.
    if !io::stdin().is_terminal() {
        return UserQuestionOutcome::Unsupported;
    }

    eprintln!();
    eprintln!("{}", "Agent needs your input".cyan().bold());
    if !request.title.is_empty() {
        eprintln!("{}", request.title.bold());
    }
    if !request.body.is_empty() {
        eprintln!("{}", request.body);
    }

    let total = request.questions.len();
    let mut answers: Vec<UserQuestionAnswer> = Vec::with_capacity(total);

    for (qi, question) in request.questions.iter().enumerate() {
        eprintln!();
        if total > 1 {
            eprintln!("{}", format!("Question {} of {}", qi + 1, total).dimmed());
        }
        eprintln!("{}", question.question.bold());

        // Numbered options, then an "Other" row when free text is offered.
        for (oi, opt) in question.options.iter().enumerate() {
            if opt.description.is_empty() {
                eprintln!("  {}. {}", oi + 1, opt.label);
            } else {
                eprintln!("  {}. {} — {}", oi + 1, opt.label, opt.description.dimmed());
            }
        }
        let other_index = question.options.len() + 1;
        if question.allow_free_text {
            eprintln!("  {other_index}. {}", "Other (type your own)".dimmed());
        }

        if question.multi_select {
            eprint!("Choose one or more, comma-separated [1]: ");
        } else {
            eprint!("Choose [1]: ");
        }
        let _ = io::stderr().flush();

        let mut line = String::new();
        if io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
            // EOF (Ctrl-D) — abandon the whole question.
            return UserQuestionOutcome::Cancelled;
        }
        let (selected_labels, other_picked) = parse_question_selection(question, &line);

        // Read the free-text answer only when "Other" was chosen.
        let mut free_text: Option<String> = None;
        if other_picked {
            eprint!("Your answer: ");
            let _ = io::stderr().flush();
            let mut ft = String::new();
            if io::stdin().read_line(&mut ft).unwrap_or(0) == 0 {
                return UserQuestionOutcome::Cancelled;
            }
            let ft = ft.trim().to_string();
            if !ft.is_empty() {
                free_text = Some(ft);
            }
        }

        answers.push(UserQuestionAnswer {
            selected_labels,
            free_text,
        });
    }

    UserQuestionOutcome::Answered(answers)
}

/// Parse the numbered-selection `line` for one question into (option labels,
/// was-"Other"-picked). Empty input defaults to option 1. Out-of-range and
/// non-numeric tokens are dropped; a single-select question keeps only the
/// first valid pick. The "Other" row is index `options.len() + 1` and is only
/// honoured when the question allows free text. Pure so it can be unit-tested
/// without stdin.
#[cfg(any(feature = "api", test))]
fn parse_question_selection(
    question: &octos_core::ui_protocol::UserQuestion,
    line: &str,
) -> (Vec<String>, bool) {
    let other_index = question.options.len() + 1;
    // The "Other" row is only selectable when the question allows free text.
    let max_index = if question.allow_free_text {
        other_index
    } else {
        question.options.len()
    };
    let trimmed = line.trim();
    let mut picks: Vec<usize> = trimmed
        .split(',')
        .filter_map(|tok| tok.trim().parse::<usize>().ok())
        .filter(|n| *n >= 1 && *n <= max_index)
        .collect();
    // Empty or all-invalid input falls back to the first option (the "[1]"
    // default shown in the prompt) so a stray keystroke still yields an answer.
    if picks.is_empty() {
        picks.push(1);
    }
    if !question.multi_select {
        picks.truncate(1);
    }

    let mut selected_labels = Vec::new();
    let mut other_picked = false;
    for n in picks {
        // `n == other_index` is only reachable when free text is allowed
        // (`max_index` excludes it otherwise), so it always means "Other".
        if n == other_index {
            other_picked = true;
        } else if let Some(opt) = question.options.get(n - 1) {
            selected_labels.push(opt.label.clone());
        }
    }
    (selected_labels, other_picked)
}

/// Machine-readable result envelope for `octos chat --json --message`.
///
/// Text, answering model and token usage come from OUP's terminal and
/// canonical persisted answer, never an inferred completion summary.
#[cfg(any(feature = "api", test))]
#[derive(Debug, serde::Serialize)]
struct ChatJsonResult {
    /// Final assistant text for this turn.
    text: String,
    /// Model that actually produced the answer (e.g. `glm-5.2`). Taken from the
    /// final reply's provider provenance, so adaptive failover to a fallback
    /// lane is reported honestly.
    model: String,
    /// Prompt tokens consumed across the turn.
    input_tokens: u32,
    /// Completion tokens produced across the turn.
    output_tokens: u32,
}

#[cfg(any(feature = "api", test))]
impl ChatJsonResult {
    /// Serialize to a single-line JSON string (nice for piping). This fixed
    /// string/number-only struct cannot realistically fail to serialize; if it
    /// ever did, fall back to a valid JSON error object so stdout stays
    /// parseable.
    fn to_json_line(&self) -> String {
        serde_json::to_string(self)
            .unwrap_or_else(|_| "{\"error\":\"failed to serialize chat result\"}".to_string())
    }
}

/// Generic failures keep the existing one-object error shape. A failed OUP
/// terminal additionally exposes its typed code, exact cumulative usage, and
/// actual nonempty partial answer. It is never a successful result envelope.
fn json_error_value(error: &eyre::Report) -> serde_json::Value {
    let value = serde_json::json!({ "error": error.to_string() });
    #[cfg(feature = "api")]
    if let Some(failure) = error.downcast_ref::<crate::commands::oup_session::OupTurnFailure>() {
        let mut value = value;
        if let Some(terminal_error) = &failure.terminal_error {
            value["code"] = serde_json::json!(terminal_error.code);
        }
        value["usage"] = serde_json::json!(failure.partial.usage);
        if !failure.partial.text.trim().is_empty() {
            value["partial"] = serde_json::json!({
                "text": failure.partial.text,
                "model": failure.partial.model,
            });
        }
        return value;
    }
    value
}

fn print_json_error(error: &eyre::Report) {
    let obj = json_error_value(error);
    println!("{obj}");
    let _ = io::stdout().flush();
}

impl Executable for ChatCommand {
    fn execute(self) -> Result<()> {
        // Capture before `self` is moved: in `--json` mode ANY failure —
        // provider/config bootstrap or the turn itself — must surface as a
        // `{"error": ...}` object on stdout with a non-zero exit, so a caller
        // can always parse stdout instead of hitting an empty stream + an eyre
        // report on stderr.
        let json = self.json;
        // Build the runtime and run the turn inside one fallible block so that
        // EVERY failure — including a runtime-build failure — is caught by the
        // `--json` arm below rather than escaping as a bare `?` before it.
        let outcome = (|| -> Result<()> {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_stack_size(8 * 1024 * 1024) // 8MB stack for deep agent futures
                .build()
                .wrap_err("failed to create tokio runtime")?;
            runtime.block_on(self.run_async())
        })();
        match outcome {
            Ok(()) => Ok(()),
            Err(error) if json => {
                print_json_error(&error);
                std::process::exit(1);
            }
            Err(error) => Err(error),
        }
    }
}

impl ChatCommand {
    async fn run_async(self) -> Result<()> {
        #[cfg(feature = "api")]
        {
            self.run_oup().await
        }
        #[cfg(not(feature = "api"))]
        {
            eyre::bail!(
                "octos chat requires the OUP runtime; rebuild with default features or --features api"
            )
        }
    }
}

/// M8.3 — resolve the runtime profile for an `octos chat` invocation.
///
/// Resolution order:
///
/// 1. `--profile <name_or_path>` CLI arg, if present.
/// 2. `~/.octos/profile` symlink, if it exists (its target is treated as a
///    profile name or path using the same rules as the CLI arg).
/// 3. Built-in `coding` profile — the behaviour-parity fallback.
///
/// Returns the resolved [`octos_agent::profile::ProfileDefinition`] plus a
/// human-readable source label (`cli`, `symlink`, or `default`) suitable for
/// inclusion in the `profile resolved: ...` log line.
#[cfg(feature = "api")]
pub(crate) fn resolve_profile(
    cli_arg: &Option<String>,
) -> Result<(octos_agent::profile::ProfileDefinition, &'static str)> {
    use octos_agent::profile::ProfileDefinition;

    if let Some(arg) = cli_arg.as_deref() {
        let (def, _) = ProfileDefinition::load(arg)
            .wrap_err_with(|| format!("failed to load profile '{arg}'"))?;
        return Ok((def, "cli"));
    }

    // `~/.octos/profile` symlink (or plain file containing a profile name).
    // A symlink target can be either a path (dereferences normally through
    // filesystem APIs, which `load` will then detect as a path arg) or a
    // simple profile name if the link points at a directory under
    // `~/.octos/profiles/`.
    if let Some(home) = dirs::home_dir() {
        let pointer = home.join(".octos/profile");
        if pointer.symlink_metadata().is_ok() {
            // Plain symlink: dereference and feed the target into `load`.
            if let Ok(target) = std::fs::read_link(&pointer) {
                let target_str = target.to_string_lossy().to_string();
                if let Ok((def, _)) = ProfileDefinition::load(&target_str) {
                    return Ok((def, "symlink"));
                }
                tracing::warn!(
                    target = %target.display(),
                    "~/.octos/profile symlink target could not be resolved; falling back to default"
                );
            } else if let Ok(text) = std::fs::read_to_string(&pointer) {
                // Regular file: treat its first non-empty line as a profile name.
                let name = text
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .unwrap_or("");
                if !name.is_empty() {
                    if let Ok((def, _)) = ProfileDefinition::load(name) {
                        return Ok((def, "symlink"));
                    }
                }
            }
        }
    }

    let (def, _) = octos_agent::profile::ProfileDefinition::load("coding")
        .wrap_err("failed to load built-in coding profile")?;
    Ok((def, "default"))
}

/// Find the matching provider-specific tool policy for the active model.
/// Checks model ID first (e.g. "claude-sonnet-4-20250514"), then provider name (e.g. "gemini").
#[cfg(test)]
pub(crate) fn resolve_provider_policy(
    config: &Config,
    provider_name: &str,
    model_id: &str,
) -> Option<octos_agent::ToolPolicy> {
    if config.tool_policy_by_provider.is_empty() {
        return None;
    }
    // Exact model ID match first
    if let Some(policy) = config.tool_policy_by_provider.get(model_id) {
        return Some(policy.clone());
    }
    // Provider name match
    if let Some(policy) = config.tool_policy_by_provider.get(provider_name) {
        return Some(policy.clone());
    }
    None
}

/// Create an embedding provider from config, if configured.
pub(crate) fn create_embedder(config: &Config) -> Option<Arc<dyn EmbeddingProvider>> {
    let cfg = config.embedding.as_ref()?;

    // `provider = "llamacpp"` (in-process llama.cpp GGUF) was removed with the
    // octos-embed-llama crate. Fail LOUDLY rather than falling through to the
    // remote-provider path below, where "llamacpp" would be treated as an API
    // provider name and produce a baffling credential error.
    if cfg.provider.eq_ignore_ascii_case("llamacpp") || cfg.provider.eq_ignore_ascii_case("llama") {
        tracing::warn!(
            "embedding.provider=\"llamacpp\" is not available in this build; \
             ignoring and disabling embeddings"
        );
        return None;
    }

    // `provider = "mlx"` was the Apple-Silicon-only in-process backend. It has
    // been replaced by `"llamacpp"` above, which is cross-platform and measured
    // at parity or better. Fail LOUDLY rather than falling through to the
    // remote-provider path below, where "mlx" would be treated as an API
    // provider name and produce a baffling credential error.
    if cfg.provider.eq_ignore_ascii_case("mlx") {
        tracing::error!(
            "embedding.provider=\"mlx\" has been removed — use \"llamacpp\" with a \
             .gguf `model_path`. NOTE: the two backends' vectors are not \
             interchangeable (~0.96-0.99 cosine), so stored episode embeddings \
             must be regenerated after switching; until then recall degrades to \
             BM25-only for those episodes."
        );
        return None;
    }

    // `api_key_env` was declared on EmbeddingConfig but never honored —
    // it wins over the provider-default var name, resolving through the
    // SAME credential chain as every other key (auth store, env_vars +
    // keychain, process env), so `octos auth login` / config-stored keys
    // keep working (codex P2).
    let key = config
        .get_api_key_with_env(&cfg.provider, cfg.api_key_env.as_deref())
        .ok()?;
    let mut e = OpenAIEmbedder::new(key);
    if let Some(ref url) = cfg.base_url {
        e = e.with_base_url(url);
    } else if !cfg.provider.eq_ignore_ascii_case("openai") {
        // A non-openai provider without an explicit base_url falls back to
        // the registry's default endpoint — otherwise the request goes to
        // api.openai.com with the other provider's key/model (codex R8).
        if let Some(url) =
            octos_llm::registry::lookup(&cfg.provider).and_then(|e| e.default_base_url)
        {
            e = e.with_base_url(url);
        }
    }
    if let Some(ref model) = cfg.model {
        e = e.with_model(model);
    }
    if let Some(dimensions) = cfg.dimensions {
        if dimensions as usize != octos_memory::EPISODIC_INDEX_DIMENSION {
            tracing::warn!(
                dimensions,
                index = octos_memory::EPISODIC_INDEX_DIMENSION,
                "embedding.dimensions differs from the episodic index dimension — \
                 vectors will be dropped to BM25-only"
            );
        }
        e = e.with_dimensions(dimensions);
    } else if let Some(model) = cfg.model.as_deref() {
        // Auto-pin to the index dimension ONLY for families known to
        // accept the OpenAI-standard `dimensions` field (OpenAI 3-series;
        // DashScope text-embedding-v3/v4, natively 1024-d). Models that
        // reject the field (ada-002) keep the legacy request shape; for
        // families we can't classify, warn loudly instead of degrading
        // silently — the native size is unknown and non-1536 vectors are
        // dropped to BM25-only.
        // Exactly the families verified to accept `dimensions: 1536`:
        // OpenAI 3-series (native 1536/3072, truncation supported) and
        // DashScope text-embedding-v4 (64–2048, verified live). v3 caps
        // below 1536 and would error — it falls to the warn path.
        let supports_dimensions =
            model.starts_with("text-embedding-3") || model == "text-embedding-v4";
        if supports_dimensions {
            tracing::info!(
                model = %e.model(),
                pinned = octos_memory::EPISODIC_INDEX_DIMENSION,
                "pinning custom embedding model to the episodic index dimension"
            );
            e = e.with_dimensions(octos_memory::EPISODIC_INDEX_DIMENSION as u32);
        } else {
            tracing::warn!(
                model = %e.model(),
                index = octos_memory::EPISODIC_INDEX_DIMENSION,
                "custom embedding model without `dimensions`: native size unknown — \
                 vectors that are not index-sized will be dropped to BM25-only; set \
                 embedding.dimensions if the provider supports it"
            );
        }
    }
    Some(Arc::new(e))
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    /// A profile-derived config carrying a full openai route.
    fn openai_route_config() -> Config {
        Config {
            provider: Some("openai".into()),
            model: Some("gpt-4o".into()),
            base_url: Some("https://fake.example/v1".into()),
            api_key_env: Some("MYFAKE_PROFILE_KEY".into()),
            api_type: Some("openai".into()),
            ..Default::default()
        }
    }

    #[test]
    fn explicit_provider_override_detaches_inherited_route() {
        // Switching to a DIFFERENT provider must drop the profile's route
        // siblings so the new provider's defaults / explicit flags apply —
        // otherwise `--profile p --provider anthropic` builds an incoherent
        // anthropic client still pointed at openai's key-env + endpoint.
        let mut config = openai_route_config();
        detach_route_on_provider_override(&mut config, Some("anthropic"));
        assert_eq!(config.base_url, None, "base_url must detach");
        assert_eq!(config.api_key_env, None, "api_key_env must detach");
        assert_eq!(config.api_type, None, "api_type must detach");
        // The model is intentionally left inherited (some providers require one;
        // blanking it would turn a swap into a hard error). Complete the switch
        // with `--model`.
        assert_eq!(config.model.as_deref(), Some("gpt-4o"));
    }

    // ---- yolo GAP #3: chat permission flags → EffectivePermissions ----

    use octos_agent::{ApprovalPolicy, PermissionProfile};

    #[test]
    fn should_default_to_workspace_write_ask_when_no_flags() {
        let perms =
            resolve_chat_permissions(false, None, None).expect("no flags is the plain default");
        assert_eq!(perms.permission_profile, PermissionProfile::WorkspaceWrite);
        assert_eq!(perms.approval_policy, ApprovalPolicy::Ask);
    }

    #[test]
    fn should_map_sandbox_read_only_and_danger_variants() {
        let ro = resolve_chat_permissions(false, Some(ChatSandboxMode::ReadOnly), None).unwrap();
        assert_eq!(ro.permission_profile, PermissionProfile::ReadOnly);

        // `--sandbox danger-full-access` is the long-form of `--yolo`.
        let danger =
            resolve_chat_permissions(false, Some(ChatSandboxMode::DangerFullAccess), None).unwrap();
        assert_eq!(
            danger.permission_profile,
            PermissionProfile::DangerFullAccess
        );
        assert!(danger.is_dangerous());
    }

    #[test]
    fn should_reject_conflicting_yolo_and_non_danger_sandbox() {
        // `--yolo` plus an explicit non-danger `--sandbox` is contradictory;
        // fail closed rather than silently pick one.
        let err = resolve_chat_permissions(true, Some(ChatSandboxMode::ReadOnly), None)
            .expect_err("conflicting flags must error");
        assert!(
            err.to_string().contains("sandbox"),
            "error should explain the sandbox conflict; got: {err}"
        );
    }

    #[test]
    fn should_map_chat_effort_to_reasoning_effort() {
        use octos_llm::ReasoningEffort;
        assert_eq!(
            ReasoningEffort::from(ChatEffort::None),
            ReasoningEffort::Disabled
        );
        assert_eq!(ReasoningEffort::from(ChatEffort::Low), ReasoningEffort::Low);
        assert_eq!(
            ReasoningEffort::from(ChatEffort::Medium),
            ReasoningEffort::Medium
        );
        assert_eq!(
            ReasoningEffort::from(ChatEffort::High),
            ReasoningEffort::High
        );
        assert_eq!(ReasoningEffort::from(ChatEffort::Max), ReasoningEffort::Max);
    }

    #[test]
    fn should_reconcile_positional_prompt_with_message() {
        // Positional-only → used as the one-shot message.
        assert_eq!(
            reconcile_one_shot_prompt(None, Some("hi".into())).unwrap(),
            Some("hi".to_string())
        );
        // --message-only → passthrough.
        assert_eq!(
            reconcile_one_shot_prompt(Some("hi".into()), None).unwrap(),
            Some("hi".to_string())
        );
        // Neither → interactive (None).
        assert_eq!(reconcile_one_shot_prompt(None, None).unwrap(), None);
        // Both → contradictory, fail closed.
        let err = reconcile_one_shot_prompt(Some("a".into()), Some("b".into()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not both"), "{err}");
    }

    #[test]
    #[cfg(feature = "api")]
    fn should_preserve_failed_turn_partial_in_json_without_claiming_success() {
        use crate::commands::oup_session::{OupTurnFailure, OupTurnResult};
        use octos_core::ui_protocol::{EnvelopeTokenUsage, TurnTerminalError};
        for text in ["actual partial\n\"quoted\"", "", "  "] {
            let error = eyre::Report::from(OupTurnFailure {
                terminal_error: Some(TurnTerminalError {
                    code: "output_truncated".into(),
                    message: "incomplete".into(),
                    data: None,
                }),
                partial: OupTurnResult {
                    text: text.into(),
                    model: None,
                    interrupted: false,
                    usage: EnvelopeTokenUsage {
                        input_tokens: 17,
                        output_tokens: 11,
                        reasoning_tokens: 5,
                        cache_read_tokens: 3,
                        cache_write_tokens: 2,
                    },
                },
            });
            let value = json_error_value(&error);
            assert_eq!(value["error"], "incomplete");
            assert_eq!(value["code"], "output_truncated");
            assert_eq!(value["usage"]["input_tokens"], 17);
            assert_eq!(value["usage"]["output_tokens"], 11);
            assert_eq!(value["usage"]["reasoning_tokens"], 5);
            assert_eq!(value["usage"]["cache_read_tokens"], 3);
            assert_eq!(value["usage"]["cache_write_tokens"], 2);
            assert!(
                value.get("text").is_none(),
                "not a successful result envelope"
            );
            if text.trim().is_empty() {
                assert!(value.get("partial").is_none(), "do not fabricate an answer");
            } else {
                assert_eq!(value["partial"]["text"], text);
                assert!(
                    value["partial"]["model"].is_null(),
                    "unknown model must stay unknown"
                );
            }
        }
        let generic = eyre::eyre!("configuration failed");
        assert_eq!(
            json_error_value(&generic),
            serde_json::json!({"error":"configuration failed"})
        );
    }

    fn q(multi: bool, allow_free_text: bool) -> octos_core::ui_protocol::UserQuestion {
        use octos_core::ui_protocol::{UserQuestion, UserQuestionOption};
        UserQuestion {
            header: "H".into(),
            question: "Which?".into(),
            options: vec![
                UserQuestionOption {
                    label: "axum".into(),
                    description: String::new(),
                },
                UserQuestionOption {
                    label: "actix".into(),
                    description: String::new(),
                },
                UserQuestionOption {
                    label: "warp".into(),
                    description: String::new(),
                },
            ],
            multi_select: multi,
            allow_free_text,
        }
    }

    #[test]
    fn parse_approval_answer_maps_y_s_and_default_deny() {
        assert_eq!(
            parse_cli_approval_answer("y\n"),
            CliApprovalAnswer::ApproveOnce
        );
        assert_eq!(
            parse_cli_approval_answer("  YES "),
            CliApprovalAnswer::ApproveOnce
        );
        assert_eq!(
            parse_cli_approval_answer("s"),
            CliApprovalAnswer::ApproveSession
        );
        assert_eq!(
            parse_cli_approval_answer("Session\n"),
            CliApprovalAnswer::ApproveSession
        );
        // Fail-closed: empty and anything unrecognized deny.
        assert_eq!(parse_cli_approval_answer(""), CliApprovalAnswer::Deny);
        assert_eq!(parse_cli_approval_answer("n"), CliApprovalAnswer::Deny);
        assert_eq!(parse_cli_approval_answer("wat"), CliApprovalAnswer::Deny);
    }

    #[tokio::test]
    async fn session_scope_auto_resolves_later_requests_without_prompting() {
        // With the session flag set, request_approval must return Approve on
        // the fast path — before spawn_blocking would ever touch stdin (this
        // test has no TTY; a prompt would deny, failing the assert).
        let requester = CliApprovalRequester::default();
        requester
            .session_approved
            .store(true, std::sync::atomic::Ordering::Release);
        let request = ToolApprovalRequest {
            tool_id: "t1".into(),
            tool_name: "shell".into(),
            title: "Approve command".into(),
            body: "Run command: sudo echo hi".into(),
            command: Some("sudo echo hi".into()),
            cwd: None,
        };
        let decision = requester.request_approval(request).await;
        assert_eq!(decision, ToolApprovalDecision::Approve);
    }

    #[test]
    fn parse_selection_multi_keeps_all_valid_and_drops_garbage() {
        let (labels, other) = parse_question_selection(&q(true, true), "1, 3, 9, x");
        assert_eq!(labels, vec!["axum", "warp"]); // 9 out-of-range, x non-numeric
        assert!(!other);
    }

    #[test]
    fn test_resolve_provider_policy_model_id_match() {
        let json = r#"{
            "tool_policy_by_provider": {
                "gemini": {"deny": ["diff_edit"]},
                "claude-sonnet-4-20250514": {"allow": ["shell"]}
            }
        }"#;
        let config: Config = serde_json::from_str(json).unwrap();
        let policy =
            resolve_provider_policy(&config, "anthropic", "claude-sonnet-4-20250514").unwrap();
        assert!(policy.is_allowed("shell"));
        assert!(!policy.is_allowed("read_file"));
    }
}

/// Create an LLM provider from name and config.
///
/// When `api_type` is `Some("anthropic")` (from config or sub-provider),
/// the Anthropic Messages API protocol is used regardless of provider name.
pub(crate) fn create_provider(
    name: &str,
    config: &Config,
    model: Option<String>,
    base_url: Option<String>,
) -> Result<Arc<dyn LlmProvider>> {
    let provider =
        create_provider_with_api_type(name, config, model, base_url, config.api_type.as_deref())?;
    eprintln!("{}: {}", "Model".green(), provider.model_id());
    Ok(provider)
}

/// Inner factory that accepts an explicit `api_type` override.
///
/// Does NOT print to stdout — callers that want a log line should print
/// after calling this function.
///
/// Exposed as `pub` (was `pub(crate)`) so the `octos-ffi` C-ABI crate can
/// reuse the exact provider-construction path (auth store → env_vars → env
/// key resolution, timeout overrides, anthropic/responses api_type bypasses)
/// instead of re-implementing a drift-prone parallel factory.
pub fn create_provider_with_api_type(
    name: &str,
    config: &Config,
    model: Option<String>,
    base_url: Option<String>,
    api_type: Option<&str>,
) -> Result<Arc<dyn LlmProvider>> {
    if name == "custom" {
        return create_custom_provider(config, model, base_url, api_type);
    }

    let entry = octos_llm::registry::lookup(name).ok_or_else(|| {
        eyre::eyre!(
            "unknown provider: {name}. Valid: {}",
            octos_llm::registry::all_names().join(", ")
        )
    })?;

    // Resolve API key via config (auth store → env var).
    let api_key = if entry.requires_api_key {
        Some(config.get_api_key(entry.name)?)
    } else {
        config.get_api_key(entry.name).ok()
    };

    if entry.requires_model && model.is_none() {
        eyre::bail!("{} provider requires --model to be specified", name);
    }
    if entry.requires_base_url && base_url.is_none() {
        eyre::bail!("{} provider requires --base-url to be specified", name);
    }

    // Extract timeout overrides from gateway config (if any).
    let llm_timeout_secs = config.gateway.as_ref().and_then(|g| g.llm_timeout_secs);
    let llm_connect_timeout_secs = config
        .gateway
        .as_ref()
        .and_then(|g| g.llm_connect_timeout_secs);

    // If api_type is "anthropic", bypass registry and use AnthropicProvider directly.
    // This allows any provider to use the Anthropic Messages API protocol.
    if api_type == Some("anthropic") {
        let key = api_key.ok_or_else(|| eyre::eyre!("API key required for anthropic api_type"))?;
        let m = model
            .or_else(|| entry.default_model().map(str::to_string))
            .ok_or_else(|| {
                eyre::eyre!(
                    "{}: no model given and the catalog declares no default for this family",
                    entry.name
                )
            })?;
        let url = base_url.unwrap_or_else(|| {
            entry
                .default_base_url
                .unwrap_or("https://api.anthropic.com")
                .into()
        });
        let mut provider =
            octos_llm::anthropic::AnthropicProvider::new(&key, &m).with_base_url(&url);
        if let Some(t) = llm_timeout_secs {
            let c = llm_connect_timeout_secs.unwrap_or(octos_llm::DEFAULT_LLM_CONNECT_TIMEOUT_SECS);
            provider = provider.with_http_timeout(t, c);
        }
        return Ok(Arc::new(provider));
    }

    // If api_type is "responses", use OpenAI Responses API directly.
    // This forces the Responses API even for models not auto-detected.
    if api_type == Some("responses") {
        let key = api_key.ok_or_else(|| eyre::eyre!("API key required for responses api_type"))?;
        let m = model
            .or_else(|| entry.default_model().map(str::to_string))
            .ok_or_else(|| {
                eyre::eyre!(
                    "{}: no model given and the catalog declares no default for this family",
                    entry.name
                )
            })?;
        let mut provider = octos_llm::openai_responses::OpenAIResponsesProvider::new(&key, &m);
        if let Some(url) = base_url {
            provider = provider.with_base_url(&url);
        }
        if let Some(t) = llm_timeout_secs {
            let c = llm_connect_timeout_secs.unwrap_or(octos_llm::DEFAULT_LLM_CONNECT_TIMEOUT_SECS);
            provider = provider.with_http_timeout(t, c);
        }
        return Ok(Arc::new(provider));
    }

    let params = octos_llm::registry::CreateParams {
        api_key,
        model,
        base_url,
        model_hints: config.model_hints.clone(),
        llm_timeout_secs,
        llm_connect_timeout_secs,
    };

    let provider = (entry.create)(params)?;
    Ok(provider)
}

fn create_custom_provider(
    config: &Config,
    model: Option<String>,
    base_url: Option<String>,
    api_type: Option<&str>,
) -> Result<Arc<dyn LlmProvider>> {
    let key = config.get_api_key("custom")?;
    let model = model.ok_or_else(|| eyre::eyre!("custom provider requires model"))?;
    let base_url = base_url.ok_or_else(|| eyre::eyre!("custom provider requires base_url"))?;

    // Extract timeout overrides from gateway config (if any).
    let llm_timeout_secs = config.gateway.as_ref().and_then(|g| g.llm_timeout_secs);
    let llm_connect_timeout_secs = config
        .gateway
        .as_ref()
        .and_then(|g| g.llm_connect_timeout_secs);

    match api_type.unwrap_or("openai") {
        "openai" => {
            let mut provider = octos_llm::openai::OpenAIProvider::new(key, model)
                .with_base_url(&base_url)
                .with_provider_label("custom");
            if let Some(t) = llm_timeout_secs {
                let c =
                    llm_connect_timeout_secs.unwrap_or(octos_llm::DEFAULT_LLM_CONNECT_TIMEOUT_SECS);
                provider = provider.with_http_timeout(t, c);
            }
            Ok(Arc::new(provider))
        }
        "anthropic" => {
            let mut provider = octos_llm::anthropic::AnthropicProvider::new(key, model)
                .with_base_url(&base_url)
                // #2194: label stays "custom" (its logical identity for
                // adaptive-lane / QoS matching); the Anthropic cache rate is
                // carried by ProviderMetadata::cache_lane, set from the provider
                // TYPE in AnthropicProvider::provider_metadata().
                .with_provider_label("custom");
            if let Some(t) = llm_timeout_secs {
                let c =
                    llm_connect_timeout_secs.unwrap_or(octos_llm::DEFAULT_LLM_CONNECT_TIMEOUT_SECS);
                provider = provider.with_http_timeout(t, c);
            }
            Ok(Arc::new(provider))
        }
        other => eyre::bail!("unsupported custom api_type '{other}'; use openai or anthropic"),
    }
}

#[cfg(test)]
mod custom_provider_tests {
    use super::*;

    fn custom_config() -> Config {
        let mut config = Config {
            api_key_env: Some("CUSTOM_API_KEY".to_string()),
            ..Default::default()
        };
        config
            .env_vars
            .insert("CUSTOM_API_KEY".to_string(), "test-key".to_string());
        config
    }

    #[test]
    fn creates_custom_openai_compatible_provider() {
        let provider = create_provider_with_api_type(
            "custom",
            &custom_config(),
            Some("llama-3.1-70b-instruct".to_string()),
            Some("http://127.0.0.1:11434/v1".to_string()),
            Some("openai"),
        )
        .unwrap();

        assert_eq!(provider.provider_name(), "custom");
        assert_eq!(provider.model_id(), "llama-3.1-70b-instruct");
        // #2194 R4: a custom OpenAI endpoint keeps the "custom" identity AND the
        // residual cache lane (full-rate reads) — NOT the Anthropic 0.1x bucket.
        let meta = provider.provider_metadata();
        assert_eq!(meta.cache_lane, octos_llm::CacheLane::Residual);
        assert_eq!(
            octos_llm::pricing::cache_rates_for_lane(meta.cache_lane).read_multiplier,
            1.0,
            "custom + api_type=openai must price cache reads at the residual rate",
        );
    }

    #[test]
    fn rejects_custom_provider_without_base_url() {
        let result = create_provider_with_api_type(
            "custom",
            &custom_config(),
            Some("llama".to_string()),
            None,
            Some("openai"),
        );

        match result {
            Ok(provider) => panic!(
                "expected missing base_url error, got provider {}",
                provider.provider_name()
            ),
            Err(err) => assert!(err.to_string().contains("requires base_url")),
        }
    }
}
