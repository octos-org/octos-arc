//! Provider/LLM diagnostic endpoints shared by the operator `my_api`
//! surface (moved here from the retired `api::admin` module).

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use eyre::WrapErr as _;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

use super::AppState;

#[derive(Deserialize)]
pub struct TestProviderRequest {
    pub provider: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    /// Route protocol override for model discovery (`"anthropic"` switches the
    /// listing strategy); absent means the family's declared protocol.
    #[serde(default)]
    pub api_type: Option<String>,
    #[serde(default)]
    pub profile_id: Option<String>,
}


#[derive(Serialize)]
pub struct TestProviderResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}


#[derive(Deserialize)]
pub struct TestSearchRequest {
    /// Search provider: "tavily", "perplexity", "brave", "you", "serper"
    pub provider: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Optional profile id whose saved env vars should be used.
    ///
    /// Admin dashboard pages use this when testing a non-admin profile. Regular
    /// users may only reference their own profile or child profiles.
    #[serde(default)]
    pub profile_id: Option<String>,
}


#[derive(Serialize)]
pub struct TestSearchResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}


fn resolve_profile_secret(env_name: &str, stored_value: Option<&str>) -> Option<String> {
    resolve_profile_secret_with_keychain(env_name, stored_value, |name| {
        crate::auth::keychain::get_secret(name).ok().flatten()
    })
}



fn resolve_saved_key(
    state: &AppState,
    identity: &Option<axum::Extension<super::router::AuthIdentity>>,
    req: &TestProviderRequest,
) -> Result<String, (StatusCode, String)> {
    let env_name = match &req.api_key_env {
        Some(name) if !name.is_empty() => name,
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                "No api_key or api_key_env provided".into(),
            ));
        }
    };

    // Get the user's profile from the store
    let ps = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "profile store not configured".into(),
    ))?;

    let profile_id = if let Some(ref pid) = req.profile_id {
        pid.clone()
    } else {
        match identity {
            Some(axum::Extension(super::router::AuthIdentity::User { id, .. })) => id.clone(),
            Some(axum::Extension(super::router::AuthIdentity::Admin)) => {
                super::profile_scope::ADMIN_PROFILE_ID.into()
            }
            None => {
                return Err((StatusCode::UNAUTHORIZED, "not authenticated".into()));
            }
        }
    };

    let profile = ps
        .get(&profile_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "profile not found".into()))?;

    let raw = profile
        .config
        .env_vars
        .get(env_name)
        .cloned()
        .unwrap_or_default();
    // Resolve a `keychain:` marker to the real secret (e.g. a Vertex SA JSON
    // stored in the OS keychain); plain values pass through unchanged.
    Ok(crate::auth::keychain::resolve_value(env_name, &raw).unwrap_or_default())
}



fn resolve_saved_search_key(
    state: &AppState,
    identity: &Option<axum::Extension<super::router::AuthIdentity>>,
    req: &TestSearchRequest,
) -> Result<String, (StatusCode, String)> {
    let env_name = match req
        .api_key_env
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .or_else(|| default_search_api_env(req.provider.as_str()))
    {
        Some(name) => name,
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                "No api_key or api_key_env provided".into(),
            ));
        }
    };

    let ps = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "profile store not configured".into(),
    ))?;

    let profile_id = resolve_test_search_profile_id(identity, req.profile_id.as_deref())?;

    let profile = ps
        .get(&profile_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "profile not found".into()))?;

    let stored = profile.config.env_vars.get(env_name).map(String::as_str);
    Ok(resolve_profile_secret(env_name, stored).unwrap_or_default())
}



fn default_search_api_env(provider: &str) -> Option<&'static str> {
    match provider {
        "tavily" => Some("TAVILY_API_KEY"),
        "perplexity" => Some("PERPLEXITY_API_KEY"),
        "brave" => Some("BRAVE_API_KEY"),
        "you" => Some("YDC_API_KEY"),
        "serper" => Some("SERPER_API_KEY"),
        _ => None,
    }
}



fn resolve_test_search_profile_id(
    identity: &Option<axum::Extension<super::router::AuthIdentity>>,
    requested_profile_id: Option<&str>,
) -> Result<String, (StatusCode, String)> {
    let requested_profile_id = requested_profile_id
        .map(str::trim)
        .filter(|id| !id.is_empty());

    match identity {
        Some(axum::Extension(super::router::AuthIdentity::Admin)) => Ok(requested_profile_id
            .unwrap_or(super::profile_scope::ADMIN_PROFILE_ID)
            .to_string()),
        Some(axum::Extension(super::router::AuthIdentity::User { id, .. })) => {
            let Some(requested) = requested_profile_id else {
                return Ok(id.clone());
            };
            let child_prefix = format!("{id}--");
            if requested == id || requested.starts_with(&child_prefix) {
                Ok(requested.to_string())
            } else {
                Err((
                    StatusCode::FORBIDDEN,
                    "cannot test search keys for another profile".into(),
                ))
            }
        }
        None => Err((StatusCode::UNAUTHORIZED, "not authenticated".into())),
    }
}



fn resolve_profile_secret_with_keychain<F>(
    env_name: &str,
    stored_value: Option<&str>,
    mut keychain_lookup: F,
) -> Option<String>
where
    F: FnMut(&str) -> Option<String>,
{
    let secret = match stored_value {
        Some(value) if value == crate::auth::KEYCHAIN_MARKER => keychain_lookup(env_name),
        Some(value) if !value.trim().is_empty() => Some(value.to_string()),
        _ => None,
    };

    secret
        .or_else(|| std::env::var(env_name).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}



fn base_url_targets_link_local(base_url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(base_url) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => v4.is_link_local(),
        Ok(std::net::IpAddr::V6(v6)) => {
            (v6.segments()[0] & 0xffc0) == 0xfe80
                || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_link_local())
        }
        Err(_) => false,
    }
}




/// POST /api/admin/test-provider or /api/my/test-provider
///
/// Verify an LLM provider/model/key combo works. Accepts either:
/// - `api_key`: raw key (for newly entered, unsaved keys)
/// - `api_key_env`: env var name to resolve from the user's saved profile
///   (used when the key is already saved and the frontend only has the masked value)
pub async fn test_provider(
    State(state): State<Arc<AppState>>,
    identity: Option<axum::Extension<super::router::AuthIdentity>>,
    Json(req): Json<TestProviderRequest>,
) -> Result<Json<TestProviderResponse>, (StatusCode, String)> {
    use octos_core::{Message, MessageRole};
    use octos_llm::{ChatConfig, LlmProvider};

    // Resolve the API key: prefer raw api_key, fall back to reading from saved profile
    let keyless = octos_llm::registry::is_keyless(&req.provider);
    let resolved = if let Some(ref key) = req.api_key {
        if !key.is_empty() && !key.contains("***") {
            Ok(key.clone())
        } else {
            resolve_saved_key(&state, &identity, &req)
        }
    } else {
        resolve_saved_key(&state, &identity, &req)
    };
    // Keyless local families (local/ollama/vllm) construct without a key —
    // both an EMPTY key and an UNRESOLVABLE key (no api_key/api_key_env in
    // the request at all) are fine for them. Dead-ending here blocked the
    // keyless onboarding flow entirely (red-team pass + live test).
    let api_key = match resolved {
        Ok(key) => key,
        Err(_) if keyless => String::new(),
        Err(error) => return Err(error),
    };

    if api_key.is_empty() && !keyless {
        return Ok(Json(TestProviderResponse {
            ok: false,
            message: String::new(),
            error: Some("No API key provided".into()),
        }));
    }

    // Link-local (cloud metadata) targets are never model servers — refuse
    // before any outbound request (adversarial review, octos#2097).
    if req
        .base_url
        .as_deref()
        .is_some_and(base_url_targets_link_local)
    {
        return Ok(Json(TestProviderResponse {
            ok: false,
            message: String::new(),
            error: Some("base_url targets a link-local/metadata address — refused".into()),
        }));
    }

    let provider: Arc<dyn LlmProvider> = {
        let params = octos_llm::registry::CreateParams {
            // Empty means "keyless family" — let the factory apply its own
            // fallback instead of sending an empty Bearer token.
            api_key: (!api_key.is_empty()).then(|| api_key.clone()),
            model: Some(req.model.clone()),
            base_url: req.base_url.clone(),
            model_hints: None,
            llm_timeout_secs: None,
            llm_connect_timeout_secs: None,
        };
        match octos_llm::registry::lookup(&req.provider) {
            Some(entry) => (entry.create)(params)
                .map_err(|e| (StatusCode::BAD_REQUEST, format!("provider error: {e:#}")))?,
            None => {
                // Unknown provider — assume OpenAI-compatible with custom base URL.
                let url = req
                    .base_url
                    .as_deref()
                    .unwrap_or("https://api.openai.com/v1");
                Arc::new(
                    octos_llm::openai::OpenAIProvider::new(&api_key, &req.model)
                        .with_base_url(url)
                        .with_provider_label(&req.provider),
                )
            }
        }
    };

    let messages = vec![Message {
        role: MessageRole::User,
        content: "Say OK".into(),
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        client_message_id: None,
        thread_id: None,
        timestamp: chrono::Utc::now(),
    }];
    // Gemini 2.5+ "thinking" models consume tokens on internal reasoning,
    // so 16 tokens is too small — they return empty content.  Use 128 for
    // Gemini and keep 16 for everyone else (fast, cheap connectivity check).
    // Resolve aliases through the registry: the web settings UI historically
    // sends `google`, which is the registered alias for `gemini`. Treating the
    // alias as an unrelated provider left the connectivity probe with only 16
    // output tokens and caused thinking-capable Gemini models to return a
    // truncated/empty candidate that was then reported as a connection error.
    let canonical_provider = octos_llm::registry::lookup(&req.provider)
        .map(|entry| entry.name)
        .unwrap_or(req.provider.as_str());
    let max_tokens = if canonical_provider == "gemini" || canonical_provider == "vertex" {
        128
    } else {
        16
    };
    let config = ChatConfig {
        max_tokens: Some(max_tokens),
        temperature: Some(0.0),
        ..Default::default()
    };

    match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        provider.chat(&messages, &[], &config),
    )
    .await
    {
        Ok(Ok(resp)) => {
            tracing::info!(provider = %req.provider, model = %req.model, "test-provider succeeded");
            Ok(Json(TestProviderResponse {
                ok: true,
                message: resp.content.unwrap_or_default(),
                error: None,
            }))
        }
        Ok(Err(e)) => {
            tracing::warn!(provider = %req.provider, model = %req.model, error = %e, "test-provider failed");
            Ok(Json(TestProviderResponse {
                ok: false,
                message: String::new(),
                error: Some(format!("{e:#}")),
            }))
        }
        Err(_) => {
            tracing::warn!(provider = %req.provider, model = %req.model, "test-provider timed out");
            Ok(Json(TestProviderResponse {
                ok: false,
                message: String::new(),
                error: Some("Request timed out after 30 seconds".into()),
            }))
        }
    }
}


/// POST /api/my/provider-models — fetch available models from a provider's API.
pub async fn provider_models(
    State(state): State<Arc<AppState>>,
    identity: Option<axum::Extension<super::router::AuthIdentity>>,
    Json(req): Json<TestProviderRequest>,
) -> Result<Json<Vec<String>>, (StatusCode, String)> {
    let keyless = octos_llm::registry::is_keyless(&req.provider);
    let resolved = if let Some(ref key) = req.api_key {
        if !key.is_empty() && !key.contains("***") {
            Ok(key.clone())
        } else {
            resolve_saved_key(&state, &identity, &req)
        }
    } else {
        resolve_saved_key(&state, &identity, &req)
    };
    // Keyless local families (local/ollama/vllm) list models without a key —
    // their /v1/models answers unauthenticated (octos#2096 review round).
    let api_key = match resolved {
        Ok(key) => key,
        Err(_) if keyless => String::new(),
        Err(error) => return Err(error),
    };
    if api_key.is_empty() && !keyless {
        return Err((StatusCode::BAD_REQUEST, "No API key".into()));
    }
    // Link-local (cloud metadata) targets are never model servers — refuse
    // before any outbound request (adversarial review, octos#2097).
    if req
        .base_url
        .as_deref()
        .is_some_and(base_url_targets_link_local)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "base_url targets a link-local/metadata address".into(),
        ));
    }
    // Protocol-aware discovery shared with the AppUI `profile/llm/
    // fetch_models` surface — the strategy resolves from the route (api_type
    // override, then the family's declared protocol — per-model for families
    // like r9s that pick the wire protocol by model name), never from the
    // literal family id, so the two clients cannot drift.
    let route = octos_llm::discovery::resolve_model_discovery(
        Some(&req.provider),
        req.api_type.as_deref(),
        (!req.model.trim().is_empty()).then_some(req.model.trim()),
        req.base_url.as_deref(),
    );
    let outcome = octos_llm::discovery::discover_models(
        &route,
        &api_key,
        req.base_url.as_deref(),
        Some(&req.provider),
    )
    .await;
    match outcome {
        // Success — including an empty catalog, which is data, not an error.
        octos_llm::discovery::DiscoveryOutcome::Discovered(models) => Ok(Json(models)),
        // Advisory: this family has no model-list endpoint. Not an error —
        // manual model-id entry, Test, and Save stay fully available, and the
        // dashboard treats an empty list as "nothing to suggest".
        octos_llm::discovery::DiscoveryOutcome::Unsupported(_) => Ok(Json(Vec::new())),
        other => Err((
            if other.status_label() == "rate_limited" {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::BAD_GATEWAY
            },
            format!(
                "{}: {}",
                other.status_label(),
                other.message().unwrap_or_default()
            ),
        )),
    }
}


/// POST /api/my/test-search
///
/// Verify a web search API key works. Makes a minimal search request.
pub async fn test_search(
    State(state): State<Arc<AppState>>,
    identity: Option<axum::Extension<super::router::AuthIdentity>>,
    Json(req): Json<TestSearchRequest>,
) -> Result<Json<TestSearchResponse>, (StatusCode, String)> {
    // Resolve the API key
    let api_key = if let Some(ref key) = req.api_key {
        if !key.is_empty() && !key.contains("***") {
            key.clone()
        } else {
            resolve_saved_search_key(&state, &identity, &req)?
        }
    } else {
        resolve_saved_search_key(&state, &identity, &req)?
    };

    if api_key.is_empty() {
        return Ok(Json(TestSearchResponse {
            ok: false,
            message: String::new(),
            error: Some("No API key provided".into()),
        }));
    }

    let client = reqwest::Client::new();
    let query = "test";

    let result = match req.provider.as_str() {
        "tavily" => {
            let body = serde_json::json!({
                "query": query,
                "max_results": 1,
                "include_answer": false,
            });
            let resp = client
                .post("https://api.tavily.com/search")
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {api_key}"))
                .json(&body)
                .send()
                .await
                .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
            if resp.status().is_success() {
                Ok("Tavily Search API connected successfully".to_string())
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                Err(format!("Tavily API error ({status}): {body}"))
            }
        }
        "perplexity" => {
            let body = serde_json::json!({
                "model": "sonar",
                "messages": [{"role": "user", "content": query}],
                "max_tokens": 32
            });
            let resp = client
                .post("https://api.perplexity.ai/chat/completions")
                .header("Authorization", format!("Bearer {api_key}"))
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await
                .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
            if resp.status().is_success() {
                Ok("Perplexity Sonar API connected successfully".to_string())
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                Err(format!("Perplexity API error ({status}): {body}"))
            }
        }
        "brave" => {
            let resp = client
                .get("https://api.search.brave.com/res/v1/web/search")
                .header("X-Subscription-Token", &api_key)
                .header("Accept", "application/json")
                .query(&[("q", query), ("count", "1")])
                .send()
                .await
                .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
            if resp.status().is_success() {
                Ok("Brave Search API connected successfully".to_string())
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                Err(format!("Brave Search API error ({status}): {body}"))
            }
        }
        "you" => {
            let resp = client
                .get("https://ydc-index.io/v1/search")
                .header("X-API-Key", &api_key)
                .query(&[("query", query), ("count", "1")])
                .send()
                .await
                .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
            if resp.status().is_success() {
                Ok("You.com Search API connected successfully".to_string())
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                Err(format!("You.com API error ({status}): {body}"))
            }
        }
        "serper" => {
            let body = serde_json::json!({
                "q": query,
                "num": 1,
            });
            let resp = client
                .post("https://google.serper.dev/search")
                .header("X-API-KEY", &api_key)
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await
                .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
            if resp.status().is_success() {
                Ok("Serper Search API connected successfully".to_string())
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                Err(format!("Serper API error ({status}): {body}"))
            }
        }
        other => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("Unknown search provider: {other}"),
            ));
        }
    };

    match result {
        Ok(msg) => Ok(Json(TestSearchResponse {
            ok: true,
            message: msg,
            error: None,
        })),
        Err(err) => Ok(Json(TestSearchResponse {
            ok: false,
            message: String::new(),
            error: Some(err),
        })),
    }
}


/// GET /api/admin/model-limits — returns model catalog (runtime source of truth).
pub async fn model_limits() -> Json<serde_json::Value> {
    // Read the runtime catalog from the profile data dir
    let home = std::env::var("HOME").unwrap_or_default();
    for base in &[
        format!("{home}/.octos/profiles"),
        format!("{home}/.crew/profiles"),
    ] {
        if let Ok(entries) = std::fs::read_dir(base) {
            for entry in entries.flatten() {
                let path = entry.path().join("data/model_catalog.json");
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) {
                        return Json(value);
                    }
                }
            }
        }
    }
    // Fallback to shared catalog
    let shared = format!("{home}/.octos/model_catalog.json");
    if let Ok(content) = std::fs::read_to_string(&shared) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) {
            return Json(value);
        }
    }
    Json(serde_json::json!({"models": []}))
}


