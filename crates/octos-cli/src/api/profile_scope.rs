//! Profile-scoping + authorization primitives shared by the stdio OUP
//! transport and the local-trust HTTP surface. The OTP/user-account login
//! machinery was removed with the multi-tenant dashboard; authorization is
//! local-trust (all local callers are admin-equivalent).

use axum::http::{HeaderMap, StatusCode};
use std::collections::HashMap;

use super::AppState;
use super::router::AuthIdentity;

pub const ADMIN_PROFILE_ID: &str = "admin";

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

fn host_scoped_profile_id(state: &AppState, headers: &HeaderMap) -> Option<String> {
    let host = request_host(headers)?;
    if is_local_request_host(&host) {
        return None;
    }

    let candidate = host.split('.').next()?;
    resolve_routed_profile_id_candidate(state, candidate)
}

/// Return `true` iff the authenticated identity is allowed to act as the
/// given profile id for `/api/my/*` endpoints.
///
/// Authorization rules:
/// - Admin token can act as any profile.
/// - A scoped user identity can act as its own profile.
/// - A user (top-level account) can also act as any sub-account they own
///   (ownership comes from the profile store's `parent_id`, not any user
///   registry — the multi-tenant user system was removed).
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

/// Ensure an admin profile exists in the store, creating one if needed.
fn ensure_admin_profile(ps: &crate::profiles::ProfileStore) -> Result<(), StatusCode> {
    if let Ok(Some(_)) = ps.get(ADMIN_PROFILE_ID) {
        return Ok(());
    }
    let profile = crate::profiles::UserProfile {
        id: ADMIN_PROFILE_ID.into(),
        name: "Admin".into(),
        public_subdomain: None,
        enabled: false,
        data_dir: None,
        parent_id: None,
        config: crate::profiles::ProfileConfig::default(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    ps.save(&profile).map_err(|e| {
        tracing::error!(error = %e, "failed to auto-create admin profile");
        StatusCode::INTERNAL_SERVER_ERROR
    })
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

/// Truncate a string to `max_len` chars (used by panel surfaces).
pub(crate) fn truncate_str(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_len).collect();
        format!("{truncated}...")
    }
}

pub(crate) fn relocate_secret_to_keychain(
    env_vars: &mut HashMap<String, String>,
    key: &str,
    profile_id: &str,
    store_available: bool,
    set_secret: impl Fn(&str, &str) -> eyre::Result<()>,
) -> Result<(), String> {
    let Some(value) = env_vars.get(key) else {
        return Ok(());
    };
    let value = value.trim().to_string();
    // Markers / masked / empty values mean "leave as configured".
    if !value.starts_with('{') {
        return Ok(());
    }
    if !store_available {
        return Err(format!(
            "{key}: keychain-backed credential storage is unavailable on this host (no secret store backend)"
        ));
    }
    let account = crate::auth::keychain::scoped_account(key, profile_id);
    set_secret(&account, &value).map_err(|e| format!("failed to store {key} in keychain: {e}"))?;
    env_vars.insert(key.to_string(), crate::auth::keychain::marker_for(&account));
    Ok(())
}
