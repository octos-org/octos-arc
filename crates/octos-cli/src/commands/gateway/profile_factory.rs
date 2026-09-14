//! Profile-scoped plugin/skill environment construction.
//!
//! The live remnants of the former gateway profile factory: the helpers that
//! resolve the plugin/skill environment for a profile. The actor-factory
//! builder that used to share this module was removed with the gateway
//! subsystem.

use crate::config::detect_provider;

const FIRST_PARTY_SKILL_ENV_VARS: &[&str] = &[
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "GEMINI_API_KEY",
    "GEMINI_BASE_URL",
    "GOOGLE_API_KEY",
    "GOOGLE_BASE_URL",
    "GOOGLE_CLOUD_PROJECT",
    "GOOGLE_CLOUD_LOCATION",
    "VERTEX_BASE_URL",
    "DASHSCOPE_API_KEY",
    "DASHSCOPE_BASE_URL",
    "ARK_API_KEY",
    "ARK_BASE_URL",
];

/// Google / Vertex credential material: the raw service-account JSON, the
/// application-default-credentials path, and OAuth access tokens. Unlike
/// [`FIRST_PARTY_SKILL_ENV_VARS`], these are forwarded to skill processes
/// ONLY when the profile's own provider chain (primary or a fallback)
/// resolves to a Google-family provider — a profile that merely has Vertex
/// credentials configured but routes through a different provider must not
/// hand its SA JSON to every skill subprocess.
///
/// The names are also force-registered via
/// [`octos_agent::register_secret_env_names`]: `VERTEX_SA_JSON` in
/// particular does not look secret to the `is_secret_env_name` heuristic,
/// and the provider-build-time registration in `Config::resolve_api_key`
/// only fires when Vertex is the ACTIVE provider.
const GOOGLE_VERTEX_CREDENTIAL_ENV_VARS: &[&str] = &[
    "GOOGLE_APPLICATION_CREDENTIALS",
    "VERTEX_SA_JSON",
    "VERTEX_ACCESS_TOKEN",
    "GOOGLE_OAUTH_ACCESS_TOKEN",
];

/// Provider families that authenticate against Google Cloud credentials.
/// Mirrors the provider-name spellings `build_plugin_env` special-cases.
fn is_google_family_provider(provider: &str) -> bool {
    matches!(
        provider,
        "gemini" | "google" | "vertex" | "vertex-ai" | "vertexai"
    )
}

/// True when the profile's provider chain (primary or any fallback)
/// resolves to a Google-family provider. Mirrors the runtime provider
/// resolution: explicit `family_id` first, else `detect_provider` on the
/// selection's model id.
fn profile_uses_google_family_provider(profile: &crate::profiles::UserProfile) -> bool {
    profile.config.llm.as_ref().is_some_and(|llm| {
        llm.primary
            .iter()
            .chain(llm.fallbacks.iter())
            .any(|selection| {
                selection
                    .family_id
                    .as_deref()
                    .or_else(|| selection.model_id.as_deref().and_then(detect_provider))
                    .is_some_and(is_google_family_provider)
            })
    })
}

fn push_env_once(env: &mut Vec<(String, String)>, key: impl Into<String>, value: String) {
    let key = key.into();
    if value.is_empty() || env.iter().any(|(existing, _)| existing == &key) {
        return;
    }
    env.push((key, value));
}

pub fn profile_plugin_env(profile: &crate::profiles::UserProfile) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    let resolved_env_vars = crate::auth::keychain::resolve_env_vars(&profile.config.env_vars);
    for key in FIRST_PARTY_SKILL_ENV_VARS {
        if let Some(value) = resolved_env_vars
            .get(*key)
            .cloned()
            .or_else(|| std::env::var(key).ok())
        {
            push_env_once(&mut env, *key, value);
        }
    }

    // Give first-party skills the same non-secret model selection that the
    // profile runtime uses. Skills still declare these names in their manifest
    // before the strict environment gate will expose them, and an explicit
    // skill-specific override remains free to take precedence inside the
    // skill. Provider credentials continue through the existing, separately
    // allowlisted secret path above.
    if let Some(primary) = profile
        .config
        .llm
        .as_ref()
        .and_then(|llm| llm.primary.as_ref())
    {
        if let Some(provider) = primary
            .family_id
            .as_deref()
            .or_else(|| primary.model_id.as_deref().and_then(detect_provider))
        {
            push_env_once(&mut env, "OCTOS_PROFILE_LLM_PROVIDER", provider.to_string());
        }
        if let Some(model) = primary.model_id.as_deref() {
            push_env_once(&mut env, "OCTOS_PROFILE_LLM_MODEL", model.to_string());
        }
    }

    // Google / Vertex credentials: ALWAYS secret-register the names (their
    // values may sit in the process env regardless of the active provider,
    // and `VERTEX_SA_JSON` doesn't trip the secret-name heuristic), but
    // forward them ONLY to skills of a profile whose provider chain actually
    // uses a Google-family provider. This keeps the sanctioned path working —
    // a Vertex-routed profile's skills still receive the SA JSON — without
    // leaking it into every skill subprocess of unrelated profiles.
    octos_agent::register_secret_env_names(GOOGLE_VERTEX_CREDENTIAL_ENV_VARS.iter().copied());
    if profile_uses_google_family_provider(profile) {
        for key in GOOGLE_VERTEX_CREDENTIAL_ENV_VARS {
            if let Some(value) = resolved_env_vars
                .get(*key)
                .cloned()
                .or_else(|| std::env::var(key).ok())
            {
                push_env_once(&mut env, *key, value);
            }
        }
    }

    // Smart-home bridge: forward the RESOLVED bridge config to the
    // `smart-home` skill as `SMART_HOME_BRIDGE_URL` / `SMART_HOME_BRIDGE_TOKEN`.
    // Callers pass the runtime-resolved profile (parent + defaults merged) and
    // `resolved_env_vars` is keychain-aware, so this covers two cases the
    // skill's own profile-JSON fallback cannot: a sub-account inheriting
    // `config.smart_home` from its parent, and a `token_env` whose value is a
    // keychain marker.
    if let Some(smart_home) = profile.config.smart_home.as_ref() {
        for (key, value) in smart_home.to_env_vars(&resolved_env_vars) {
            push_env_once(&mut env, key, value);
        }
    }

    env
}
