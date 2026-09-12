//! Profile-scoping + authorization primitives shared by the stdio OUP
//! transport and the local-trust HTTP surface. The OTP/user-account login
//! machinery was removed with the multi-tenant dashboard; authorization is
//! now local-trust (all local callers are admin-equivalent).

use std::collections::HashMap;
use std::sync::Arc;

use axum::http::HeaderMap;

use axum::http::StatusCode;
use axum::Extension;

use super::AppState;
use super::router::AuthIdentity;

pub const ADMIN_PROFILE_ID: &str = "admin";

pub(crate) fn is_top_level_profile_id(state: &AppState, profile_id: &str) -> bool {
    state
        .profile_store
        .as_ref()
        .and_then(|store| store.get(profile_id).ok().flatten())
        .map(|profile| profile.parent_id.is_none())
        .unwrap_or(false)
}

pub(crate) fn scoped_host_allows_profile_id(
    _state: &AppState,
    scoped_profile_id: &str,
    candidate_profile_id: &str,
) -> bool {
    scoped_profile_id == candidate_profile_id
}

fn request_host(headers: &HeaderMap) -> Option<String> {
    let raw = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get("host"))?
        .to_str()
        .ok()?
        .split(',')
        .next()?
        .trim()
        .to_ascii_lowercase();
    if raw.is_empty() {
        return None;
    }
    Some(strip_port_from_host(&raw).to_string())
}

/// The endpoint URL a scanning client should dial: original authority
/// (host AND port — `request_host` strips the port, which would send
/// scanners to :80/:443, codex P2) plus `X-Forwarded-Proto` when a
/// reverse proxy supplies it, else https with a loopback http fallback.
fn request_endpoint(headers: &HeaderMap) -> Option<String> {
    let authority = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get("host"))?
        .to_str()
        .ok()?
        .split(',')
        .next()?
        .trim()
        .to_ascii_lowercase();
    if authority.is_empty() {
        return None;
    }
    let forwarded_proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|p| p == "http" || p == "https");
    let scheme = forwarded_proto.unwrap_or_else(|| {
        let host_only = strip_port_from_host(&authority);
        if host_only == "localhost" || host_only.starts_with("127.") || host_only == "::1" {
            "http".to_string()
        } else {
            "https".to_string()
        }
    });
    Some(format!("{scheme}://{authority}"))
}

fn strip_port_from_host(host: &str) -> &str {
    if let Some(stripped) = host.strip_prefix('[') {
        return stripped.split(']').next().unwrap_or(host);
    }

    if host.matches(':').count() == 1 {
        return host.split(':').next().unwrap_or(host);
    }

    host
}

fn is_local_request_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

fn resolve_routed_profile_id_candidate(state: &AppState, candidate: &str) -> Option<String> {
    let candidate = candidate.trim();
    if candidate.is_empty()
        || matches!(
            candidate,
            "www" | "app" | "admin" | "api" | "crew" | "octos"
        )
    {
        return None;
    }

    state
        .profile_store
        .as_ref()
        .and_then(|store| store.resolve_routable_profile_id(candidate).ok().flatten())
}

fn resolve_trusted_local_profile_id_candidate(state: &AppState, candidate: &str) -> Option<String> {
    let candidate = candidate.trim();
    if candidate.is_empty() {
        return None;
    }

    resolve_routed_profile_id_candidate(state, candidate).or_else(|| {
        state
            .profile_store
            .as_ref()
            .and_then(|store| store.get(candidate).ok().flatten())
            .map(|profile| profile.id)
    })
}

fn host_scoped_profile_id(state: &AppState, headers: &HeaderMap) -> Option<String> {
    let host = request_host(headers)?;
    if is_local_request_host(&host) {
        return None;
    }

    let candidate = host.split('.').next()?;
    resolve_routed_profile_id_candidate(state, candidate)
}

fn trusted_auth_scope_profile_id(state: &AppState, headers: &HeaderMap) -> Option<String> {
    if let Some(profile_id) = host_scoped_profile_id(state, headers) {
        return Some(profile_id);
    }

    let host = request_host(headers)?;
    if !is_local_request_host(&host) {
        return None;
    }

    headers
        .get("x-profile-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|candidate| resolve_trusted_local_profile_id_candidate(state, candidate))
}

/// Return `true` iff the authenticated identity is allowed to act as the
/// given profile id for `/api/my/*` endpoints.
///
/// Authorization rules:
/// - Admin token can act as any profile.
///   (matches the rest of the router which treats admin email sessions
///   as full admins for `/api/admin/*`). Without this carve-out, an
///   admin who logs in via OTP would 403 on tenant subdomains while
///   the bootstrap admin token would not — codex P2 (PR #958 review).
/// - A user can act as their own profile.
/// - A user (top-level account) can also act as any sub-account they own.
/// - Everyone else is denied (returns `false`).
pub(crate) fn is_authorized_for_profile(
    state: &AppState,
    identity: &AuthIdentity,
    profile_id: &str,
) -> bool {
    match identity {
        AuthIdentity::Admin => true,
        AuthIdentity::User { id } => {
            if id == profile_id {
                return true;
            }
            // Allow a top-level user to act as any of their sub-accounts.
            let Some(store) = state.profile_store.as_ref() else {
                return false;
            };
            match store.get(profile_id) {
                Ok(Some(profile)) => profile.parent_id.as_deref() == Some(id.as_str()),
                _ => false,
            }
        }
    }
}

/// Resolve the profile ID for "my" endpoints.
///
/// Server-side host-authoritative scoping (Option Y, closes #315):
/// 1. If the request `Host` / `X-Forwarded-Host` header resolves to a
///    tenant profile via `host_scoped_profile_id`, return that profile id
///    — but only after verifying the authenticated identity is allowed
///    to view it. If the identity is NOT authorized, return 403 rather
///    than silently falling through to the identity's default profile,
///    which would be both confusing and a cross-tenant data leak.
/// 2. Otherwise (no tenant subdomain, unknown host, or local request),
///    fall back to the identity-based default: admin token returns the
///    fixed admin profile id, user sessions return the user's own id.
///
/// For regular users, returns their user ID. For admin token, returns the admin's own profile ID
/// (auto-creating the admin profile if it doesn't exist yet).
pub(crate) fn resolve_my_profile_id(
    identity: &AuthIdentity,
    ps: &crate::profiles::ProfileStore,
    state: &AppState,
    headers: &HeaderMap,
) -> Result<String, StatusCode> {
    if let Some(scoped) = host_scoped_profile_id(state, headers) {
        if !is_authorized_for_profile(state, identity, &scoped) {
            tracing::warn!(
                identity = ?identity,
                scoped_profile = %scoped,
                "/api/my/* host-scope denied — identity not authorized for the tenant subdomain"
            );
            return Err(StatusCode::FORBIDDEN);
        }
        return Ok(scoped);
    }

    match identity {
        AuthIdentity::Admin => {
            ensure_admin_profile(ps)?;
            Ok(ADMIN_PROFILE_ID.into())
        }
        AuthIdentity::User { id, .. } => Ok(id.clone()),
    }
}

// Helper for `ui_protocol_transport::handle_content_list` (M12 Phase D-5).
// The REST route `GET /api/my/content` was retired in this milestone; the
// function survives as a private helper that the WS dispatcher calls
// directly to back the `content/list` RPC method. Downgraded to
// `pub(super)` so the public API surface no longer exposes a fn whose
// route was removed.
/// Helper backing the WS `content/list` RPC method (formerly `GET /api/my/content`).
pub(super) async fn my_content(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    axum::extract::Query(query): axum::extract::Query<crate::content_catalog::ContentQuery>,
) -> Result<Json<crate::content_catalog::ContentQueryResult>, (StatusCode, String)> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "not configured".into()))?;
    let mgr = state.content_catalog_mgr.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "content catalog not configured".into(),
    ))?;
    // Use X-Profile-Id header (from Caddy proxy) if available, otherwise resolve from identity.
    //
    // Codex P1 fix (PR #958 review): authorize the X-Profile-Id branch the
    // same way the host-scoped path does. Without `is_authorized_for_profile`
    // a bearer-authenticated user could pass any tenant id and read its
    // catalog, since the bearer auth completes before the middleware's
    // loopback-only X-Profile-Id check runs. The new check matches the
    // semantics enforced by `resolve_my_profile_id`'s host-scoped branch:
    // admin can target any tenant, users can target their own profile or
    // sub-accounts they own. Cross-tenant access returns 403.
    let profile = if let Some(pid) = headers.get("x-profile-id").and_then(|v| v.to_str().ok()) {
        if !is_authorized_for_profile(&state, &identity, pid) {
            tracing::warn!(
                identity = ?identity,
                requested_profile = %pid,
                "GET /api/my/content X-Profile-Id denied — identity not authorized for the profile"
            );
            return Err((StatusCode::FORBIDDEN, "forbidden".into()));
        }
        ps.get(pid)
            .map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "profile store error".into(),
                )
            })?
            .ok_or((StatusCode::NOT_FOUND, format!("profile '{pid}' not found")))?
    } else {
        resolve_my_profile(&identity, ps, &state, &headers)
            .map_err(|s| (s, "profile not found".into()))?
    };
    let data_dir = ps.resolve_data_dir(&profile);

    let catalog = mgr
        .get_catalog_with_scan(&profile.id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let cat = catalog.read().await;
    let result = cat.query(&query);
    let entries = result
        .entries
        .into_iter()
        .filter_map(|mut entry| {
            let handle =
                response_path_for_profile_file(&data_dir, std::path::Path::new(&entry.path))?;
            entry.path = handle;
            entry.thumbnail_path = entry
                .thumbnail_path
                .as_ref()
                .map(|_| "available".to_string());
            Some(entry)
        })
        .collect();
    Ok(Json(crate::content_catalog::ContentQueryResult {
        entries,
        total: result.total,
    }))
}

// Helper for `ui_protocol_transport::handle_content_delete` (M12 Phase D-5).
// The REST route `DELETE /api/my/content/{id}` was retired in this
// milestone; the function survives as a private helper backing the
// `content/delete` WS RPC method.
/// Helper backing the WS `content/delete` RPC method (formerly `DELETE /api/my/content/{id}`).
pub(super) async fn delete_my_content(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    Path(id): Path<String>,
) -> Result<Json<ActionResponse>, (StatusCode, String)> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "not configured".into()))?;
    let mgr = state.content_catalog_mgr.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "content catalog not configured".into(),
    ))?;
    let profile = resolve_my_profile(&identity, ps, &state, &headers)
        .map_err(|s| (s, "profile not found".into()))?;

    let catalog = mgr
        .get_catalog(&profile.id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut cat = catalog.write().await;
    let deleted = cat
        .delete(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(ActionResponse {
        ok: deleted,
        message: if deleted {
            Some("Content deleted.".into())
        } else {
            Some("Content not found.".into())
        },
    }))
}

// Helper for `ui_protocol_transport::handle_content_bulk_delete` (M12 Phase D-5).
// The REST route `POST /api/my/content/bulk-delete` was retired in this
// milestone; the function survives as a private helper backing the
// `content/bulk_delete` WS RPC method.
/// Helper backing the WS `content/bulk_delete` RPC method (formerly `POST /api/my/content/bulk-delete`).
pub(super) async fn bulk_delete_my_content(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    Json(req): Json<BulkDeleteRequest>,
) -> Result<Json<ActionResponse>, (StatusCode, String)> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "not configured".into()))?;
    let mgr = state.content_catalog_mgr.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "content catalog not configured".into(),
    ))?;
    let profile = resolve_my_profile(&identity, ps, &state, &headers)
        .map_err(|s| (s, "profile not found".into()))?;

    let catalog = mgr
        .get_catalog(&profile.id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut cat = catalog.write().await;
    let deleted = cat
        .bulk_delete(&req.ids)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(ActionResponse {
        ok: true,
        message: Some(format!("{deleted} item(s) deleted.")),
    }))
}

#[derive(Deserialize)]
pub(super) struct BulkDeleteRequest {
    pub ids: Vec<String>,
}
