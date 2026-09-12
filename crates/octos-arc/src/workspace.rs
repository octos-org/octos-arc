use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;

use eyre::{Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};

pub fn file_digest(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub fn directory(path: &Path) -> Result<()> {
    if path.exists() || path.is_symlink() {
        ensure!(
            !path.is_symlink() && path.is_dir(),
            "Not a plain directory: {}",
            path.display()
        );
    } else {
        fs::create_dir(path)?;
    }
    Ok(())
}

pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    ensure!(
        !path.is_symlink(),
        "Refusing to write through symlink: {}",
        path.display()
    );
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    serde_json::to_writer_pretty(&mut temp, value)?;
    temp.write_all(b"\n")?;
    temp.flush()?;
    temp.persist(path)?;
    Ok(())
}

pub fn source_files(root: &Path) -> Result<BTreeMap<String, String>> {
    fn visit(
        root: &Path,
        dir: &Path,
        files: &mut BTreeMap<String, String>,
        depth: usize,
    ) -> Result<()> {
        ensure!(depth <= 64, "Project tree is too deep");
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if matches!(
                name.as_ref(),
                ".git" | ".arc" | "node_modules" | "target" | "dist" | "build" | ".next" | ".env"
            ) || name.starts_with(".env.")
            {
                continue;
            }
            let path = entry.path();
            ensure!(
                !path.is_symlink(),
                "Source symlink not supported: {}",
                path.display()
            );
            if path.is_dir() {
                visit(root, &path, files, depth + 1)?;
            } else {
                ensure!(
                    path.is_file(),
                    "Non-regular source entry: {}",
                    path.display()
                );
                ensure!(
                    files.len() < 10_000 && fs::metadata(&path)?.len() <= 32 * 1024 * 1024,
                    "Source snapshot limit exceeded"
                );
                files.insert(
                    path.strip_prefix(root)?.to_string_lossy().into_owned(),
                    file_digest(&path)?,
                );
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    visit(root, root, &mut files, 0)?;
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_sources_without_runtime_state() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("index.js"), "hello").unwrap();
        fs::create_dir(temp.path().join(".arc")).unwrap();
        fs::write(temp.path().join(".arc/report.json"), "{}").unwrap();
        let first = source_files(temp.path()).unwrap();
        assert_eq!(first.len(), 1);
        fs::write(temp.path().join("index.js"), "changed").unwrap();
        assert_ne!(first, source_files(temp.path()).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_source_and_report_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target.txt");
        fs::write(&target, "original").unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(source_files(temp.path()).is_err());
        assert!(write_json(&link, &serde_json::json!({})).is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), "original");
    }
}
