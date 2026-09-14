//! Profile-scoping helpers shared by the stdio OUP transport. The
//! local-trust HTTP surface, OTP/user-account login machinery and the
//! multi-tenant dashboard were removed; authorization is local-trust
//! (all local callers are admin-equivalent).

use std::collections::HashMap;

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
