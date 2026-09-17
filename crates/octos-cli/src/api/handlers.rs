//! Surviving non-REST handler helpers.
//!
//! slim5-batch4: the axum REST surface was removed with the HTTP router.
//! Only the keychain-relocation helper used by the AppUI profile/llm RPCs
//! remains.

/// Relocate keychain-backed secrets (e.g. a Vertex SA JSON supplied as a
/// route api_key) out of `env_vars` into the OS keychain before persisting,
/// so the AppUI profile RPCs can't write a private key to plaintext profile
/// config. Detection is by **content** (a raw service-account JSON), not just
/// the declared `VERTEX_SA_JSON` name.
pub(crate) fn relocate_keychain_backed_secrets(
    env_vars: &mut std::collections::HashMap<String, String>,
    profile_id: &str,
) -> Result<(), String> {
    let keys: Vec<String> = env_vars
        .iter()
        .filter(|(key, value)| crate::auth::keychain::needs_keychain_relocation(key, value))
        .map(|(key, _)| key.clone())
        .collect();
    for key in keys {
        super::profile_scope::relocate_secret_to_keychain(
            env_vars,
            &key,
            profile_id,
            crate::auth::keychain::is_available(),
            crate::auth::keychain::set_secret,
        )?;
    }
    Ok(())
}
