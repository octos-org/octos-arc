//! Environment filtering for subprocess-style tools.
//!
//! Gateway/profile secrets are available in the agent process so first-party
//! providers can resolve credentials. User-invoked subprocess tools should not
//! inherit those raw secrets unless the tool explicitly allowlists them.

use std::collections::HashSet;

use tokio::process::Command;

// The provider-secret machinery (the injection denylist, the heuristic, AND the
// runtime-registered-secret registry) lives in `octos_core::env_hygiene` (the
// bottom crate), so it is the SINGLE source consulted by BOTH this subprocess
// sanitiser AND octos-core's controller-side git sanitiser. A name registered
// via `register_secret_env_names` (re-exported unchanged at
// `octos_agent::register_secret_env_names`) is therefore stripped from EVERY
// spawned subprocess — sandboxed worker git ops AND controller-side git ops.
use crate::sandbox::BLOCKED_ENV_VARS;
pub use octos_core::register_secret_env_names;
use octos_core::{is_registered_secret_env_name, is_secret_env_name};

#[derive(Debug, Clone, Default)]
pub(crate) struct EnvAllowlist {
    names: HashSet<String>,
}

impl EnvAllowlist {
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    pub(crate) fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        Self {
            names: names.into_iter().map(normalize_env_name).collect(),
        }
    }

    fn contains(&self, name: &str) -> bool {
        self.names.contains(&normalize_env_name(name))
    }
}

/// Strict variant of [`should_forward_env_name`] for plugin tools that
/// declare a non-empty manifest env allowlist.
///
/// Default semantics ([`should_forward_env_name`]) gate **only** secret-like

fn normalize_env_name(name: &str) -> String {
    name.to_ascii_uppercase()
}

pub(crate) fn is_injection_env_name(name: &str) -> bool {
    BLOCKED_ENV_VARS
        .iter()
        .any(|blocked| name.eq_ignore_ascii_case(blocked))
}

pub(crate) fn should_forward_env_name(name: &str, allowlist: &EnvAllowlist) -> bool {
    if is_injection_env_name(name) {
        return false;
    }
    // Registered provider-key vars are treated like heuristic secrets: stripped
    // by default, but still allowlistable (skills that call LLMs declare them).
    let is_secret = is_secret_env_name(name) || is_registered_secret_env_name(name);
    !is_secret || allowlist.contains(name)
}

/// Strip provider/API-key and process-injection env vars from `cmd` using the
/// DEFAULT (empty-allowlist) policy — the exact sanitisation the shell tool
/// applies before every command ([`sanitize_command_env`] with
/// [`EnvAllowlist::empty`]). Public so an out-of-crate subprocess spawner that
/// runs at a LOWER trust level than the harness (e.g. the fleet worker's
/// sandboxed git ops, whose `.git` `filter.*`/hooks are worker-controlled) can
/// match the shell's secret hygiene and never hand controller credentials to
/// worker-planted code.
pub fn sanitize_default_subprocess_env(cmd: &mut Command) {
    sanitize_command_env(cmd, &EnvAllowlist::empty());
}

pub(crate) fn sanitize_command_env(cmd: &mut Command, allowlist: &EnvAllowlist) {
    for (key, _) in std::env::vars_os() {
        let Some(name) = key.to_str() else {
            continue;
        };
        if !should_forward_env_name(name, allowlist) {
            cmd.env_remove(&key);
        }
    }

    // Remove known code-injection variables even if they are not present in the
    // current process environment. This also clears values set earlier on `cmd`.
    for name in BLOCKED_ENV_VARS {
        cmd.env_remove(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_subprocess_env_strips_injection_vars() {
        // The public wrapper the fleet worker's sandboxed git ops use must strip
        // every process-injection var (defense the child inherits nothing
        // dangerous), matching the shell tool's empty-allowlist sanitisation.
        let mut cmd = Command::new("true");
        sanitize_default_subprocess_env(&mut cmd);
        let removed: Vec<String> = cmd
            .as_std()
            .get_envs()
            .filter_map(|(k, v)| {
                if v.is_none() {
                    Some(k.to_string_lossy().to_string())
                } else {
                    None
                }
            })
            .collect();
        for var in BLOCKED_ENV_VARS {
            assert!(removed.iter().any(|r| r == *var), "must strip {var}");
        }
    }

    #[test]
    fn detects_common_secret_env_names() {
        for name in [
            "OPENAI_API_KEY",
            "TAVILY_API_KEY",
            "SMTP_PASSWORD",
            "GITHUB_TOKEN",
            "GITHUB_PAT",
            "LARK_APP_SECRET",
            "SESSION_COOKIE",
            "AWS_SECRET_ACCESS_KEY",
            "private_key",
        ] {
            assert!(is_secret_env_name(name), "{name} should be secret");
        }
    }

    #[test]
    fn does_not_flag_common_non_secret_runtime_env_names() {
        for name in [
            "PATH",
            "HOME",
            "PWD",
            "USER",
            "OPENAI_BASE_URL",
            "OMINIX_API_URL",
            "OCTOS_PROFILE_ID",
            "PPT_TEMPLATE_DIR",
            "TOKENIZERS_PARALLELISM",
            "NPM_CONFIG_CACHE",
        ] {
            assert!(!is_secret_env_name(name), "{name} should not be secret");
        }
    }

    #[test]
    fn registered_secret_env_is_stripped_by_default_but_allowlistable() {
        // A custom `api_key_env` whose NAME the heuristic does NOT flag.
        let odd = "ZZ_PROVIDER_CREDS_FIXTURE";
        assert!(
            !is_secret_env_name(odd),
            "fixture must be a name the heuristic does not flag"
        );
        // Before registration it would be forwarded (not secret-looking) — this
        // is the gap: the shell tool could echo it.
        assert!(should_forward_env_name(odd, &EnvAllowlist::empty()));

        register_secret_env_names([odd]);

        // After registration it is treated as a secret: stripped from the
        // default (empty-allowlist) environment, so shell/hooks/validators
        // can't echo it...
        assert!(!should_forward_env_name(odd, &EnvAllowlist::empty()));
        // (case-insensitive)
        assert!(!should_forward_env_name(
            "zz_provider_creds_fixture",
            &EnvAllowlist::empty()
        ));
        // ...but a tool that declares it in its manifest allowlist may still
        // receive it (the sanctioned path for skills that call LLMs).
        let allowlisted = EnvAllowlist::from_names([odd]);
        assert!(should_forward_env_name(odd, &allowlisted));
    }

    #[test]
    fn allowlist_permits_secret_names_but_not_injection_names() {
        let allowlist = EnvAllowlist::from_names(["OPENAI_API_KEY", "LD_PRELOAD"]);

        assert!(should_forward_env_name("OPENAI_API_KEY", &allowlist));
        assert!(!should_forward_env_name("TAVILY_API_KEY", &allowlist));
        assert!(!should_forward_env_name("LD_PRELOAD", &allowlist));
        assert!(should_forward_env_name("OPENAI_BASE_URL", &allowlist));
    }
}
