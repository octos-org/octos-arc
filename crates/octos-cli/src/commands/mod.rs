//! CLI commands for octos.

pub mod auth;
mod cache;
pub mod chat;
mod clean;
mod completions;
mod config;
pub mod gateway;

#[cfg(feature = "api")]
pub mod agent_factory;
mod init;
#[cfg(feature = "api")]
pub(crate) mod oup_client;
#[cfg(feature = "api")]
#[cfg(feature = "api")]
pub(crate) mod oup_session;
#[cfg(feature = "api")]
mod oup_text;
#[cfg_attr(test, allow(unused_imports))]
#[cfg(feature = "api")]
mod serve;
pub mod serve_console;
pub mod skills;
pub use std::path::PathBuf;

use clap::{Parser, Subcommand};
use eyre::Result;

pub use auth::AuthCommand;
pub use cache::CacheCommand;
pub use chat::ChatCommand;
pub use clean::CleanCommand;
pub use completions::CompletionsCommand;
pub use config::ConfigCommand;
pub use octos_arc::ArcCommand;

pub use init::InitCommand;
#[cfg_attr(not(test), allow(unused_imports))]
#[cfg(feature = "api")]
pub use serve::ServeCommand;
pub use skills::SkillsCommand;

/// octos: Rust-native coding agent orchestration.
#[derive(Debug, Parser)]
#[command(name = "octos")]
#[command(author, about, long_about = None)]
#[command(version = version_string())]
pub struct Args {
    #[command(subcommand)]
    pub command: Command,
}

/// Build a version string like "0.1.0 (abc1234 2026-03-02)".
fn version_string() -> &'static str {
    const VERSION: &str = env!("CARGO_PKG_VERSION");
    const GIT_HASH: &str = match option_env!("OCTOS_GIT_HASH") {
        Some(v) => v,
        None => "",
    };
    const BUILD_DATE: &str = match option_env!("OCTOS_BUILD_DATE") {
        Some(v) => v,
        None => "",
    };

    // Leak a formatted string so we get a &'static str for clap
    #[allow(clippy::const_is_empty)]
    if GIT_HASH.is_empty() {
        VERSION
    } else {
        Box::leak(format!("{VERSION} ({GIT_HASH} {BUILD_DATE})").into_boxed_str())
    }
}

/// Available commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Manage authentication for LLM providers.
    Auth(AuthCommand),
    /// Interactive multi-turn chat with an agent.
    Chat(ChatCommand),
    Arc(ArcCommand),
    /// Inspect and reclaim the build-cache pool (`status` / `gc` / `gate`).
    Cache(CacheCommand),
    /// Inspect the saved startup config (`show` / `path`); read-only.
    Config(ConfigCommand),
    /// Initialize a new .octos configuration.
    Init(InitCommand),
    /// Start the REST API server (requires --features api).
    #[cfg(feature = "api")]
    Serve(ServeCommand),

    /// Clean up stale state and cache files.
    Clean(CleanCommand),
    /// Generate shell completions.
    Completions(CompletionsCommand),
    /// Manage agent skills (list, install, remove).
    Skills(SkillsCommand),
}

/// Whether stdout is reserved for protocol or assistant output, so tracing
/// must use stderr instead of interleaving log lines with that output.
///
/// * `acp` speaks ACP JSON-RPC on stdout (one stray log line → a `-32700`
///   parse error at strict clients like Zed);
/// * `profile` emits payloads meant for `$(...)` capture / piping;
/// * `chat` streams assistant text (or one `--json` result) on stdout.
///
/// Every other command keeps its historical stdout console routing untouched.
pub fn reserve_stdout(command: &Command) -> bool {
    match command {
        Command::Chat(_) | Command::Arc(_) => true,
        // `octos cache <sub> --json` emits a machine-readable object meant
        // for scripting / the outer loop's gate check. Without `--json` the
        // human table stays on the historical stdout routing.
        Command::Cache(cmd) => cmd.emits_json(),
        _ => false,
    }
}

/// Trait for executable commands (following dora-rs pattern).
pub trait Executable {
    fn execute(self) -> Result<()>;
}

/// Resolve the data directory for episodes, memory, sessions, etc.
///
/// Priority: `--data-dir` CLI flag > `OCTOS_HOME` env var > `~/.octos` default.
/// Delegates to the canonical [`resolve_config_context`] so the `data_dir`
/// computation (including empty-string env handling) never diverges from
/// config/auth resolution.
pub fn resolve_data_dir(cli_override: Option<PathBuf>) -> eyre::Result<PathBuf> {
    let ctx = crate::config_context::resolve_config_context(cli_override.as_deref());
    std::fs::create_dir_all(&ctx.data_dir).ok();
    Ok(ctx.data_dir)
}

/// Resolve the canonical [`ConfigContext`](crate::config_context::ConfigContext)
/// for a command, create the data dir, and run the (idempotent, best-effort)
/// config + auth migrations exactly once.
///
/// Every command that loads config should go through this so the resolver runs
/// at a single shared entrypoint per invocation.
pub fn resolve_command_context(
    cli_override: Option<PathBuf>,
) -> eyre::Result<crate::config_context::ConfigContext> {
    let ctx = crate::config_context::resolve_config_context(cli_override.as_deref());
    std::fs::create_dir_all(&ctx.data_dir).ok();
    crate::config_context::run_migrations(&ctx);
    Ok(ctx)
}

/// Load a prompt from `~/.octos/prompts/{name}.md` at runtime.
/// Falls back to `compiled_default` if the file doesn't exist or is empty.
pub(crate) fn load_prompt(name: &str, compiled_default: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let path = home.join(".octos/prompts").join(format!("{name}.md"));
        if let Ok(content) = std::fs::read_to_string(&path) {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    compiled_default.to_string()
}

/// Load optional bootstrap/personality files from the .octos/ directory.
/// Used by both chat and gateway to build the system prompt from AGENTS.md, SOUL.md, etc.
pub(crate) fn load_bootstrap_files(data_dir: &std::path::Path) -> String {
    const FILES: &[&str] = &["AGENTS.md", "SOUL.md", "USER.md", "TOOLS.md", "IDENTITY.md"];
    let mut parts = Vec::new();
    for filename in FILES {
        let path = data_dir.join(filename);
        if let Ok(content) = std::fs::read_to_string(&path) {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                parts.push(format!("## {filename}\n\n{trimmed}"));
            }
        }
    }
    parts.join("\n\n")
}

/// M8.3: load a profile's `system_prompt_template` hint.
///
/// The path is treated as relative to `~/.octos/profiles/<profile_name>/`.
/// Missing files are not an error — we log and return `None` so the agent
/// keeps its default prompt. Empty files are also treated as missing.
pub(crate) fn load_profile_prompt_template(
    profile_name: &str,
    template_rel: &std::path::Path,
) -> Option<String> {
    let home = dirs::home_dir()?;
    let base = home.join(".octos/profiles").join(profile_name);
    let path = base.join(template_rel);
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                tracing::warn!(
                    path = %path.display(),
                    "profile system_prompt_template exists but is empty; using default prompt"
                );
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "profile system_prompt_template not found; using default prompt"
            );
            None
        }
    }
}

impl Executable for Command {
    fn execute(self) -> Result<()> {
        match self {
            Self::Auth(cmd) => cmd.execute(),
            Self::Skills(cmd) => cmd.execute(),
            Self::Chat(cmd) => cmd.execute(),
            Self::Arc(cmd) => {
                eyre::ensure!(
                    cfg!(feature = "api") || cmd.prepare_only,
                    "ARC coding requires an Octos build with the api feature"
                );
                let executable = std::env::current_exe()?;
                let result = octos_arc::execute(
                    cmd,
                    octos_arc::pin::BinaryIdentity {
                        executable: &executable,
                        source_commit: env!("OCTOS_GIT_SHA"),
                        target: env!("OCTOS_BUILD_TARGET"),
                        dirty: env!("OCTOS_SOURCE_DIRTY") != "false",
                    },
                )?;
                println!("{}", serde_json::to_string_pretty(&result)?);
                Ok(())
            }
            Self::Cache(cmd) => cmd.execute(),
            Self::Config(cmd) => cmd.execute(),
            Self::Init(cmd) => cmd.execute(),
            #[cfg(feature = "api")]
            Self::Serve(cmd) => cmd.execute(),
            Self::Clean(cmd) => cmd.execute(),
            Self::Completions(cmd) => cmd.execute(),
        }
    }
}

#[cfg(test)]
mod reserve_stdout_tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn should_reserve_stdout_when_chat_json_set() {
        // `octos chat --json` opts into a pure-stdout JSON result stream, so
        // its tracing logs must route to stderr.
        let args = Args::try_parse_from(["octos", "chat", "--json", "--message", "hi"])
            .expect("`chat --json` must parse");
        assert!(reserve_stdout(&args.command));
    }

    #[test]
    fn should_reserve_stdout_for_plain_chat_streaming() {
        // Shared OUP bootstrap/progress logs must not split assistant text.
        let args =
            Args::try_parse_from(["octos", "chat", "--message", "hi"]).expect("`chat` must parse");
        assert!(reserve_stdout(&args.command));
    }
}
