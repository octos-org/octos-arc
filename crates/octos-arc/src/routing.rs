//! Optional per-request routing shared by codegen and kernel tool turns.
//! The relay sees the complete wire request, including accumulated tool history.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use axum::body::{Body, to_bytes};
use axum::extract::State;
use axum::http::{Request, Response, StatusCode};
use eyre::{Result, ensure};
use serde::Deserialize;
use serde_json::{Map, Value, json};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Implement,
    Repair,
    Verify,
    Design,
}

impl Phase {
    pub fn for_label(label: &str) -> Self {
        if label.contains("repair") || label.contains("rewrite") {
            Self::Repair
        } else if label.contains("final check") {
            Self::Verify
        } else if label.contains("design") {
            Self::Design
        } else {
            Self::Implement
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    model: String,
    phases: Option<Vec<Phase>>,
    max_input_chars: Option<usize>,
    #[serde(default)]
    tools: bool,
    #[serde(default)]
    images: bool,
    #[serde(default)]
    parameters: Map<String, Value>,
}

pub fn parse(raw: &str) -> Result<Vec<Rule>> {
    let value: Value = serde_json::from_str(if raw.is_empty() { "[]" } else { raw })?;
    if let Some(rules) = value.as_array() {
        for rule in rules {
            ensure!(
                rule.get("phases").is_none_or(Value::is_array),
                "model route phases must be an array"
            );
        }
    }
    let rules: Vec<Rule> = serde_json::from_str(if raw.is_empty() { "[]" } else { raw })?;
    for rule in &rules {
        ensure!(
            !rule.model.trim().is_empty(),
            "model route requires a model ID"
        );
        ensure!(
            rule.phases.as_ref().is_none_or(|p| !p.is_empty()),
            "model route phases cannot be empty"
        );
        ensure!(
            rule.max_input_chars != Some(0),
            "max_input_chars must be positive"
        );
        ensure!(
            rule.parameters.keys().all(|k| matches!(
                k.as_str(),
                "temperature"
                    | "top_p"
                    | "max_tokens"
                    | "max_completion_tokens"
                    | "thinking"
                    | "reasoning_effort"
            )),
            "model route parameters cannot replace messages, tools or routing"
        );
        ensure!(
            !(rule.parameters.contains_key("max_tokens")
                && rule.parameters.contains_key("max_completion_tokens")),
            "choose one output token limit"
        );
    }
    Ok(rules)
}

pub fn route(body: &[u8], rules: &[Rule], phase: Phase) -> Result<Vec<u8>> {
    if rules.is_empty() {
        return Ok(body.to_vec());
    }
    let mut data: Value = serde_json::from_slice(body)?;
    let Some(messages) = data.get("messages").and_then(Value::as_array) else {
        return Ok(body.to_vec());
    };
    let images = messages.iter().any(|m| {
        m.get("content").and_then(Value::as_array).is_some_and(|c| {
            c.iter().any(|c| {
                matches!(
                    c.get("type").and_then(Value::as_str),
                    Some("image_url" | "input_image")
                )
            })
        })
    });
    let tools = data
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|t| !t.is_empty())
        || messages.iter().any(|m| {
            m.get("role").and_then(Value::as_str) == Some("tool")
                || m.get("tool_calls")
                    .and_then(Value::as_array)
                    .is_some_and(|t| !t.is_empty())
        });
    let chars = serde_json::to_string(
        &json!({"messages": messages, "tools": data.get("tools").unwrap_or(&json!([]))}),
    )?
    .chars()
    .count();
    for rule in rules {
        if rule.phases.as_ref().is_some_and(|p| !p.contains(&phase))
            || rule.max_input_chars.is_some_and(|limit| chars > limit)
            || (tools && !rule.tools)
            || (images && !rule.images)
        {
            continue;
        }
        let obj = data.as_object_mut().expect("messages object");
        obj.insert("model".into(), json!(rule.model));
        obj.remove("thinking");
        obj.remove("reasoning_effort");
        if rule.parameters.contains_key("max_tokens") {
            obj.remove("max_completion_tokens");
        }
        if rule.parameters.contains_key("max_completion_tokens") {
            obj.remove("max_tokens");
        }
        obj.extend(rule.parameters.clone());
        return Ok(serde_json::to_vec(&data)?);
    }
    Ok(body.to_vec())
}

#[derive(Clone)]
pub struct Control(Arc<RwLock<Phase>>);

impl Control {
    pub(crate) fn phase(&self) -> Phase {
        *self.0.read().expect("routing phase lock")
    }

    pub fn set_label(&self, label: &str) {
        *self.0.write().expect("routing phase lock") = Phase::for_label(label);
    }
}

#[derive(Clone)]
struct RelayState {
    upstream: String,
    rules: Vec<Rule>,
    control: Control,
    client: reqwest::Client,
    log: PathBuf,
}

/// Owns its loopback listener and runtime; dropping it releases both.
pub struct Relay {
    pub base_url: String,
    pub control: Control,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Relay {
    pub fn start(upstream: &str, rules: Vec<Rule>, arc_dir: &Path) -> Result<Self> {
        let url = reqwest::Url::parse(upstream)?;
        ensure!(
            url.username().is_empty() && url.password().is_none(),
            "use authorization headers for endpoint credentials"
        );
        ensure!(
            matches!(url.scheme(), "http" | "https"),
            "routing needs an HTTP endpoint"
        );
        ensure!(
            url.query().is_none() && url.fragment().is_none(),
            "endpoint must not include a query or fragment"
        );
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let base_url = format!("http://{}/v1", listener.local_addr()?);
        let control = Control(Arc::new(RwLock::new(Phase::Implement)));
        let state = RelayState {
            upstream: upstream.trim_end_matches('/').to_owned(),
            rules,
            control: control.clone(),
            log: arc_dir.join("model-routes.jsonl"),
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
        };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let thread = std::thread::spawn(move || {
            runtime.block_on(async move {
                let listener =
                    tokio::net::TcpListener::from_std(listener).expect("nonblocking listener");
                let app = axum::Router::new().fallback(forward).with_state(state);
                tokio::select! {
                    _ = axum::serve(listener, app) => {},
                    _ = stopped => {},
                }
            })
        });
        Ok(Self {
            base_url,
            control,
            stop: Some(stop),
            thread: Some(thread),
        })
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn hop_header(name: &str) -> bool {
    matches!(
        name,
        "host"
            | "content-length"
            | "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

async fn forward(State(state): State<RelayState>, request: Request<Body>) -> Response<Body> {
    match forward_inner(state, request).await {
        Ok(response) => response,
        Err(error) => Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(Body::from(format!("local model routing failed: {error}")))
            .expect("response"),
    }
}

async fn forward_inner(state: RelayState, request: Request<Body>) -> Result<Response<Body>> {
    let phase = state.control.phase();
    let (parts, body) = request.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let suffix = path.strip_prefix("/v1").unwrap_or(path);
    let url = format!("{}{suffix}", state.upstream);
    let body = to_bytes(body, 16 * 1024 * 1024).await?;
    let chat = parts.method == "POST" && parts.uri.path().ends_with("/chat/completions");
    let body = if chat {
        route(&body, &state.rules, phase)?
    } else {
        body.to_vec()
    };
    let model = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| v.get("model").cloned());
    let mut outgoing = state.client.request(parts.method, url);
    for (name, value) in &parts.headers {
        if !hop_header(name.as_str()) {
            outgoing = outgoing.header(name, value);
        }
    }
    let response = outgoing.body(body).send().await?;
    if chat {
        let _ = std::fs::create_dir_all(state.log.parent().expect("log parent"));
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&state.log)
        {
            let _ = writeln!(
                file,
                "{}",
                json!({"phase": format!("{phase:?}").to_lowercase(), "model": model, "http_status": response.status().as_u16()})
            );
        }
    }
    let mut builder = Response::builder().status(response.status());
    for (name, value) in response.headers() {
        if !hop_header(name.as_str()) {
            builder = builder.header(name, value);
        }
    }
    // Preserve streaming; do not collect SSE until the whole model turn finishes.
    Ok(builder.body(Body::from_stream(response.bytes_stream()))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read};
    use std::time::Duration;

    #[test]
    fn shared_python_rust_contract() {
        let cases: Vec<Value> = serde_json::from_str(include_str!(
            "../../../arc/tests/fixtures/model-routing.json"
        ))
        .unwrap();
        for case in cases {
            let body = serde_json::to_vec(&case["body"]).unwrap();
            let rules = parse(&case["rules"].to_string()).unwrap();
            let phase = serde_json::from_value(case["phase"].clone()).unwrap();
            let result = route(&body, &rules, phase).unwrap();
            let parsed: Value = serde_json::from_slice(&result).unwrap();
            assert_eq!(parsed["model"], case["model"], "{}", case["name"]);
            if case["model"] == "original" {
                assert_eq!(result, body);
            }
        }
    }

    #[test]
    fn configuration_rejects_unsafe_overrides_and_ambiguous_limits() {
        for raw in [
            r#"[{"model":""}]"#,
            r#"[{"model":"m","phases":[]}]"#,
            r#"[{"model":"m","max_input_chars":0}]"#,
            r#"[{"model":"m","parameters":{"messages":[]}}]"#,
            r#"[{"model":"m","parameters":{"max_tokens":1,"max_completion_tokens":2}}]"#,
        ] {
            assert!(parse(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn complete_context_and_capabilities_determine_eligibility() {
        let rules = parse(r#"[{"model":"small","max_input_chars":120}]"#).unwrap();
        for message in [
            json!({"role":"tool","content":"result"}),
            json!({"role":"assistant","tool_calls":[{"id":"1"}]}),
            json!({"role":"user","content":[{"type":"image_url","image_url":{"url":"image"}}]}),
            json!({"role":"user","content":"x".repeat(121)}),
        ] {
            let body =
                serde_json::to_vec(&json!({"model":"original","messages":[message]})).unwrap();
            assert_eq!(route(&body, &rules, Phase::Implement).unwrap(), body);
        }
        let body = br#"{ "model": "original", "messages": [{"role":"user","content":"hello"}] }"#;
        assert_eq!(route(body, &[], Phase::Repair).unwrap(), body);
        let routed: Value =
            serde_json::from_slice(&route(body, &rules, Phase::Implement).unwrap()).unwrap();
        assert_eq!(routed["model"], "small");
    }

    #[test]
    fn relay_changes_models_between_phases_and_preserves_auth_and_sse() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut received = Vec::new();
            for attempt in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut header = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    header.push_str(&line);
                }
                assert!(header.starts_with("POST /v1/chat/completions "));
                assert!(
                    header
                        .to_lowercase()
                        .contains("authorization: bearer test-only")
                );
                let length: usize = header
                    .lines()
                    .find_map(|l| {
                        l.to_lowercase()
                            .strip_prefix("content-length: ")
                            .map(|s| s.trim().parse().unwrap())
                    })
                    .unwrap();
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                received.push(serde_json::from_slice::<Value>(&body).unwrap());
                if attempt == 3 {
                    let body = r#"{"error":{"code":"insufficient_balance"}}"#;
                    write!(stream, "HTTP/1.1 402 Payment Required\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
                    continue;
                }
                let response = "data: {\"choices\":[]}\n\ndata: [DONE]\n\n";
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
            }
            received
        });
        let dir = tempfile::tempdir().unwrap();
        let rules = parse(r#"[{"model":"small","phases":["implement"]},{"model":"strong","phases":["repair"],"tools":true,"parameters":{"max_completion_tokens":200,"temperature":0.2}}]"#).unwrap();
        let relay = Relay::start(&upstream, rules, dir.path()).unwrap();
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        for label in ["implement", "repair", "final check", "design"] {
            relay.control.set_label(label);
            let mut request = json!({"model":"original","messages":[{"role":"user","content":"hello"}], "max_tokens":100,"thinking":{"type":"enabled"}, "stream":true});
            if label == "repair" {
                request["tools"] = json!([{"type":"function","function":{"name":"read"}}]);
            }
            let response = client
                .post(format!("{}/chat/completions", relay.base_url))
                .bearer_auth("test-only")
                .json(&request)
                .send()
                .unwrap();
            if label == "design" {
                assert_eq!(response.status().as_u16(), 402);
                assert_eq!(
                    response.json::<Value>().unwrap()["error"]["code"],
                    "insufficient_balance"
                );
                continue;
            }
            assert_eq!(response.headers()["content-type"], "text/event-stream");
            assert_eq!(
                response.text().unwrap(),
                "data: {\"choices\":[]}\n\ndata: [DONE]\n\n"
            );
        }
        let received = server.join().unwrap();
        assert_eq!(received[0]["model"], "small");
        assert!(received[0].get("thinking").is_none());
        assert_eq!(received[1]["model"], "strong");
        assert_eq!(received[1]["max_completion_tokens"], 200);
        assert!(received[1].get("max_tokens").is_none());
        assert_eq!(received[2]["model"], "original");
        assert_eq!(received[2]["thinking"]["type"], "enabled");
        let log = std::fs::read_to_string(dir.path().join("model-routes.jsonl")).unwrap();
        assert_eq!(log.lines().count(), 4);
        assert!(!log.contains("test-only"));
    }
}
