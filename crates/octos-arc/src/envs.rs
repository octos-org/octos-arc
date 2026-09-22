//! Environment vectors for child processes. The harness inherits the caller's
//! environment (node, npm, mirrors, PATH from the platform) and layers
//! overrides on top; `None` removes a variable.

use std::ffi::OsString;

pub type EnvVec = Vec<(OsString, OsString)>;

pub fn inherited(overrides: &[(&str, Option<&str>)]) -> EnvVec {
    let mut env: EnvVec = std::env::vars_os()
        .filter(|(key, _)| {
            !overrides
                .iter()
                .any(|(name, _)| key == name.as_ref() as &std::ffi::OsStr)
        })
        .collect();
    for (name, value) in overrides {
        if let Some(value) = value {
            env.push((OsString::from(name), OsString::from(value)));
        }
    }
    env
}

pub fn lookup<'a>(env: &'a EnvVec, name: &str) -> Option<&'a OsString> {
    env.iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_override_and_remove_variables() {
        let env = inherited(&[("PATH", Some("/x")), ("OCTOS_ARC_TEST_REMOVED", None)]);
        assert_eq!(lookup(&env, "PATH").unwrap(), "/x");
        assert!(lookup(&env, "OCTOS_ARC_TEST_REMOVED").is_none());
    }
}
