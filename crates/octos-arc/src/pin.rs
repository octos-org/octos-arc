use std::path::Path;

use eyre::{Result, ensure, eyre};
use serde::{Deserialize, Serialize};

use crate::workspace::file_digest;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeRelease {
    pub version: String,
    pub source_commit: String,
    pub binary_sha256: String,
    pub target: String,
    pub archive_sha256: String,
    pub url: String,
}

#[derive(Debug, Deserialize)]
struct Lock {
    schema_version: u32,
    runtime_release: Option<RuntimeRelease>,
}

pub struct BinaryIdentity<'identity> {
    pub executable: &'identity Path,
    pub source_commit: &'identity str,
    pub target: &'identity str,
    pub dirty: bool,
}

fn hexadecimal(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn verify(path: &Path, identity: &BinaryIdentity<'_>) -> Result<RuntimeRelease> {
    let lock: Lock = serde_json::from_slice(&std::fs::read(path)?)?;
    ensure!(lock.schema_version == 1, "Unsupported runtime lock version");
    let release = lock
        .runtime_release
        .ok_or_else(|| eyre!("Source-only baseline: no fixed runtime release has been built"))?;
    ensure!(
        !identity.dirty,
        "Refusing a binary built from tracked uncommitted changes"
    );
    ensure!(
        hexadecimal(&release.source_commit, 40) && release.source_commit == identity.source_commit,
        "Runtime source commit mismatch"
    );
    ensure!(
        release.target == identity.target,
        "Runtime build target mismatch"
    );
    ensure!(
        !release.version.trim().is_empty()
            && !release.version.to_ascii_lowercase().contains("latest"),
        "Runtime version must be fixed"
    );
    let prefix = format!(
        "https://github.com/octos-org/octos-arc-runtime/releases/download/{}/",
        release.version
    );
    ensure!(
        release.url.starts_with(&prefix)
            && !release.url.contains(['?', '#'])
            && !release.url.contains("/../"),
        "Runtime URL must identify this downstream versioned release"
    );
    ensure!(
        hexadecimal(&release.archive_sha256, 64),
        "Missing archive checksum"
    );
    ensure!(
        hexadecimal(&release.binary_sha256, 64)
            && file_digest(identity.executable)? == release.binary_sha256,
        "Runtime binary checksum mismatch"
    );
    Ok(release)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rejects_source_only_and_wrong_binaries() {
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("octos");
        std::fs::write(&binary, "test executable bytes").unwrap();
        let lock = temp.path().join("lock.json");
        let identity = BinaryIdentity {
            executable: &binary,
            source_commit: "1111111111111111111111111111111111111111",
            target: "test",
            dirty: false,
        };
        std::fs::write(&lock, r#"{"schema_version":1,"runtime_release":null}"#).unwrap();
        assert!(
            verify(&lock, &identity)
                .unwrap_err()
                .to_string()
                .contains("Source-only")
        );
        let mut value = json!({"schema_version":1,"runtime_release":{"version":"arc-v1","source_commit":identity.source_commit,"target":"test","binary_sha256":file_digest(&binary).unwrap(),"archive_sha256":"a".repeat(64),"url":"https://github.com/octos-org/octos-arc-runtime/releases/download/arc-v1/runtime.tar.gz"}});
        std::fs::write(&lock, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(verify(&lock, &identity).is_ok());
        value["runtime_release"]["url"] =
            json!("https://github.com/octos-org/octos/releases/latest/download/runtime.tar.gz");
        std::fs::write(&lock, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(verify(&lock, &identity).is_err());
        value["runtime_release"]["url"] = json!(
            "https://github.com/octos-org/octos-arc-runtime/releases/download/arc-v1/runtime.tar.gz"
        );
        std::fs::write(&lock, serde_json::to_vec(&value).unwrap()).unwrap();
        std::fs::write(&binary, "other binary").unwrap();
        assert!(
            verify(&lock, &identity)
                .unwrap_err()
                .to_string()
                .contains("checksum")
        );
    }
}
