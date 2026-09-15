# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Downstream ARC build

This is the ARC competition downstream of `octos-org/octos` (baseline `arc-base-20260910`,
see ARC_BASELINE.md). The workspace is slimmed to the coding runtime: no bundled
app-skills/platform-skills, no server/FFI/wasm/embedding crates, no web/browser/media
tools, no channel integrations. The competition channels are `arc/` (platform submission
adapter) and `crates/octos-arc` (native `octos arc` workflow, which drives the same
binary's `octos chat --profile coding`).

## Build & Test Commands

```bash
cargo build --workspace          # Build all crates
cargo test --workspace           # Run all tests
cargo test -p octos-agent        # Test single crate
cargo test -p octos-agent test_name  # Run single test
cargo clippy --workspace         # Lint
cargo fmt --all                  # Format
cargo fmt --all -- --check       # Check formatting
cargo install --path crates/octos-cli --features "api"
                                 # Install CLI locally. `api` is the only
                                 # feature and is on by default; it gates
                                 # `octos serve`. There are no
                                 # telegram/discord/email/audio features.
```

## Architecture

octos is a Rust-native agentic coding runtime. 7-crate workspace, layered:

```
octos-cli  (CLI: clap commands, config loading, api/serve, session actor)
    |
octos-agent  (Agent loop, tool system, sandbox, MCP, compaction, skills loader)
    |          \
octos-memory   octos-llm  (hybrid search + memory store | LLM providers)
    \           /
    octos-core  (Task, Message, Error types, truncate_utf8 - no internal deps)
```

Alongside octos-agent:
- **octos-bus**: session persistence + file-handle resolution (cron service and resume policy were removed in round 5)
- **octos-arc**: the `octos arc` competition workflow (drives `octos chat`)

Skills are pure SKILL.md prompt injections (`octos-agent/src/skills.rs`: `SkillsLoader`
parses frontmatter + body, builds an XML summary for the system prompt and injects
marked content). The binary plugin protocol (`octos-plugin` crate + plugin tools/
skill actions/multi-user onboarding) was retired in the round-4 slimming; `octos
skills install` is file-copy + git only.

Commands: chat, arc, cache (status/gc/gate), config (show/path), serve (`#[cfg(feature = "api")]`, stdio-only), skills (list/install/remove), auth (login/logout/status). (init/clean/completions retired in round 5.)

Runtime modes:
- `octos chat` — interactive coding session (the core loop)
- `octos serve --stdio --solo` — NDJSON JSON-RPC transport over stdin/stdout; this is the
  ARC bench driver (one process per session, no HTTP)
- `octos serve` — stdio-only since round 5. The HTTP/WS server, REST surface,
  `/metrics`, session ingress, and the web-only protocol commands (M12 auxiliary,
  monitor/loop, task-control REST, agent-panel notifications) were all removed;
  the dispatcher itself is shared by both `octos serve --stdio --solo` and `octos chat`
  (in-process duplex), so one code path serves both.
- `octos arc` — competition workflow that drives `octos chat` end to end

Auth module (`octos-cli/src/auth/`): OAuth PKCE + device code for OpenAI, paste-token for others. Stored in `~/.octos/auth.json`. `config.rs` checks auth store before env vars.

### Key Flow: Agent Loop (`octos-agent/src/agent.rs`)

1. Build messages (system prompt + conversation history + memory context)
2. Call LLM with tool specs (filtered by ToolPolicy + provider policy)
3. If tool calls returned -> execute tools -> append results -> loop
4. If EndTurn or budget exceeded -> return result
5. Context compaction kicks in when token budget fills (`compaction.rs`)

### Tool System (`octos-agent/src/tools/`)

All tools implement `Tool` trait (`spec() -> ToolSpec`, `execute(&Value) -> ToolResult`). Registered in `ToolRegistry` (HashMap). Coding-core tools only: shell/exec_command/bash/write_stdin, read_file/write_file/edit_file/diff_edit/apply_patch, glob/grep/list_dir, check, view_image, tool_search/tool_suggest, update_plan, the spawn family (spawn/spawn_agent/send_input/resume_agent/wait_agent/close_agent + `delegate` alias wrapping spawn_agent+wait_agent), configure_tool, plus serve-registered send_file/read_task_output/check_background_tasks. (cron, delegate_task, request_user_input were removed in round 5.) `git` (gix) and `ast` (tree-sitter) tools are feature-gated. There are NO web/browser/research/media tools. Tool argument size limit: 1MB (non-allocating `estimate_json_size` with escape accounting). File tools use `O_NOFOLLOW` (Unix) for symlink-safe I/O.

**Tool Policies** (`tools/policy.rs`): Allow/deny lists with deny-wins semantics, wildcard matching (`exec*`), and named groups: `group:fs` (file tools), `group:runtime` (shell entry points), `group:search` (glob/grep/list_dir), `group:sessions` (spawn family + delegate), `group:memory` (the 4 memory tools), `group:admin` (configure_tool), `group:delegated` was removed with delegate_task in round 5. Provider-specific policies via `tools.byProvider` in config.

### Sandbox (`octos-agent/src/sandbox/`)

Five sandbox backends: `Bwrap` (Linux), `Landlock` (Linux, octos-sandbox helper), `Macos` (sandbox-exec), `AppContainer` (Windows, octos-sandbox.exe helper), `Docker` (any OS). Resolution is a pure decision layer (`decide_sandbox` over `HostOs` + `HostBackendProbe` — every platform's matrix unit-tested from any host): an explicit mode that cannot be honored on this host FAILS CLOSED with a typed `SandboxUnavailable` refusal (`RefusingSandbox` — every command refuses with per-OS remediation; never a silent `NoSandbox`, never a blind ENOENT backend). `SandboxMode::Auto` picks the best available backend; with none it degrades to `NoSandbox` loudly (warned once per process) unless `sandbox.fail_closed = true` (default false) turns the degradation into a refusal. `enabled = false` / `mode = "none"` remain the explicit unconfined opt-outs and beat `fail_closed`. Shared `BLOCKED_ENV_VARS` constant (18 env vars) across all backends and MCP server spawning. Docker supports mount modes (none/ro/rw), resource limits (CPU/memory/PIDs), network isolation. Path validation rejects injection characters (`:`, `\0`, `\n`, `\r` for Docker; control chars, `(`, `)`, `\`, `"` for macOS SBPL).

### MCP (`octos-agent/src/mcp.rs`)

JSON-RPC stdio transport for MCP servers. Env var sanitization via shared `BLOCKED_ENV_VARS`. Input schema validation: max depth 10, max size 64KB — tools with invalid schemas are rejected at registration.

### Context Compaction (`octos-agent/src/compaction.rs`)

Token-aware message compaction: estimates tokens, strips tool arguments, summarizes to first lines, preserves recent tool call/result pairs.

### LLM Providers (`octos-llm/src/`)

`LlmProvider` trait with `chat()` method. Native providers: `AnthropicProvider`, `OpenAIProvider`, `GeminiProvider` (+ `OpenAIResponsesProvider` for Responses-capable models). 7 registry families since round 5: anthropic, openai, gemini, deepseek, zai, zai-coding, moonshot-coding — `model_catalog.json` is the SSOT for model names/defaults (trimmed to these families). Failover is two layers: `RetryProvider` (exponential backoff on 429/5xx) → `ProviderChain` (the FallbackProvider third layer, Vertex-AI auth for gemini, adaptive hedge racing, lane scoring, the local-server family, credential_pool, and discovery were removed in rounds 5/6).

### Skills (`octos-agent/src/skills.rs`)

SKILL.md frontmatter + body; `SkillsLoader` scans `<data_dir>/skills/` (+ builtin),
filters via `SkillFilter`, injects the XML summary + `always: true` content into the
system prompt. The binary plugin protocol (manifest.json executables, skill actions,
spawn_only plugin tools) was retired in round 4 — the spawn family (spawn/spawn_agent
etc., `group:sessions`) remains the one background-subagent mechanism.

### Memory (`octos-memory/src/`)

- `EpisodeStore`: redb database at `.octos/episodes.redb`, stores task completion summaries
- `MemoryStore`: Long-term memory (MEMORY.md), daily notes, recent memories (7-day window)
- `HybridSearch`: BM25 + vector (cosine similarity) hybrid ranking with HNSW index (`hnsw_rs`). Configurable weights via `with_weights()` (default 0.7 vector / 0.3 BM25). Named HNSW constants. BM25 epsilon prevents NaN. Falls back to BM25-only without embedding provider.

### Session Management (`octos-bus/src/session.rs`)

JSONL persistence with LRU in-memory cache. Session forking (`/new` command) with parent_key tracking. Percent-encoded filenames with hash suffix on truncation (prevents collisions). File size limit: 10MB. Atomic write-then-rename for crash safety.

### Hooks (`octos-agent/src/hooks.rs`)

Lifecycle hook system for running shell commands at agent events. 4 events: `before_tool_call`, `after_tool_call`, `before_llm_call`, `after_llm_call`. Before-hooks can deny operations (exit code 1). Shell protocol: JSON payload on stdin, exit code semantics (0=allow, 1=deny, 2+=error). Circuit breaker auto-disables hooks after 3 consecutive failures (configurable via `HookExecutor::with_threshold()`). Commands use argv array (no shell interpretation). Environment sanitized via shared `BLOCKED_ENV_VARS`. Tilde expansion supports `~/` and `~username/`. Config: `hooks` array in config.json with `event`, `command`, `timeout_ms` (default 5000), `tool_filter`. Hook changes require a process restart.

## Key Types

- `Task` (octos-core): UUID v7 ID, kind (Code/Plan/Review/Custom), status, context
- `Message` (octos-core): role (System/User/Assistant/Tool), content, tool_call_id. `MessageRole` has `as_str()` and `Display` impl.
- `ChatResponse` (octos-llm): content, tool_calls, stop_reason, token usage
- `AgentConfig` (octos-agent): max_iterations (default 0 = unlimited for interactive chat; unattended session actors fall back to `UNATTENDED_MAX_ITERATIONS_FALLBACK` = 50 when unset), max_tokens, save_episodes
- `truncate_utf8`/`truncated_utf8` (octos-core): Shared UTF-8 safe string truncation (in-place and copying variants)

## TDD - Test Driven Development

All code changes follow the RED -> GREEN -> REFACTOR cycle.

- **New features/bug fixes**: Write a failing test first, then implement
- **Unit tests**: Inline `#[cfg(test)]` modules in the same file
- **Integration tests**: `crates/*/tests/` directory, `#[ignore]` for tests needing external services
- **Verify**: `cargo test -p <crate> <test_name>` after each step, full suite before done
- **Naming**: `should_<expected>_when_<condition>`

## Project Conventions

- Edition 2024, rust-version 1.85.0
- Pure Rust TLS via rustls (no OpenSSL dependency)
- `eyre`/`color-eyre` for error handling (not `anyhow`)
- `Arc<dyn Trait>` for shared providers/tools/reporters
- `AtomicBool` for shutdown signaling (Release on store, Acquire on load)
- API keys from env vars via `api_key_env` or OAuth via `octos auth login`
- `ShellTool` has `SafePolicy` that denies dangerous commands (rm -rf /, dd, mkfs, fork bomb). Whitespace-normalized before matching. Timeout clamped to [1, 600]s.
- `BLOCKED_ENV_VARS` shared across sandbox backends, MCP, and hooks (18 vars: LD_PRELOAD, DYLD_*, NODE_OPTIONS, etc.)
- Shared SSRF protection (`tools/ssrf.rs`): blocks private IPs, IPv6 ULA/link-local, IPv4-mapped/compatible addresses
- Symlink-safe file I/O via `O_NOFOLLOW` on Unix (eliminates TOCTOU races); symlink-check fallback on Windows
- Cross-platform: shell via `cmd /C` on Windows, `sh -c` on Unix; process kill via `taskkill` on Windows, `kill` signals on Unix; `where` on Windows, `which` on Unix for binary discovery
- `deny(unsafe_code)` workspace-wide lint
- `octos serve` is stdio-only (no HTTP listener); each stdio instance shares `state_home` for config-like state
