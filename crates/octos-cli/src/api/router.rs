//! API router construction.

use std::collections::HashSet;
use std::sync::Arc;

use axum::Router;
use axum::extract::{DefaultBodyLimit, MatchedPath};
use axum::middleware::{self, Next};
use axum::routing::{get, post};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use url::Url;

use super::AppState;
use super::handlers;
use super::metrics;
use super::session_ingress;
use super::ui_protocol_transport;

/// Authentication identity extracted by the auth middleware.
#[derive(Clone, Debug)]
pub enum AuthIdentity {
    /// Local-trust callers are admin-equivalent (the multi-tenant user
    /// account system was removed with the dashboard).
    Admin,
    /// A scoped user identity (kept for sub-account profile authorization).
    User { id: String },
}

/// Return the matched route template for logging, never the raw request path.
///
/// Besides query credentials, some public endpoints carry credentials in path
/// parameters. A route template preserves useful request context while
/// replacing those values with placeholders. Unmatched paths use a fixed
/// marker rather than falling back to attacker-controlled input.
fn request_log_path<B>(request: &axum::http::Request<B>) -> &str {
    request
        .extensions()
        .get::<MatchedPath>()
        .map(MatchedPath::as_str)
        .unwrap_or("<unmatched>")
}

/// Build the HTTP request span without recording URI query parameters.
///
/// Some browser transports authenticate through query parameters (notably
/// WebSocket `?token=...`). `TraceLayer::new_for_http()` records the complete
/// URI by default, which would disclose those credentials whenever debug
/// request tracing is enabled.
fn make_http_trace_span(request: &axum::http::Request<axum::body::Body>) -> tracing::Span {
    tracing::debug_span!(
        "request",
        method = %request.method(),
        path = %request_log_path(request),
        version = ?request.version(),
    )
}

/// Compose the default CORS/browser-origin allowlist.
///
/// Local development origins only. Deployment origins are operator-supplied
/// via `appui.allowed_origins` / `OCTOS_APPUI_ALLOWED_ORIGINS` (plus the
/// bound loopback port, appended automatically at serve startup).
pub fn default_cors_allowlist() -> Vec<String> {
    vec![
        "http://localhost:3000".to_string(),
        "http://localhost:5173".to_string(),
        // octos-web Vite dev server (embedded same-origin at /app in prod, so
        // CORS is only needed when running the web app from `vite dev`).
        "http://localhost:5174".to_string(),
    ]
}

/// Compose the exact browser-origin allowlist shared by CORS and both
/// UI Protocol WebSocket upgrade gates.
///
/// Local development entries come first; operator-provided,
/// startup-normalized origins are appended in declaration order, with the
/// first occurrence winning.
pub(crate) fn browser_origin_allowlist(appui_allowed_origins: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    default_cors_allowlist()
        .into_iter()
        .chain(appui_allowed_origins.iter().cloned())
        .filter(|origin| seen.insert(origin.clone()))
        .collect()
}

/// Validate and normalize one exact browser origin.
///
/// Paths other than `/`, queries, fragments, credentials, opaque origins,
/// wildcards, and non-HTTP(S) schemes are deliberately rejected. Returning
/// `Url::origin()`'s ASCII serialization makes equivalent spellings (scheme
/// or host case, default ports, IDNs) compare exactly after startup.
fn normalize_appui_origin(raw: &str) -> eyre::Result<String> {
    eyre::ensure!(
        !raw.chars().any(char::is_control),
        "control characters are not valid in an exact browser origin"
    );
    let candidate = raw.trim();
    eyre::ensure!(!candidate.is_empty(), "origin is empty");
    eyre::ensure!(
        !candidate.eq_ignore_ascii_case("null"),
        "`null` is not a trusted browser origin"
    );
    eyre::ensure!(
        !candidate.contains('*'),
        "wildcards are not allowed; configure each exact origin"
    );
    eyre::ensure!(
        !candidate.contains('\\'),
        "backslashes are not valid in an exact browser origin"
    );

    let parsed =
        Url::parse(candidate).map_err(|error| eyre::eyre!("invalid origin URL: {error}"))?;
    eyre::ensure!(
        matches!(parsed.scheme(), "http" | "https"),
        "scheme must be http or https"
    );
    eyre::ensure!(parsed.host().is_some(), "origin must include a host");

    // `Url::username()` cannot distinguish no userinfo from an explicitly
    // empty username (`https://@host`), so also inspect the raw authority.
    let authority_and_suffix = candidate
        .find("://")
        .map(|scheme_end| &candidate[scheme_end + 3..])
        .ok_or_else(|| eyre::eyre!("origin must use `scheme://host` syntax"))?;
    let authority_end = authority_and_suffix
        .find(['/', '?', '#'])
        .unwrap_or(authority_and_suffix.len());
    let authority = &authority_and_suffix[..authority_end];
    let raw_suffix = &authority_and_suffix[authority_end..];
    eyre::ensure!(
        !authority.contains('@') && parsed.username().is_empty() && parsed.password().is_none(),
        "userinfo is not allowed in an origin"
    );
    eyre::ensure!(
        parsed.path() == "/",
        "origin must not include a non-root path"
    );
    eyre::ensure!(parsed.query().is_none(), "origin must not include a query");
    eyre::ensure!(
        parsed.fragment().is_none(),
        "origin must not include a fragment"
    );
    eyre::ensure!(
        raw_suffix.is_empty() || raw_suffix == "/",
        "origin must contain only scheme, authority, and an optional root slash"
    );

    Ok(parsed.origin().ascii_serialization())
}

/// Resolve the effective operator/loopback origin list at serve startup.
///
/// A non-empty `OCTOS_APPUI_ALLOWED_ORIGINS` value replaces the config list;
/// an absent or whitespace-only value leaves config authoritative. The
/// bound HTTP port's three loopback spellings are appended automatically.
/// HTTP serve resolves an ephemeral `--port 0` before calling this helper;
/// a literal `0` is only meaningful to non-HTTP callers and adds no origin.
pub(crate) fn resolve_appui_allowed_origins(
    configured: &[String],
    env_value: Option<&str>,
    serve_port: u16,
) -> eyre::Result<Vec<String>> {
    let env_override = env_value.filter(|value| !value.trim().is_empty());
    let selected: Vec<&str> = match env_override {
        Some(value) => value.split(',').collect(),
        None => configured.iter().map(String::as_str).collect(),
    };

    let source = if env_override.is_some() {
        "OCTOS_APPUI_ALLOWED_ORIGINS"
    } else {
        "appui.allowed_origins"
    };
    let mut normalized = Vec::new();
    let mut seen = HashSet::new();
    for (index, raw) in selected.into_iter().enumerate() {
        let origin = normalize_appui_origin(raw)
            .map_err(|error| eyre::eyre!("{source}[{index}] is invalid: {error}"))?;
        if seen.insert(origin.clone()) {
            normalized.push(origin);
        }
    }

    if serve_port != 0 {
        for raw in [
            format!("http://127.0.0.1:{serve_port}"),
            format!("http://localhost:{serve_port}"),
            format!("http://[::1]:{serve_port}"),
        ] {
            let origin = normalize_appui_origin(&raw)
                .expect("generated loopback origins are valid exact HTTP origins");
            if seen.insert(origin.clone()) {
                normalized.push(origin);
            }
        }
    }

    Ok(normalized)
}

/// Build the axum router with all API routes.
pub fn build_router(state: Arc<AppState>) -> Router {
    // Restrict CORS to an explicit allowlist of known origins.
    // Do NOT use suffix matching (e.g. ends_with(".example.com")) — a hijacked
    // subdomain would pass the check and enable cross-origin requests.
    //
    // The allowlist combines the local-dev defaults with the
    // startup-normalized operator origins. The same composition is used by
    // both WebSocket gates. CORS intentionally does not call
    // `allow_credentials`: bearer/work-secret auth is an independent layer
    // and must not become ambient browser authority.
    let allowed_origins: Arc<Vec<String>> =
        Arc::new(browser_origin_allowlist(&state.appui_allowed_origins));
    let cors = {
        let allowed = allowed_origins.clone();
        CorsLayer::new()
            .allow_origin(tower_http::cors::AllowOrigin::predicate(
                move |origin, _| {
                    let o = origin.to_str().unwrap_or("");
                    allowed.iter().any(|s| s == o)
                },
            ))
            .allow_methods(tower_http::cors::Any)
            // `Authorization` is a Fetch non-wildcard request header, so
            // `Access-Control-Allow-Headers: *` does not authorize the bearer
            // preflight used by AppUI. Mirror the browser's requested header
            // names after the exact Origin predicate has accepted the request.
            .allow_headers(tower_http::cors::AllowHeaders::mirror_request())
    };

    // Chat + status API (existing)
    //
    // Transport history:
    // - M9-α-5/α-6 (ADR PR #830 / audit issue #845): the chat SSE
    //   transport (`POST /api/chat?stream=true`, `GET /api/chat/stream`,
    //   `GET /api/sessions/:id/events/stream`) was deleted.
    // - Cleanup PR #908: the legacy text-frame `/api/ws` was retired
    //   (no live clients and it never carried the UI Protocol v1 wire
    //   format).
    // - Cleanup follow-up to #908: the surviving sync REST endpoint
    //   `POST /api/chat` was retired once the last callers
    //   (coding_multi_session integration test, three e2e specs,
    //   validate-m4-1a-live.sh) migrated to the canonical WS path.
    //
    // The sole chat transport is `/api/ui-protocol/ws`. The
    // harness/admin `/api/events/harness` SSE endpoint is unrelated to
    // chat and remains.
    //
    // M12 Phase D-5 (ADR PR #910 / audit PR #911): the auxiliary
    // session/status REST surface has been retired and replaced by the
    // WS UI Protocol v1 RPC methods on `/api/ui-protocol/ws`:
    //
    //   GET    /api/sessions                          → session/list
    //   GET    /api/sessions/{id}/messages            → session/messages_page
    //   GET    /api/sessions/{id}/status              → session/status.get
    //   GET    /api/sessions/{id}/files               → session/files.list
    //   GET    /api/sessions/{id}/tasks               → session/tasks.list
    //   GET    /api/sessions/{id}/workspace-contract  → session/workspace.get
    //   PATCH  /api/sessions/{id}/title               → session/title.set
    //   DELETE /api/sessions/{id}                     → session/delete
    //   GET    /api/status                            → system/status.get
    //
    // The handler functions are retained as private helpers because the
    // WS dispatcher in `ui_protocol.rs` still reuses them to back the
    // RPC methods above; only the REST route registrations are dropped.
    // Auth (`/api/auth/*`), blob (`/api/files/*`), task-control
    // (`/api/tasks/*`), chat ingress (`/api/ui-protocol/ws`), uploads,
    // and site-preview remain REST per the ADR.
    let chat_api = Router::new()
        .route(
            "/api/ui-protocol/ws",
            get(ui_protocol_transport::ws_handler),
        )
        .route(
            "/api/upload",
            post(handlers::upload).layer(DefaultBodyLimit::max(100 * 1024 * 1024)),
        )
        .route("/api/files/list", get(handlers::list_content_files))
        .route("/api/files/{filename}", get(handlers::serve_file))
        .route("/api/files", get(handlers::serve_file_by_query))
        // M7.9 / W2 — task supervisor exposure (kept REST). NOT an AppUI
        // duplicate of the WS `task/cancel` method: this is the channel/CLI
        // task-cancel path, also backed by the octos-bus API channel
        // (crates/octos-bus/src/api_channel.rs). See octos#1371 + spec §11.
        .route("/api/tasks/{task_id}/cancel", post(handlers::cancel_task))
        .route(
            "/api/tasks/{task_id}/restart-from-node",
            post(handlers::restart_task_from_node),
        );

    // Admin API routes (admin auth only, 1MB body limit)

    // Admin API routes (admin auth only, 1MB body limit)

    // Auth middleware was removed with the multi-tenant dashboard: all
    // HTTP callers are local-trust. stdio (the ARC path) never enters the
    // router at all.
    let protected = chat_api;

    // Metrics route — protected when auth is configured, public otherwise
    let metrics_route = Router::new().route("/metrics", get(metrics::metrics_handler));

    // Public version/health endpoints (no auth required)
    let version_routes = Router::new()
        .route("/api/version", get(handlers::version))
        .route("/health", get(handlers::health));

    // Unauthenticated routes (metrics + internal ingress + version)
    //
    // Issue #994 (P0 sev2 cross-tenant data read): `/api/preview/...`
    // used to live here. It now sits on the authenticated `chat_api`
    // group above — the handler asserts identity-owns-profile +
    // session-belongs-to-profile, so the URL tuple is no longer
    // sufficient to read another tenant's built site.
    //
    // Issue #1001 follow-up: the signed-URL preview route
    // `/api/preview-signed/{token}/{*path}` lives here so the SPA
    // iframe can GET it without `Authorization: Bearer ...`. The
    // token itself is the credential — `handlers::serve_signed_preview`
    // looks the token up in `AppState.preview_tokens`, re-validates the
    // issuer bearer, and re-checks identity ↔ profile authorisation
    // before serving content. Daemon restart drops the token cache and
    // every outstanding preview link invalidates with it.
    let public = Router::new()
        .merge(metrics_route)
        .route(
            "/v1/session_ingress/ws/{session_id}",
            get(session_ingress::ws_handler),
        )
        .merge(version_routes);

    // Layer 1 defence for issue #995 — the strip middleware runs OUTSIDE
    // every other route layer so an unauthenticated request can never
    // see an attacker-supplied `X-Profile-Id` on its way to a handler
    // (handler-level Layer 2 in `handlers::decide_resolved_profile_id`
    // is the second line of defence). Loopback / private connections are
    // considered trusted; everything else has the header stripped before
    // any handler / auth middleware runs.
    public
        .merge(protected)
        .layer(middleware::from_fn(strip_untrusted_profile_id_middleware))
        .layer(TraceLayer::new_for_http().make_span_with(make_http_trace_span))
        .layer(cors)
        .with_state(state)
}

/// Cached parsed list of trusted-proxy CIDRs from `OCTOS_TRUSTED_PROXY_CIDRS`.
///
/// Initialised once on first call. Empty if the env var is unset or no
/// entries parse. Each entry is `(network address as 16-byte big-endian, prefix bits)`,
/// with IPv4 mapped into the IPv4-in-IPv6 prefix (`::ffff:0:0/96`) so the
/// matcher can run a single big-endian bit comparison regardless of family.
/// Middleware: strip `X-Profile-Id` from requests that did not originate
/// from a trusted proxy.
///
/// This is the Layer 1 defence for issue #995. The header is meant to be
/// set by the operator's Caddy ingress, which talks to the daemon over
/// loopback — so anything not from loopback / a configured trusted
/// proxy is treated as forged and the header is removed before any
/// handler or downstream middleware can see it.
async fn strip_untrusted_profile_id_middleware(
    mut req: axum::http::Request<axum::body::Body>,
    next: Next,
) -> axum::response::Response {
    let remote_ip = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip());

    if !is_trusted_proxy_addr(remote_ip) && req.headers().contains_key("x-profile-id") {
        let raw = req
            .headers()
            .get("x-profile-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        req.headers_mut().remove("x-profile-id");
        tracing::warn!(
            target: "octos::api::auth",
            remote_addr = ?remote_ip,
            stripped_value = %raw,
            path = %request_log_path(&req),
            "X-Profile-Id stripped: request not from a trusted proxy (#995 hardening)"
        );
    }

    next.run(req).await
}

fn trusted_proxy_cidrs() -> &'static [TrustedProxyCidr] {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Vec<TrustedProxyCidr>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let raw = std::env::var("OCTOS_TRUSTED_PROXY_CIDRS").unwrap_or_default();
            let mut out = Vec::new();
            for entry in raw.split(',') {
                let entry = entry.trim();
                if entry.is_empty() {
                    continue;
                }
                match parse_cidr(entry) {
                    Some(cidr) => out.push(cidr),
                    None => tracing::warn!(
                        target: "octos::api::auth",
                        cidr = %entry,
                        "OCTOS_TRUSTED_PROXY_CIDRS entry could not be parsed; ignoring"
                    ),
                }
            }
            out
        })
        .as_slice()
}

/// Parsed trusted-proxy CIDR — IPv4 entries are normalised into the
/// IPv4-mapped-IPv6 space so a single 128-bit big-endian comparison
/// handles both families.
#[derive(Clone, Copy, Debug)]
struct TrustedProxyCidr {
    network: [u8; 16],
    prefix_bits: u8,
}

fn parse_cidr(entry: &str) -> Option<TrustedProxyCidr> {
    let (addr, prefix) = entry.split_once('/')?;
    let addr: std::net::IpAddr = addr.trim().parse().ok()?;
    let prefix: u8 = prefix.trim().parse().ok()?;

    let (octets, max_prefix) = match addr {
        std::net::IpAddr::V4(v4) => {
            // Map IPv4 into ::ffff:V4 (16 bytes, big-endian).
            let mut buf = [0u8; 16];
            buf[10] = 0xff;
            buf[11] = 0xff;
            buf[12..16].copy_from_slice(&v4.octets());
            (buf, 32u8)
        }
        std::net::IpAddr::V6(v6) => (v6.octets(), 128u8),
    };

    if prefix > max_prefix {
        return None;
    }

    // For IPv4 entries the prefix is on the last 32 bits, so add the
    // 96-bit IPv4-mapped prefix.
    let effective_prefix = match addr {
        std::net::IpAddr::V4(_) => 96 + prefix,
        std::net::IpAddr::V6(_) => prefix,
    };

    Some(TrustedProxyCidr {
        network: mask_to_prefix(octets, effective_prefix),
        prefix_bits: effective_prefix,
    })
}

fn mask_to_prefix(octets: [u8; 16], prefix_bits: u8) -> [u8; 16] {
    let mut out = octets;
    let full_bytes = (prefix_bits / 8) as usize;
    let remaining_bits = prefix_bits % 8;
    for byte in out.iter_mut().skip(full_bytes) {
        *byte = 0;
    }
    if full_bytes < 16 && remaining_bits > 0 {
        let mask = 0xffu8 << (8 - remaining_bits);
        out[full_bytes] = octets[full_bytes] & mask;
    }
    out
}

fn ip_matches_cidr(ip: std::net::IpAddr, cidr: &TrustedProxyCidr) -> bool {
    let octets = match ip {
        std::net::IpAddr::V4(v4) => {
            let mut buf = [0u8; 16];
            buf[10] = 0xff;
            buf[11] = 0xff;
            buf[12..16].copy_from_slice(&v4.octets());
            buf
        }
        std::net::IpAddr::V6(v6) => v6.octets(),
    };
    mask_to_prefix(octets, cidr.prefix_bits) == cidr.network
}

/// Decide whether a remote address is a trusted reverse-proxy.
///
/// The wildcard Caddy ingress in `scripts/install.sh` runs on the same
/// host as the daemon and `reverse_proxy localhost:NN`, so the
/// `X-Profile-Id` it sets always arrives over loopback. The fleet's
/// trust model is therefore: loopback ⇒ trusted, anything else ⇒ untrusted
/// unless the operator explicitly opts in via `OCTOS_TRUSTED_PROXY_CIDRS`
/// (a comma-separated list of CIDRs).
///
/// `None` for the remote addr means we couldn't read `ConnectInfo` —
/// e.g. axum tests built with `into_make_service()` rather than
/// `into_make_service_with_connect_info()`. In production this never
/// happens (`commands::serve` always uses the connect-info variant); in
/// tests we treat missing connect-info as untrusted so the strip path
/// is exercised. Tests that want to simulate a loopback hop must use
/// `into_make_service_with_connect_info::<SocketAddr>()`.
pub(crate) fn is_trusted_proxy_addr(addr: Option<std::net::IpAddr>) -> bool {
    let Some(addr) = addr else {
        return false;
    };
    if addr.is_loopback() {
        return true;
    }
    let cidrs = trusted_proxy_cidrs();
    cidrs.iter().any(|cidr| ip_matches_cidr(addr, cidr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::AppState;
    use crate::config::DeploymentMode;
    use axum::http::Request;
    use chrono::Utc;
    use std::io::Write;
    use std::sync::Arc;
    use tower::ServiceExt;

    #[derive(Clone, Default)]
    struct CapturedLogs(Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for CapturedLogs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl CapturedLogs {
        fn as_string(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }
}
