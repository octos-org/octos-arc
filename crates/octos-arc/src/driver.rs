//! Kernel session driver (`arc/octos_stdio.py` + `OctosDriver`): spawns
//! `octos serve --stdio --solo` from this very executable and speaks the UI
//! protocol over NDJSON JSON-RPC. No handshake: create a solo profile,
//! select its model, open a session, start turns, collect notifications
//! until `turn/completed` / `turn/error`. Approvals are auto-approved so
//! unattended runs never block.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eyre::{Result, bail};
use serde_json::{Value, json};

use crate::envs::{self, EnvVec};
use crate::llm::ReasoningMode;

/// How the kernel child is started and which model the profile selects.
#[derive(Debug, Clone)]
pub struct KernelConfig {
    pub executable: PathBuf,
    pub cwd: PathBuf,
    pub data_dir: PathBuf,
    /// Base environment (the harness's, plus platform variables).
    pub env: EnvVec,
    pub danger_full_access: bool,
    /// Provider family id for `profile/llm/upsert` (openai | custom | deepseek …).
    pub provider: String,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    /// `before_tool_call` hooks written into the profile registry.
    pub hooks: Vec<Value>,
    /// Profile `gateway.max_output_tokens` (the `max_tokens` floor).
    pub max_output_tokens: u32,
}

/// Per-turn shape: the kernel reads these once per process, so a session
/// scope other than `turn` keeps the first turn's settings.
#[derive(Debug, Clone)]
pub struct TurnSettings {
    pub reasoning: ReasoningMode,
    /// Request cap (`gateway.max_iterations`); None = unlimited.
    pub max_iterations: Option<u32>,
    /// Tool allow-list for the session (subset of the stdio coding tools).
    pub tools: Option<Vec<String>>,
}

pub struct TurnOutcome {
    pub ok: bool,
    pub text: String,
    /// LLM calls observed during the turn: the highest `iteration` the
    /// kernel's progress events reported (`token_cost_update` is per turn).
    pub requests: u32,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub cache_hit: u32,
    pub tool_calls: u32,
}

pub struct StdioSession {
    child: Child,
    stdin: ChildStdin,
    notifications: Receiver<Value>,
    pending: Arc<Mutex<HashMap<String, Sender<Value>>>>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    pub session_id: String,
    profile_id: Option<String>,
    counter: u64,
    data_dir: PathBuf,
    cwd: PathBuf,
}

impl StdioSession {
    pub fn spawn(config: &KernelConfig, env: &EnvVec) -> Result<Self> {
        let mut command = Command::new(&config.executable);
        command
            .arg("serve")
            .arg("--stdio")
            .arg("--solo")
            .arg("--data-dir")
            .arg(&config.data_dir);
        if config.danger_full_access {
            command.arg("--danger-full-access");
        }
        command
            .current_dir(&config.cwd)
            .env_clear()
            .envs(env.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| eyre::eyre!("kernel stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| eyre::eyre!("kernel stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| eyre::eyre!("kernel stderr"))?;
        let (notify_tx, notifications) = channel::<Value>();
        let pending: Arc<Mutex<HashMap<String, Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_reader = Arc::clone(&pending);
        let notify_stdout = notify_tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if frame.get("method").is_some() {
                    if notify_stdout.send(frame).is_err() {
                        break;
                    }
                } else if let Some(id) = frame.get("id") {
                    let key = match id {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    let waiter = pending_reader.lock().ok().and_then(|mut p| p.remove(&key));
                    if let Some(waiter) = waiter {
                        let _ = waiter.send(frame);
                    }
                }
            }
        });
        let stderr_tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
        let tail_writer = Arc::clone(&stderr_tail);
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Ok(mut tail) = tail_writer.lock() {
                    if tail.len() >= 50 {
                        tail.pop_front();
                    }
                    tail.push_back(line.clone());
                }
                if line.contains("[arc-mod]") {
                    let _ =
                        notify_tx.send(json!({"method": "core/marker", "params": {"line": line}}));
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            notifications,
            pending,
            stderr_tail,
            session_id: format!(
                "arc-run:{}",
                &uuid::Uuid::new_v4().simple().to_string()[..8]
            ),
            profile_id: None,
            counter: 0,
            data_dir: config.data_dir.clone(),
            cwd: config.cwd.clone(),
        })
    }

    pub fn stderr_tail(&self) -> String {
        self.stderr_tail
            .lock()
            .map(|t| {
                t.iter()
                    .rev()
                    .take(10)
                    .cloned()
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }

    fn send(
        &mut self,
        method: &str,
        params: Option<Value>,
        want_response: bool,
        timeout: Duration,
    ) -> Result<Option<Value>> {
        self.counter += 1;
        let id = format!("rs-{}", self.counter);
        let mut frame = json!({"jsonrpc": "2.0", "method": method});
        if let Some(params) = params {
            frame["params"] = params;
        }
        let waiter = if want_response {
            frame["id"] = json!(id);
            let (tx, rx) = channel::<Value>();
            self.pending
                .lock()
                .map_err(|_| eyre::eyre!("pending lock"))?
                .insert(id.clone(), tx);
            Some(rx)
        } else {
            None
        };
        if let Err(error) = writeln!(self.stdin, "{frame}").and_then(|_| self.stdin.flush()) {
            bail!(
                "octos stdio write failed (process died?): {error}; stderr tail: {}",
                self.stderr_tail()
            );
        }
        let Some(rx) = waiter else { return Ok(None) };
        let reply = rx.recv_timeout(timeout).map_err(|_| {
            eyre::eyre!(
                "timeout waiting response to {method}; stderr tail: {}",
                self.stderr_tail()
            )
        })?;
        if let Some(error) = reply.get("error") {
            bail!("{method} failed: {error}");
        }
        Ok(Some(reply.get("result").cloned().unwrap_or(Value::Null)))
    }

    /// Create a solo profile and select its LLM. The requested id is unique
    /// per call: the runner machine's profile store persists across
    /// submissions and our own retries, so a fixed id would collide.
    pub fn bootstrap_profile(
        &mut self,
        config: &KernelConfig,
        settings: &TurnSettings,
    ) -> Result<()> {
        let unique = format!(
            "arc-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_millis() % 10_000_000
        );
        let result = self.send(
            "profile/local/create",
            Some(json!({"requested_id": unique, "name": "ARC Harness Agent", "username": unique, "email": format!("{unique}@solo.local")})),
            true,
            Duration::from_secs(60),
        )?;
        let profile_id = result
            .as_ref()
            .and_then(|r| r.get("profile_id"))
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| eyre::eyre!("profile/local/create gave no profile_id: {result:?}"))?;
        // The solo ProfileRuntime builds hooks and gateway knobs from the
        // profile's own config: patch the registry file before the LLM upsert
        // re-reads and re-saves it.
        let mut patch = json!({"hooks": config.hooks});
        patch["gateway"] = json!({"max_output_tokens": config.max_output_tokens});
        if let Some(cap) = settings.max_iterations {
            patch["gateway"]["max_iterations"] = json!(cap);
        }
        self.patch_profile_config(&profile_id, &patch)?;
        let api_type = if config.provider == "anthropic" {
            "anthropic"
        } else {
            "openai"
        };
        let mut route = json!({"api_type": api_type});
        if let Some(base_url) = &config.base_url {
            route["base_url"] = json!(base_url);
        }
        if let Some(key_env) = &config.api_key_env {
            route["api_key_env"] = json!(key_env);
        }
        self.send(
            "profile/llm/upsert",
            Some(json!({"profile_id": profile_id, "set_primary": true,
                "selection": {"family_id": config.provider, "model_id": config.model, "route": route}})),
            true,
            Duration::from_secs(60),
        )?;
        self.profile_id = Some(profile_id);
        Ok(())
    }

    fn patch_profile_config(&self, profile_id: &str, patch: &Value) -> Result<()> {
        let path = self
            .data_dir
            .join("profiles")
            .join(format!("{profile_id}.json"));
        let text = std::fs::read_to_string(&path).map_err(|e| {
            eyre::eyre!("profile registry file {} not readable: {e}", path.display())
        })?;
        let mut data: Value = serde_json::from_str(&text)?;
        let config = data
            .as_object_mut()
            .ok_or_else(|| eyre::eyre!("profile registry is not an object"))?
            .entry("config")
            .or_insert_with(|| json!({}));
        if let (Some(target), Some(source)) = (config.as_object_mut(), patch.as_object()) {
            for (key, value) in source {
                match (target.get_mut(key), value) {
                    (Some(Value::Object(existing)), Value::Object(incoming)) => {
                        for (k, v) in incoming {
                            existing.insert(k.clone(), v.clone());
                        }
                    }
                    _ => {
                        target.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        std::fs::write(&path, serde_json::to_string_pretty(&data)?)?;
        Ok(())
    }

    pub fn open(&mut self) -> Result<()> {
        let mut params = json!({"session_id": self.session_id, "cwd": self.cwd});
        if let Some(profile_id) = &self.profile_id {
            params["profile_id"] = json!(profile_id);
        }
        self.send("session/open", Some(params), true, Duration::from_secs(120))?;
        Ok(())
    }

    /// Run one turn; every notification is handed to `observer`. Returns the
    /// final assistant text (message deltas, or the persisted assistant row
    /// when the kernel answered non-streaming).
    pub fn run_turn(
        &mut self,
        text: &str,
        timeout: Duration,
        observer: &mut dyn FnMut(&str, &Value),
    ) -> TurnOutcome {
        let turn_id = uuid::Uuid::new_v4().to_string();
        let mut outcome = TurnOutcome {
            ok: false,
            text: String::new(),
            requests: 0,
            tokens_in: 0,
            tokens_out: 0,
            cache_hit: 0,
            tool_calls: 0,
        };
        if timeout.is_zero() {
            outcome.text = "octos turn timed out".into();
            return outcome;
        }
        let started = Instant::now();
        if let Err(error) = self.send(
            "turn/start",
            Some(json!({"session_id": self.session_id, "turn_id": turn_id, "input": [{"kind": "text", "text": text}]})),
            true,
            Duration::from_secs(60).min(timeout),
        ) {
            outcome.text = format!("{error:#}");
            return outcome;
        }
        let mut chunks = String::new();
        let mut persisted: Option<String> = None;
        let mut last_heartbeat = Instant::now();
        loop {
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                outcome.text = "octos turn timed out".into();
                return outcome;
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                outcome.text = format!(
                    "octos process exited {status}; stderr tail: {}",
                    self.stderr_tail()
                );
                return outcome;
            }
            if last_heartbeat.elapsed() >= Duration::from_secs(30) {
                last_heartbeat = Instant::now();
                observer(
                    "harness/heartbeat",
                    &json!({"elapsed_s": started.elapsed().as_secs()}),
                );
            }
            let frame = match self
                .notifications
                .recv_timeout(remaining.min(Duration::from_secs(5)))
            {
                Ok(frame) => frame,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    outcome.text =
                        format!("octos stdout closed; stderr tail: {}", self.stderr_tail());
                    return outcome;
                }
            };
            let method = frame
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let params = frame.get("params").cloned().unwrap_or(Value::Null);
            observer(&method, &params);
            let same_turn = params.get("turn_id").and_then(Value::as_str) == Some(turn_id.as_str());
            match method.as_str() {
                "server/heartbeat" => {}
                "message/delta" if same_turn => {
                    chunks.push_str(params.get("text").and_then(Value::as_str).unwrap_or(""));
                }
                "projection/envelope" if same_turn => {
                    if params
                        .get("payload")
                        .and_then(|p| p.get("type"))
                        .and_then(Value::as_str)
                        == Some("assistant_persisted")
                        && let Some(text) = params
                            .get("payload")
                            .and_then(|p| p.get("data"))
                            .and_then(|d| d.get("text"))
                            .and_then(Value::as_str)
                    {
                        persisted = Some(text.to_string());
                    }
                }
                "tool/started" if same_turn => outcome.tool_calls += 1,
                "progress/updated" if same_turn => {
                    let metadata = params.get("metadata");
                    if let Some(iteration) = metadata
                        .and_then(|m| m.get("iteration"))
                        .and_then(Value::as_u64)
                    {
                        outcome.requests = outcome.requests.max(iteration as u32);
                    } else if metadata.and_then(|m| m.get("kind")).and_then(Value::as_str)
                        == Some("token_cost_update")
                    {
                        outcome.requests = outcome.requests.max(1);
                    }
                }
                "approval/requested" => {
                    let approval_id = params.get("approval_id").cloned().unwrap_or(Value::Null);
                    let _ = self.send(
                        "approval/respond",
                        Some(json!({"approval_id": approval_id, "decision": "approve", "approval_scope": "request"})),
                        true,
                        Duration::from_secs(30),
                    );
                }
                "turn/completed" if same_turn => {
                    outcome.ok = true;
                    outcome.tokens_in =
                        params.get("tokens_in").and_then(Value::as_u64).unwrap_or(0) as u32;
                    outcome.tokens_out = params
                        .get("tokens_out")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as u32;
                    outcome.cache_hit =
                        params.get("cache_hit").and_then(Value::as_u64).unwrap_or(0) as u32;
                    outcome.text = if chunks.is_empty() {
                        persisted.unwrap_or_default()
                    } else {
                        chunks
                    };
                    return outcome;
                }
                "turn/error" if same_turn => {
                    let code = params
                        .get("code")
                        .and_then(Value::as_str)
                        .unwrap_or("error");
                    let message = params.get("message").and_then(Value::as_str).unwrap_or("");
                    outcome.text = format!("{code}: {message}").chars().take(1000).collect();
                    return outcome;
                }
                _ => {}
            }
        }
    }

    pub fn close(&mut self) {
        #[cfg(unix)]
        {
            let _ = Command::new("kill")
                .args(["-TERM", &self.child.id().to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for StdioSession {
    fn drop(&mut self) {
        self.close();
    }
}

/// Session lifecycle (`OctosDriver`): a fresh session per turn by default
/// (the per-turn prompt carries all the state the model needs and a short,
/// byte-stable prefix is what the provider's prefix cache keys on), or one
/// per node / per run.
pub struct Driver {
    config: KernelConfig,
    scope: String,
    session: Option<StdioSession>,
    retries: u32,
    backoff: Duration,
}

impl Driver {
    pub fn new(config: KernelConfig, scope: &str, retries: u32, backoff: Duration) -> Self {
        Self {
            config,
            scope: scope.to_string(),
            session: None,
            retries: retries.max(1),
            backoff,
        }
    }

    fn session_env(&self, settings: &TurnSettings) -> EnvVec {
        let reasoning = settings.reasoning.label().to_string();
        let tools = settings.tools.as_ref().map(|t| t.join(","));
        let mut overrides: Vec<(&str, Option<&str>)> = vec![
            (
                "OCTOS_STDIO_REASONING_EFFORT",
                (settings.reasoning != ReasoningMode::Passthrough).then_some(reasoning.as_str()),
            ),
            ("OCTOS_STDIO_SOLO_TOOLS", tools.as_deref()),
        ];
        let base: Vec<(String, String)> = self
            .config
            .env
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect();
        for (k, v) in &base {
            overrides.push((k.as_str(), Some(v.as_str())));
        }
        envs::inherited(&overrides)
    }

    fn get_session(&mut self, settings: &TurnSettings) -> Result<&mut StdioSession> {
        if self.session.is_none() {
            let env = self.session_env(settings);
            let mut session = StdioSession::spawn(&self.config, &env)?;
            session.bootstrap_profile(&self.config, settings)?;
            session.open()?;
            self.session = Some(session);
        }
        Ok(self.session.as_mut().expect("session just created"))
    }

    /// Run a turn with transient-error retries (the harness's own timeouts
    /// are never replayed).
    pub fn run(
        &mut self,
        prompt: &str,
        timeout: Duration,
        settings: &TurnSettings,
        observer: &mut dyn FnMut(&str, &Value),
    ) -> TurnOutcome {
        let deadline = Instant::now() + timeout;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let outcome = match self.get_session(settings) {
                Ok(session) => session.run_turn(
                    prompt,
                    deadline.saturating_duration_since(Instant::now()),
                    observer,
                ),
                Err(error) => TurnOutcome {
                    ok: false,
                    text: format!("stdio driver error: {error:#}"),
                    requests: 0,
                    tokens_in: 0,
                    tokens_out: 0,
                    cache_hit: 0,
                    tool_calls: 0,
                },
            };
            if outcome.ok || attempt >= self.retries || !crate::llm::is_transient(&outcome.text) {
                if self.scope == "turn" {
                    self.close();
                }
                return outcome;
            }
            let wait = self.backoff * attempt;
            if wait >= deadline.saturating_duration_since(Instant::now()) {
                if self.scope == "turn" {
                    self.close();
                }
                return outcome;
            }
            observer(
                "harness/retry",
                &json!({"attempt": attempt + 1, "of": self.retries, "wait_s": wait.as_secs(), "error": outcome.text.chars().take(200).collect::<String>()}),
            );
            std::thread::sleep(wait);
            self.close();
        }
    }

    /// Called at node boundaries; closes the session when the configured
    /// scope ends.
    pub fn end_scope(&mut self, scope: &str) {
        if scope == self.scope || self.scope == "turn" {
            self.close();
        }
    }

    pub fn close(&mut self) {
        if let Some(mut session) = self.session.take() {
            session.close();
        }
    }
}

/// Environment for the kernel child (`main.build_octos_env`): the platform
/// variables plus the ARC switches. `key_env` is the variable the profile
/// route reads; when the provider family wants its own name, mirror the key.
pub fn kernel_env(
    config_dir: &Path,
    provider: &str,
    api_key_env: &str,
    smoke_port: u16,
    destream: bool,
) -> (EnvVec, String) {
    let family_key = match provider {
        "openai" => "OPENAI_API_KEY".to_string(),
        "deepseek" => "DEEPSEEK_API_KEY".to_string(),
        "anthropic" => "ANTHROPIC_API_KEY".to_string(),
        other => format!("{}_API_KEY", other.to_ascii_uppercase()),
    };
    let key = std::env::var(api_key_env).unwrap_or_default();
    let config_dir = config_dir.to_string_lossy().into_owned();
    let port = smoke_port.to_string();
    let mut overrides: Vec<(&str, Option<&str>)> = vec![
        ("OCTOS_CONFIG_DIR", Some(&config_dir)),
        ("OCTOS_DANGER_FULL_ACCESS", Some("1")),
        ("PORT", Some(&port)),
    ];
    if destream {
        overrides.push(("OCTOS_DISABLE_STREAMING", Some("1")));
    }
    if std::env::var_os("npm_config_registry").is_none() {
        overrides.push((
            "npm_config_registry",
            Some("https://registry.npmmirror.com"),
        ));
    }
    if std::env::var_os("NPM_CONFIG_REGISTRY").is_none() {
        overrides.push((
            "NPM_CONFIG_REGISTRY",
            Some("https://registry.npmmirror.com"),
        ));
    }
    if family_key != api_key_env && !key.is_empty() && std::env::var_os(&family_key).is_none() {
        overrides.push((family_key.as_str(), Some(key.as_str())));
    }
    let env = envs::inherited(&overrides);
    (env, family_key)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake kernel: answers the bootstrap RPCs and one turn with a delta,
    /// a tool call, a token update and turn/completed.
    const FAKE_KERNEL: &str = r#"
import json, sys
for line in sys.stdin:
    req = json.loads(line)
    m, i, p = req.get("method"), req.get("id"), req.get("params") or {}
    def reply(result): print(json.dumps({"jsonrpc": "2.0", "id": i, "result": result}), flush=True)
    def notify(method, params): print(json.dumps({"jsonrpc": "2.0", "method": method, "params": params}), flush=True)
    if m == "profile/local/create":
        import os
        data_dir = sys.argv[sys.argv.index("--data-dir") + 1]
        os.makedirs(os.path.join(data_dir, "profiles"), exist_ok=True)
        with open(os.path.join(data_dir, "profiles", p["requested_id"] + ".json"), "w") as fh:
            json.dump({"id": p["requested_id"], "config": {"gateway": {"max_history": None}}}, fh)
        reply({"profile_id": p["requested_id"]})
    elif m in ("profile/llm/upsert", "session/open", "approval/respond"): reply({})
    elif m == "turn/start":
        reply({})
        t = p["turn_id"]
        notify("message/delta", {"turn_id": t, "text": "hello "})
        notify("tool/started", {"turn_id": t, "tool_call_id": "1", "tool_name": "write_file", "arguments": {"path": "a"}})
        notify("approval/requested", {"turn_id": t, "approval_id": "ap-1"})
        notify("progress/updated", {"turn_id": t, "metadata": {"kind": "thinking", "iteration": 1}})
        notify("progress/updated", {"turn_id": t, "metadata": {"kind": "response", "iteration": 2}})
        notify("progress/updated", {"turn_id": t, "metadata": {"kind": "token_cost_update", "token_cost": {"input_tokens": 10}}})
        notify("message/delta", {"turn_id": t, "text": "world"})
        notify("turn/completed", {"turn_id": t, "tokens_in": 10, "tokens_out": 4, "cache_hit": 2})
"#;

    fn fake_config(dir: &Path) -> Option<KernelConfig> {
        let python = ["python3", "python"].iter().find_map(|p| {
            Command::new(p)
                .arg("--version")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|_| PathBuf::from(p))
        })?;
        let script = dir.join("fake_kernel.py");
        std::fs::write(&script, FAKE_KERNEL).unwrap();
        // The session spawns `<executable> serve --stdio --solo --data-dir X`; a wrapper ignores the arguments.
        let wrapper = dir.join("octos");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nexec {} {} \"$@\"\n",
                python.display(),
                script.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        std::fs::create_dir_all(dir.join("data/profiles")).unwrap();
        Some(KernelConfig {
            executable: wrapper,
            cwd: dir.to_path_buf(),
            data_dir: dir.join("data"),
            env: envs::inherited(&[]),
            danger_full_access: true,
            provider: "custom".into(),
            model: "m".into(),
            base_url: Some("http://127.0.0.1:1/v1".into()),
            api_key_env: Some("CUSTOM_API_KEY".into()),
            hooks: vec![
                json!({"event": "before_tool_call", "command": ["true"], "timeout_ms": 1000, "tool_filter": ["write_file"]}),
            ],
            max_output_tokens: 32768,
        })
    }

    #[cfg(unix)]
    #[test]
    fn should_bootstrap_patch_the_profile_and_collect_a_turn() {
        let dir = tempfile::tempdir().unwrap();
        let Some(config) = fake_config(dir.path()) else {
            return;
        };
        let settings = TurnSettings {
            reasoning: ReasoningMode::Low,
            max_iterations: Some(7),
            tools: None,
        };
        let mut session = StdioSession::spawn(&config, &config.env).unwrap();
        session.bootstrap_profile(&config, &settings).unwrap();
        let id = session.profile_id.clone().unwrap();
        let registry = dir.path().join("data/profiles").join(format!("{id}.json"));
        let patched: Value =
            serde_json::from_str(&std::fs::read_to_string(&registry).unwrap()).unwrap();
        assert_eq!(patched["config"]["gateway"]["max_iterations"], 7);
        assert_eq!(patched["config"]["gateway"]["max_output_tokens"], 32768);
        assert!(patched["config"]["gateway"]["max_history"].is_null());
        assert_eq!(patched["config"]["hooks"][0]["event"], "before_tool_call");
        session.open().unwrap();
        let mut seen: Vec<String> = Vec::new();
        let outcome = session.run_turn("do it", Duration::from_secs(10), &mut |method, _| {
            seen.push(method.to_string())
        });
        assert!(outcome.ok, "{}", outcome.text);
        assert_eq!(outcome.text, "hello world");
        assert_eq!((outcome.requests, outcome.tool_calls), (2, 1));
        assert_eq!(
            (outcome.tokens_in, outcome.tokens_out, outcome.cache_hit),
            (10, 4, 2)
        );
        assert!(seen.iter().any(|m| m == "approval/requested"));
        session.close();
    }

    #[cfg(unix)]
    #[test]
    fn retry_backoff_cannot_outlive_turn_budget() {
        let dir = tempfile::tempdir().unwrap();
        let Some(config) = fake_config(dir.path()) else {
            return;
        };
        let script = FAKE_KERNEL.replace(
            "notify(\"turn/completed\", {\"turn_id\": t, \"tokens_in\": 10, \"tokens_out\": 4, \"cache_hit\": 2})",
            "notify(\"turn/error\", {\"turn_id\": t, \"message\": \"HTTP 503 temporarily unavailable\"})");
        std::fs::write(dir.path().join("fake_kernel.py"), script).unwrap();
        let settings = TurnSettings {
            reasoning: ReasoningMode::Low,
            max_iterations: Some(7),
            tools: None,
        };
        let mut driver = Driver::new(config, "turn", 3, Duration::from_secs(2));
        let started = Instant::now();
        let mut retries = 0;
        let outcome = driver.run(
            "generic task",
            Duration::from_millis(500),
            &settings,
            &mut |method, _| {
                if method == "harness/retry" {
                    retries += 1;
                }
            },
        );
        assert!(!outcome.ok);
        assert_eq!(retries, 0);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn should_build_the_kernel_environment_with_the_arc_switches() {
        let (env, family_key) =
            kernel_env(Path::new("/cfg"), "custom", "OPENAI_API_KEY", 3100, true);
        assert_eq!(family_key, "CUSTOM_API_KEY");
        assert_eq!(envs::lookup(&env, "OCTOS_CONFIG_DIR").unwrap(), "/cfg");
        assert_eq!(envs::lookup(&env, "OCTOS_DISABLE_STREAMING").unwrap(), "1");
        assert_eq!(envs::lookup(&env, "OCTOS_DANGER_FULL_ACCESS").unwrap(), "1");
        assert_eq!(envs::lookup(&env, "PORT").unwrap(), "3100");
        let (env, _) = kernel_env(Path::new("/cfg"), "openai", "OPENAI_API_KEY", 3100, false);
        assert!(envs::lookup(&env, "OCTOS_DISABLE_STREAMING").is_none());
    }
}
