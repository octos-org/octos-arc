//! Model calls for the harness (`arc/llm_proxy.py` folded into the
//! provider configuration): non-streaming requests, reasoning control per
//! turn, a `max_tokens` floor, transient-error retries and a per-request
//! usage ledger in `.arc/llm-usage.jsonl` (same fields the Python proxy
//! wrote, so `arc/metrics.py` reads both).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eyre::{Result, WrapErr, bail, ensure};
use octos_core::Message;
use octos_llm::openai::OpenAIProvider;
use octos_llm::{ChatConfig, LlmProvider, ReasoningEffort, StopReason, TokenUsage};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::policy::ReasoningPolicy;

/// Where the model lives (from the runner spec; the key stays in the environment).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRoute {
    /// Provider family: openai-compatible endpoints only for now.
    #[serde(default = "default_provider")]
    pub provider: String,
    pub model: String,
    pub base_url: String,
    #[serde(default = "default_key_env")]
    pub api_key_env: String,
}

fn default_provider() -> String {
    "openai".into()
}

fn default_key_env() -> String {
    "OPENAI_API_KEY".into()
}

/// `OCTOS_ARC_REASONING` levels (`llm_proxy.inject_reasoning`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningMode {
    /// Leave the request untouched.
    Passthrough,
    /// `thinking: disabled` on DeepSeek-style endpoints.
    Disabled,
    Low,
    Medium,
    High,
}

impl ReasoningMode {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "passthrough" => Some(Self::Passthrough),
            "none" | "off" | "disabled" => Some(Self::Disabled),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Passthrough => "passthrough",
            Self::Disabled => "none",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    pub fn effort(self) -> Option<ReasoningEffort> {
        match self {
            Self::Passthrough => None,
            Self::Disabled => Some(ReasoningEffort::Disabled),
            Self::Low => Some(ReasoningEffort::Low),
            Self::Medium => Some(ReasoningEffort::Medium),
            Self::High => Some(ReasoningEffort::High),
        }
    }
}

pub struct CompletionRequest<'a> {
    pub label: &'a str,
    pub system: &'a str,
    pub user: &'a str,
    pub mode: ReasoningMode,
    pub timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct Completion {
    pub text: String,
    /// The reply hit `max_tokens`.
    pub truncated: bool,
    pub usage: TokenUsage,
    pub elapsed_ms: u64,
    pub attempts: u32,
}

pub trait Completer {
    fn complete(&mut self, request: &CompletionRequest<'_>) -> Result<Completion>;

    /// Wait for the endpoint to answer (the platform proxy has outages);
    /// returns log lines.
    fn probe(&mut self, _patience: Duration) -> Vec<String> {
        Vec::new()
    }
}

/// The ledger is shared by the in-process client and the kernel-session
/// driver so `[usage] provider totals` covers both turn shapes.
pub type SharedLedger = std::sync::Arc<std::sync::Mutex<UsageLedger>>;

fn http_statuses(text: &str) -> Vec<String> {
    static HTTP_STATUS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\bhttp(?:/\d(?:\.\d)?)?\s+(\d{3})\b").unwrap()
    });
    HTTP_STATUS
        .captures_iter(text)
        .map(|c| c[1].to_string())
        .collect()
}

/// Account rejection cannot be repaired by generating different application code.
pub fn is_permanent_provider_error(text: &str) -> bool {
    let lowered = text.to_lowercase();
    http_statuses(&lowered)
        .iter()
        .any(|c| matches!(c.as_str(), "401" | "402" | "403"))
        || [
            "insufficient_balance",
            "quota exhausted",
            "balance is exhausted",
            "invalid_api_key",
            "authentication failed",
            "unauthorized",
        ]
        .iter()
        .any(|term| lowered.contains(term))
}

/// Provider hiccups worth a retry; the harness's own timeouts are not replayed.
pub fn is_transient(text: &str) -> bool {
    let lowered = text.to_lowercase();
    if lowered.contains("octos turn timed out")
        || lowered.contains("turn timed out after")
        || is_permanent_provider_error(text)
    {
        return false;
    }
    let codes = http_statuses(&lowered);
    if !codes.is_empty() {
        return codes.iter().any(|c| {
            matches!(
                c.as_str(),
                "408" | "425" | "429" | "500" | "502" | "503" | "504"
            )
        });
    }
    [
        "temporarily unavailable",
        "rate limit",
        "timeout",
        "timed out",
        "connection reset",
        "overloaded",
        "failed to send",
        "streaming request",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
}

/// One request's accounting inputs.
pub struct UsageEntry<'a> {
    pub label: &'a str,
    pub mode: ReasoningMode,
    pub usage: &'a TokenUsage,
    pub elapsed_ms: u64,
    pub request_chars: usize,
    pub response_chars: usize,
    pub shape: Value,
}

/// Per-request usage records (`llm_proxy.usage_record`) plus the
/// `turn/completed` / `token_cost_update` mirror `arc/metrics.py` reads.
pub struct UsageLedger {
    usage_path: PathBuf,
    events_path: PathBuf,
    model: String,
    provider: String,
    /// Mirror `turn/completed` / `token_cost_update` into `.arc/octos-events.jsonl`
    /// (the in-process client has no kernel session emitting them).
    mirror_events: bool,
    requests: u32,
    prompt_tokens: u64,
    completion_tokens: u64,
    reasoning_tokens: u64,
    cache_hit_tokens: u64,
    cost: f64,
    single_model_pricing: bool,
}

impl UsageLedger {
    pub fn new(arc_dir: &Path, model: &str, provider: &str) -> Self {
        Self {
            usage_path: arc_dir.join("llm-usage.jsonl"),
            events_path: arc_dir.join("octos-events.jsonl"),
            model: model.to_string(),
            provider: provider.to_string(),
            mirror_events: true,
            requests: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            reasoning_tokens: 0,
            cache_hit_tokens: 0,
            cost: 0.0,
            single_model_pricing: true,
        }
    }

    fn append(path: &Path, line: &str) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{line}");
        }
    }

    pub fn shared(arc_dir: &Path, model: &str, provider: &str) -> SharedLedger {
        std::sync::Arc::new(std::sync::Mutex::new(Self::new(arc_dir, model, provider)))
    }

    /// Routed turns may contain multiple models; the default model's price is invalid.
    pub fn disable_single_model_pricing(&mut self) {
        self.single_model_pricing = false;
    }

    fn price(&mut self, usage: &TokenUsage) {
        if !self.single_model_pricing {
            return;
        }
        let pricing_provider = if self.model.to_lowercase().contains("deepseek") {
            "deepseek"
        } else {
            self.provider.as_str()
        };
        if let Some(pricing) = octos_llm::pricing::model_pricing(&self.model) {
            self.cost += pricing.cost_with_cache_for_provider(
                pricing_provider,
                &self.model,
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_tokens,
                usage.cache_write_tokens,
            );
        }
    }

    /// One kernel-session turn (tool mode): the kernel already emitted its own
    /// `turn/completed` / `token_cost_update` events, so only the usage line
    /// is written, with the number of LLM calls the turn made.
    pub fn record_turn(
        &mut self,
        label: &str,
        mode: ReasoningMode,
        requests: u32,
        usage: &TokenUsage,
        elapsed_ms: u64,
    ) -> Value {
        let prompt = u64::from(usage.input_tokens) + u64::from(usage.cache_read_tokens);
        let completion = u64::from(usage.output_tokens);
        self.requests += requests.max(1);
        self.prompt_tokens += prompt;
        self.completion_tokens += completion;
        self.reasoning_tokens += u64::from(usage.reasoning_tokens);
        self.cache_hit_tokens += u64::from(usage.cache_read_tokens);
        self.price(usage);
        let record = json!({
            "ts": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
            "elapsed_ms": elapsed_ms,
            "mode": mode.label(),
            "label": label,
            "requests": requests.max(1),
            "sse_chunks": 0,
            "prompt_tokens": prompt,
            "completion_tokens": completion,
            "total_tokens": prompt + completion,
            "prompt_cache_hit_tokens": usage.cache_read_tokens,
            "reasoning_tokens": usage.reasoning_tokens,
        });
        Self::append(&self.usage_path, &record.to_string());
        record
    }

    pub fn record(&mut self, entry: &UsageEntry<'_>) -> Value {
        let UsageEntry {
            label,
            mode,
            usage,
            elapsed_ms,
            request_chars,
            response_chars,
            shape,
        } = entry;
        let (label, mode, usage, elapsed_ms, request_chars, response_chars) = (
            *label,
            *mode,
            *usage,
            *elapsed_ms,
            *request_chars,
            *response_chars,
        );
        let prompt = u64::from(usage.input_tokens) + u64::from(usage.cache_read_tokens);
        let completion = u64::from(usage.output_tokens);
        self.requests += 1;
        self.prompt_tokens += prompt;
        self.completion_tokens += completion;
        self.reasoning_tokens += u64::from(usage.reasoning_tokens);
        self.cache_hit_tokens += u64::from(usage.cache_read_tokens);
        self.price(usage);
        let record = json!({
            "ts": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
            "elapsed_ms": elapsed_ms,
            "mode": mode.label(),
            "label": label,
            "sse_chunks": 0,
            "prompt_tokens": prompt,
            "completion_tokens": completion,
            "total_tokens": prompt + completion,
            "prompt_cache_hit_tokens": usage.cache_read_tokens,
            "reasoning_tokens": usage.reasoning_tokens,
            "request_bytes": request_chars,
            "response_bytes": response_chars,
            "request": shape.clone(),
        });
        Self::append(&self.usage_path, &record.to_string());
        if self.mirror_events {
            Self::append(
                &self.events_path,
                &json!({"method": "turn/completed", "params": {"session_id": "arc-run", "turn_id": label,
                    "tokens_in": prompt, "tokens_out": completion, "cache_hit": usage.cache_read_tokens}})
                .to_string(),
            );
            Self::append(
                &self.events_path,
                &json!({"method": "progress/updated", "params": {"session_id": "arc-run", "metadata": {"kind": "token_cost_update",
                    "token_cost": {"session_cost": self.cost, "input_tokens": self.prompt_tokens, "output_tokens": self.completion_tokens}}}})
                .to_string(),
            );
        }
        record
    }

    pub fn totals(&self) -> Value {
        json!({
            "requests": self.requests,
            "prompt_tokens": self.prompt_tokens,
            "completion_tokens": self.completion_tokens,
            "reasoning_tokens": self.reasoning_tokens,
            "prompt_cache_hit_tokens": self.cache_hit_tokens,
            "total_tokens": self.prompt_tokens + self.completion_tokens,
            "estimated_cost": if self.single_model_pricing { Some(self.cost) } else { None },
        })
    }
}

/// Real model client on top of `octos-llm`.
pub struct LlmClient {
    provider: OpenAIProvider,
    /// For the token-free start-up probe (GET /models with the same key).
    base_url: String,
    api_key: String,
    model: String,
    runtime: tokio::runtime::Runtime,
    max_tokens_min: u32,
    retries: u32,
    backoff: Duration,
    ledger: SharedLedger,
    dump_dir: Option<PathBuf>,
    dumped: usize,
}

impl LlmClient {
    pub fn new(
        route: &ModelRoute,
        policy: &ReasoningPolicy,
        ledger: SharedLedger,
        arc_dir: &Path,
        chat_timeout: Duration,
        dump: bool,
    ) -> Result<Self> {
        ensure!(
            route.provider != "anthropic",
            "octos arc run drives OpenAI-compatible endpoints; provider {} is not supported yet",
            route.provider
        );
        let key = std::env::var(&route.api_key_env)
            .wrap_err_with(|| format!("{} is not set", route.api_key_env))?;
        ensure!(!key.trim().is_empty(), "{} is empty", route.api_key_env);
        ensure!(!route.model.trim().is_empty(), "runner spec has no model");
        let base_url = if route.base_url.trim().is_empty() {
            "https://api.openai.com/v1".to_string()
        } else {
            route.base_url.trim().trim_end_matches('/').to_string()
        };
        let mut provider = OpenAIProvider::new(key.clone(), route.model.clone());
        if !route.base_url.trim().is_empty() {
            provider = provider.with_base_url(&base_url);
        }
        provider = provider.with_chat_timeout(chat_timeout);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .build()?;
        Ok(Self {
            provider,
            base_url,
            api_key: key,
            model: route.model.clone(),
            runtime,
            max_tokens_min: policy.max_tokens_min,
            retries: policy.transient_retries.max(1),
            backoff: Duration::from_secs(policy.transient_backoff_seconds),
            ledger,
            dump_dir: dump.then(|| arc_dir.join("llm-requests")),
            dumped: 0,
        })
    }

    fn config(&self, mode: ReasoningMode, max_tokens: u32) -> ChatConfig {
        ChatConfig {
            max_tokens: Some(max_tokens),
            reasoning_effort: mode.effort(),
            ..ChatConfig::default()
        }
    }

    fn dump(&mut self, messages: &[Message]) {
        let Some(dir) = &self.dump_dir else { return };
        if self.dumped >= 3 {
            return;
        }
        if std::fs::create_dir_all(dir).is_ok() {
            self.dumped += 1;
            let _ = std::fs::write(
                dir.join(format!("request-{:02}.json", self.dumped)),
                serde_json::to_string_pretty(&json!({"messages": messages})).unwrap_or_default(),
            );
        }
    }

    fn call_once(
        &self,
        messages: &[Message],
        config: &ChatConfig,
        timeout: Duration,
    ) -> Result<octos_llm::ChatResponse> {
        self.runtime.block_on(async {
            match tokio::time::timeout(timeout, self.provider.chat(messages, &[], config)).await {
                Ok(result) => result,
                Err(_) => bail!("octos turn timed out after {}s", timeout.as_secs()),
            }
        })
    }
}

impl Completer for LlmClient {
    fn complete(&mut self, request: &CompletionRequest<'_>) -> Result<Completion> {
        let messages = vec![Message::system(request.system), Message::user(request.user)];
        self.dump(&messages);
        let config = self.config(request.mode, self.max_tokens_min);
        let started = Instant::now();
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let remaining = request.timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                bail!("octos turn timed out after {}s", request.timeout.as_secs());
            }
            match self.call_once(&messages, &config, remaining) {
                Ok(response) => {
                    let text = response.content.clone().unwrap_or_default();
                    let elapsed_ms = started.elapsed().as_millis() as u64;
                    let shape = json!({
                        "messages": 2, "tools": 0, "tools_chars": 2,
                        "system_chars": request.system.chars().count(),
                        "user_chars": request.user.chars().count(),
                    });
                    let request_chars = request.system.len() + request.user.len();
                    let response_chars = text.len()
                        + response
                            .reasoning_content
                            .as_ref()
                            .map(|r| r.len())
                            .unwrap_or(0);
                    if let Ok(mut ledger) = self.ledger.lock() {
                        ledger.record(&UsageEntry {
                            label: request.label,
                            mode: request.mode,
                            usage: &response.usage,
                            elapsed_ms,
                            request_chars,
                            response_chars,
                            shape,
                        });
                    }
                    return Ok(Completion {
                        text,
                        truncated: response.stop_reason == StopReason::MaxTokens,
                        usage: response.usage,
                        elapsed_ms,
                        attempts: attempt,
                    });
                }
                Err(error) => {
                    let text = format!("{error:#}");
                    if attempt >= self.retries || !is_transient(&text) {
                        return Err(error);
                    }
                    let wait = self.backoff * attempt;
                    eprintln!(
                        "[driver] transient error, retry {}/{} after {}s: {}",
                        attempt + 1,
                        self.retries,
                        wait.as_secs(),
                        text.chars().take(200).collect::<String>()
                    );
                    std::thread::sleep(wait);
                }
            }
        }
    }

    /// `main.probe_endpoint` (round 33): wait out endpoint/proxy outages without
    /// spending tokens — GET /models first (unbilled; any non-5xx answer = up); only
    /// if that never answers, one chat request with thinking disabled and
    /// `max_tokens: 1`. The old "Reply with exactly: OK" probe let the model reason
    /// before its answer (≈¥0.0008 per run, a third of a Smoke task).
    fn probe(&mut self, patience: Duration) -> Vec<String> {
        let mut lines = Vec::new();
        let deadline = Instant::now() + patience;
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(60))
            .build();
        let Ok(http) = http else {
            lines.push("[probe] could not build an HTTP client; proceeding anyway".into());
            return lines;
        };
        let bearer = format!("Bearer {}", self.api_key);
        let mut attempt = 0;
        loop {
            attempt += 1;
            match http
                .get(format!("{}/models", self.base_url))
                .header("Authorization", &bearer)
                .timeout(Duration::from_secs(30))
                .send()
            {
                Ok(response) if endpoint_is_up(response.status().as_u16()) => {
                    lines.push(format!(
                        "[probe] GET /models -> HTTP {} (endpoint up, no tokens spent)",
                        response.status().as_u16()
                    ));
                    return lines;
                }
                Ok(response) => lines.push(format!(
                    "[probe] attempt {attempt}: GET /models -> HTTP {}",
                    response.status().as_u16()
                )),
                Err(error) => {
                    lines.push(format!("[probe] attempt {attempt}: GET /models -> {error}"));
                    // Some gateways expose only chat/completions: one minimal, reasoning-free request.
                    match http
                        .post(format!("{}/chat/completions", self.base_url))
                        .header("Authorization", &bearer)
                        .json(&minimal_probe_body(&self.model))
                        .send()
                    {
                        Ok(response) if endpoint_is_up(response.status().as_u16()) => {
                            lines.push(format!(
                                "[probe] minimal chat probe -> HTTP {} (endpoint up)",
                                response.status().as_u16()
                            ));
                            return lines;
                        }
                        Ok(response) => lines.push(format!(
                            "[probe] attempt {attempt}: chat -> HTTP {}",
                            response.status().as_u16()
                        )),
                        Err(error) => {
                            lines.push(format!("[probe] attempt {attempt}: chat -> {error}"))
                        }
                    }
                }
            }
            if Instant::now() >= deadline {
                lines.push("[probe] endpoint still failing; proceeding anyway".into());
                return lines;
            }
            std::thread::sleep(Duration::from_secs(30));
        }
    }
}

/// `main.endpoint_is_up`: any non-5xx HTTP answer proves the endpoint is reachable (401/404 included).
pub fn endpoint_is_up(status: u16) -> bool {
    status < 500
}

/// `main.minimal_probe_body`: a fallback chat probe that cannot bill reasoning.
pub fn minimal_probe_body(model: &str) -> Value {
    json!({"model": model, "messages": [{"role": "user", "content": "OK"}], "max_tokens": 1,
        "thinking": {"type": "disabled"}})
}

/// `OCTOS_ARC_DRYRUN=1`: walk the flow without calling a model.
pub struct DryRunCompleter {
    pub calls: u32,
}

/// `main.DRYRUN_FILES`: the placeholder app a dry-run codegen turn "returns".
pub const DRYRUN_FILES: &str = r#"<<<FILE frontend/src/index.html>>>
<!DOCTYPE html><html><head><meta charset="utf-8"><title>dry run</title></head>
<body><!--NAV--><main data-testid="dryrun">dry run placeholder</main></body></html>
<<<END FILE>>>
<<<FILE backend/server.js>>>
const http = require('http'); const fs = require('fs'); const path = require('path');
const dist = path.join(__dirname, '..', 'frontend', 'dist');
const handler = (req, res) => { try {
  const file = path.join(dist, req.url === '/' ? 'index.html' : req.url.split('?')[0]);
  if (!file.startsWith(dist) || !fs.existsSync(file) || fs.statSync(file).isDirectory()) { res.writeHead(404); return res.end('not found'); }
  res.writeHead(200, {'Content-Type': 'text/html; charset=utf-8'}); res.end(fs.readFileSync(file));
} catch (e) { res.writeHead(500); res.end('error'); } };
http.createServer(handler).listen(process.env.PORT || 3000);
process.on('uncaughtException', () => {});
<<<END FILE>>>
"#;

impl Completer for DryRunCompleter {
    fn complete(&mut self, request: &CompletionRequest<'_>) -> Result<Completion> {
        self.calls += 1;
        // Same fixed replies as the Python `DryRunDriver`: file blocks for codegen
        // prompts, a bare page for the tiny tier, a sentence otherwise.
        let text = if request.user.contains("<<<FILE") {
            DRYRUN_FILES.to_string()
        } else if request.user.contains("page markup only") {
            "<!DOCTYPE html><html><head><meta charset=\"utf-8\"></head><body><main>dry run</main></body></html>".to_string()
        } else {
            format!("dry run: no model call for {}", request.label)
        };
        Ok(Completion {
            text,
            truncated: false,
            usage: TokenUsage::default(),
            elapsed_ms: 0,
            attempts: 1,
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn routed_usage_does_not_claim_the_default_models_price() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = super::UsageLedger::new(dir.path(), "default-model", "openai");
        ledger.disable_single_model_pricing();
        assert!(ledger.totals()["estimated_cost"].is_null());
    }

    use super::*;

    #[test]
    fn should_treat_any_non_5xx_answer_as_endpoint_up_and_keep_the_probe_reasoning_free() {
        assert!(endpoint_is_up(200) && endpoint_is_up(401) && endpoint_is_up(404));
        assert!(!endpoint_is_up(502) && !endpoint_is_up(503));
        let body = minimal_probe_body("m");
        assert_eq!(body["max_tokens"], 1);
        assert_eq!(body["thinking"]["type"], "disabled");
    }

    #[test]
    fn should_map_reasoning_modes_like_the_proxy() {
        assert_eq!(ReasoningMode::parse("none"), Some(ReasoningMode::Disabled));
        assert_eq!(
            ReasoningMode::Disabled.effort(),
            Some(ReasoningEffort::Disabled)
        );
        assert_eq!(ReasoningMode::Low.effort(), Some(ReasoningEffort::Low));
        assert_eq!(ReasoningMode::Passthrough.effort(), None);
        assert_eq!(ReasoningMode::parse("loud"), None);
    }

    #[test]
    fn should_not_retry_own_turn_timeouts_but_retry_provider_errors() {
        assert!(!is_transient("octos turn timed out after 900s"));
        assert!(is_transient("HTTP 503 Service Temporarily Unavailable"));
        assert!(is_transient("failed to send streaming request"));
        assert!(!is_transient("codegen reply contained no blocks"));
        assert!(!is_transient(
            "HTTP 402 insufficient_balance request id 503429401"
        ));
        assert!(!is_transient("HTTP 401 authentication failed"));
        assert!(!is_transient("HTTP 403 forbidden: request timed out"));
        assert!(!is_transient("bad input request id 502"));
    }

    #[test]
    fn should_write_usage_records_in_the_proxy_format() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = UsageLedger::new(dir.path(), "deepseek-v4-flash", "openai");
        let usage = TokenUsage {
            input_tokens: 100,
            output_tokens: 40,
            reasoning_tokens: 10,
            cache_read_tokens: 400,
            ..Default::default()
        };
        let record = ledger.record(&UsageEntry {
            label: "REQ-1 implement",
            mode: ReasoningMode::Disabled,
            usage: &usage,
            elapsed_ms: 1200,
            request_chars: 10,
            response_chars: 20,
            shape: json!({"messages": 2}),
        });
        assert_eq!(record["prompt_tokens"], 500);
        assert_eq!(record["completion_tokens"], 40);
        assert_eq!(record["prompt_cache_hit_tokens"], 400);
        assert_eq!(record["mode"], "none");
        let usage_lines = std::fs::read_to_string(dir.path().join("llm-usage.jsonl")).unwrap();
        assert_eq!(usage_lines.lines().count(), 1);
        let events = std::fs::read_to_string(dir.path().join("octos-events.jsonl")).unwrap();
        assert!(events.contains("\"method\":\"turn/completed\""));
        assert!(events.contains("token_cost_update"));
        assert_eq!(ledger.totals()["requests"], 1);
        assert_eq!(ledger.totals()["total_tokens"], 540);
        let turn = ledger.record_turn("REQ-1 repair 1/5", ReasoningMode::Low, 3, &usage, 5000);
        assert_eq!(turn["requests"], 3);
        assert_eq!(ledger.totals()["requests"], 4);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("llm-usage.jsonl"))
                .unwrap()
                .lines()
                .count(),
            2
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("octos-events.jsonl"))
                .unwrap()
                .lines()
                .count(),
            2
        );
    }

    #[test]
    fn should_answer_dry_runs_without_a_model() {
        let mut dry = DryRunCompleter { calls: 0 };
        let request = CompletionRequest {
            label: "x",
            system: "s",
            user: "u",
            mode: ReasoningMode::Low,
            timeout: Duration::from_secs(1),
        };
        let completion = dry.complete(&request).unwrap();
        assert!(completion.text.starts_with("dry run"));
        assert_eq!(dry.calls, 1);
    }
}
