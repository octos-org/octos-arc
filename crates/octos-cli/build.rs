use std::process::Command;

fn main() {
    let source_commit = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_default();
    let source_dirty = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .map(|output| !output.status.success() || !output.stdout.is_empty())
        .unwrap_or(true);
    println!("cargo:rustc-env=OCTOS_GIT_SHA={source_commit}");
    println!("cargo:rustc-env=OCTOS_SOURCE_DIRTY={source_dirty}");
    println!(
        "cargo:rustc-env=OCTOS_BUILD_TARGET={}",
        std::env::var("TARGET").unwrap_or_default()
    );
    // Git short hash
    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    println!("cargo:rustc-env=OCTOS_GIT_HASH={hash}");

    // Build date (YYYY-MM-DD)
    let date = Command::new("date")
        .arg("+%Y-%m-%d")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    println!("cargo:rustc-env=OCTOS_BUILD_DATE={date}");

    // Re-run if git HEAD changes
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads/");
}
