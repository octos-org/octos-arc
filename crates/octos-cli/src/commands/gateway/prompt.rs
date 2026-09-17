//! System prompt construction for gateway mode.

use std::path::Path;

use octos_agent::SkillsLoader;

/// Build the system prompt with bootstrap files, memory context, and skills.
///
/// `max_inject_tokens` caps the injected memory block (long-term memory +
/// daily notes + bank summary combined); use
/// [`crate::config::MemoryConfig::effective_max_inject_tokens`] to resolve it
/// from config.
// These are the cohesive set of bootstrap inputs a system prompt is composed
// from (persona base, data/project dirs, memory + skills sources, and the two
// resolved memory knobs); they don't group into a smaller, meaningful sub-type,
// so the arg count is expected here.
#[allow(clippy::too_many_arguments)]
/// The base system prompt split at the memory slot. Memory is injected as
/// a NAMED per-agent prompt segment BETWEEN these halves (the pre-refactor
/// order was `…bootstrap/soul → memory → skills/tool prefs`; keeping the
/// slot preserves prompt precedence — persisted user memory must not
/// override the skill/tool guidance that always followed it).
#[derive(Debug, Clone, Default)]
pub struct GatewayPromptParts {
    /// Everything before the memory slot (base, date, platform, persona,
    /// bootstrap files, soul).
    pub pre_memory: String,
    /// Everything after it (active skills, skills summary, tool prefs).
    pub post_memory: String,
}

impl GatewayPromptParts {
    /// The joined prompt WITHOUT a memory block — for read-only consumers
    /// (length logging, tests) that don't build agents.
    pub fn joined(&self) -> String {
        let mut out = self.pre_memory.clone();
        out.push_str(&self.post_memory);
        out
    }
}

pub async fn build_system_prompt(
    base: Option<&str>,
    // Retained for signature stability with the persona/soul injection the
    // slimming removed; the base prompt no longer reads the data dir.
    _data_dir: &Path,
    project_dir: &Path,
    skills_loader: &SkillsLoader,
) -> GatewayPromptParts {
    let compiled = include_str!("../../prompts/gateway_default.txt");
    let runtime = super::super::load_prompt("gateway", compiled);
    let mut prompt = base.unwrap_or(&runtime).to_string();

    // Inject current date so the model knows "今年" = which year
    let today = chrono::Local::now().format("%Y-%m-%d");
    prompt.push_str(&format!("\n\nCurrent date: {today}"));

    // Inject platform guidance so the model doesn't emit Unix-only shell
    // commands on Windows hosts.
    #[cfg(windows)]
    {
        prompt.push_str(
            "\n\n## Runtime Platform\n\n\
             Current host OS: Windows.\n\
             If you use the shell tool, write Windows cmd.exe-compatible commands only.\n\
             Do NOT use Unix-only commands like `ps`, `grep`, `head`, `rm`, `ls`, `cat`, `which`, or `bash`.\n\
             Prefer built-in tools (`glob`, `grep`, `list_dir`, `read_file`) over shell whenever possible.\n\
             If a task depends on a tool or binary that is not available on this host, say so explicitly and do not retry via shell.",
        );
    }

    // Append bootstrap files (AGENTS.md, SOUL.md, USER.md, etc.)
    let bootstrap = super::super::load_bootstrap_files(project_dir);
    if !bootstrap.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(&bootstrap);
    }

    // ---- memory slot ----------------------------------------------------
    // Memory is NOT inlined here anymore: every model call flows through a
    // per-session `octos_agent::Agent`, which owns the memory as a named
    // prompt segment refreshed at each turn start (chat.rs pattern —
    // fingerprint stat per turn). Inlining it in this base String froze it
    // at build time. The split preserves the slot's POSITION.
    let pre_memory = std::mem::take(&mut prompt);

    // Append always-on skills
    if let Ok(always_names) = skills_loader.get_always_skills().await {
        if !always_names.is_empty() {
            if let Ok(skills_content) = skills_loader.load_skills_for_context(&always_names).await {
                if !skills_content.is_empty() {
                    prompt.push_str("\n\n## Active Skills\n\n");
                    prompt.push_str(&skills_content);
                }
            }
        }
    }

    // Append skills summary
    if let Ok(summary) = skills_loader.build_skills_summary().await {
        if !summary.is_empty() {
            prompt.push_str("\n\n## Available Skills\n\n");
            prompt.push_str(&summary);
        }
    }

    GatewayPromptParts {
        pre_memory,
        post_memory: prompt,
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the compiled-in gateway (base coding) system prompt at
    //! `crates/octos-cli/src/prompts/gateway_default.txt`, loaded via
    //! `include_str!` in [`build_system_prompt`]. The prompt is a lean
    //! CODING base: identity, tool discipline, verification, subagents,
    //! memory, and communication rules. Assertions are substring-based
    //! so the prompt can be edited around the rules without breaking
    //! the tests — but the load-bearing pieces must stay.

    const PROMPT: &str = include_str!("../../prompts/gateway_default.txt");

    #[test]
    fn should_identity_as_coding_agent() {
        assert!(
            PROMPT.contains("software engineering agent"),
            "prompt must open with the coding-agent identity"
        );
        assert!(
            PROMPT.contains("workspace"),
            "prompt must anchor the agent in the user's workspace"
        );
    }

    #[test]
    fn should_require_read_before_write() {
        assert!(
            PROMPT.contains("READ BEFORE WRITE"),
            "prompt must carry the read-before-write rule"
        );
        for tool in ["read_file", "grep", "glob", "list_dir"] {
            assert!(
                PROMPT.contains(&format!("`{tool}`")),
                "prompt must name the precise tool `{tool}`"
            );
        }
    }

    #[test]
    fn should_require_check_and_honest_verification() {
        assert!(
            PROMPT.contains("`check`"),
            "prompt must tell the model to run `check` after changes"
        );
        assert!(
            PROMPT.contains("NEVER invent, simulate"),
            "prompt must forbid inventing or simulating command results"
        );
        assert!(
            PROMPT.contains("update_plan"),
            "prompt must mention `update_plan` for multi-step work"
        );
        assert!(
            PROMPT.contains("ask_user_question"),
            "prompt must mention `ask_user_question` when blocked"
        );
    }

    #[test]
    fn should_describe_spawn_and_memory() {
        assert!(
            PROMPT.contains("`spawn`"),
            "prompt must describe subagent use via `spawn`"
        );
        assert!(
            PROMPT.contains("save_memory"),
            "prompt must instruct proactive `save_memory` usage"
        );
    }

    #[test]
    fn should_not_reference_removed_media_or_search_tools() {
        for gone in [
            "news_fetch",
            "get_weather",
            "get_time",
            "podcast_generate",
            "podcast_voices",
            "voice_synthesize",
            "comic_generate",
            "fm_tts",
            "web_search",
            "web_fetch",
            "deep_crawl",
            "synthesize_research",
            "send_app_card",
            "configure_tool",
            "search menu",
            "1/2/3",
        ] {
            assert!(
                !PROMPT.contains(gone),
                "base coding prompt must not reference the removed tool or rule {gone:?}"
            );
        }
    }
}
