//! Profile-scoping + authorization primitives shared by the stdio OUP
//! transport and the local-trust HTTP surface. The OTP/user-account login
//! machinery was removed with the multi-tenant dashboard; authorization is
//! local-trust (all local callers are admin-equivalent).

use std::collections::HashMap;

use super::AppState;
use super::router::AuthIdentity;

pub const ADMIN_PROFILE_ID: &str = "admin";

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
