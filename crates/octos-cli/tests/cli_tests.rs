//! Integration tests for the octos CLI.

use std::process::Command;

/// Get the path to the octos binary.
fn octos_binary() -> std::path::PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // Remove test binary name
    path.pop(); // Remove deps
    path.push("octos");
    path
}

#[test]
fn test_help_command() {
    let output = Command::new(octos_binary())
        .arg("--help")
        .output()
        .expect("Failed to execute command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("octos"));
    assert!(stdout.contains("chat"));
    assert!(stdout.contains("status"));
}

#[test]
fn test_version_command() {
    let output = Command::new(octos_binary())
        .arg("--version")
        .output()
        .expect("Failed to execute command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("octos"));
}

#[test]
fn test_chat_help() {
    let output = Command::new(octos_binary())
        .args(["chat", "--help"])
        .output()
        .expect("Failed to execute command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--provider"));
    assert!(stdout.contains("--model"));
    assert!(stdout.contains("--message"));
    assert!(stdout.contains("--verbose"));
}

// ── Skill system tests ──────────────────────────────────────────────

#[test]
fn test_skills_help() {
    let output = Command::new(octos_binary())
        .args(["skills", "--help"])
        .output()
        .expect("Failed to execute command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("list"));
    assert!(stdout.contains("install"));
    assert!(stdout.contains("remove"));
    assert!(stdout.contains("search"));
}

#[test]
fn test_skills_list() {
    let output = Command::new(octos_binary())
        .args(["skills", "list"])
        .output()
        .expect("Failed to execute command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Installed") || stdout.contains("skill"),
        "skills list should show installed skills"
    );
}

/// Search the octos-hub registry for mofa skills.
#[test]
#[ignore] // Requires network access to GitHub
fn test_skills_search_registry() {
    let output = Command::new(octos_binary())
        .args(["skills", "search", "mofa"])
        .output()
        .expect("Failed to execute command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("mofa-skills"),
        "registry should contain mofa-skills package"
    );
    assert!(
        stdout.contains("mofa-org/mofa-skills"),
        "should show install command"
    );
}

/// Search registry for a non-existent skill.
#[test]
#[ignore] // Requires network access to GitHub
fn test_skills_search_no_results() {
    let output = Command::new(octos_binary())
        .args(["skills", "search", "xyznonexistent99"])
        .output()
        .expect("Failed to execute command");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No matching") || stdout.is_empty() || !stdout.contains("Install:"),
        "should not find nonexistent skills"
    );
}

/// Install a skill from GitHub, verify it appears in list, then remove it.
#[test]
#[ignore] // Requires network access to GitHub + git
fn test_skills_install_and_remove() {
    let skill_name = "mofa-cards";
    let repo = "mofa-org/mofa-skills/mofa-cards";

    // Remove first in case it's already installed
    let _ = Command::new(octos_binary())
        .args(["skills", "remove", skill_name])
        .output();

    // Install
    let install_output = Command::new(octos_binary())
        .args(["skills", "install", repo])
        .output()
        .expect("Failed to execute install");

    assert!(
        install_output.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&install_output.stderr)
    );
    let stdout = String::from_utf8_lossy(&install_output.stdout);
    assert!(stdout.contains("Installed"), "should confirm installation");

    // Verify it shows in list
    let list_output = Command::new(octos_binary())
        .args(["skills", "list"])
        .output()
        .expect("Failed to execute list");

    assert!(list_output.status.success());
    let list_stdout = String::from_utf8_lossy(&list_output.stdout);
    assert!(
        list_stdout.contains(skill_name),
        "installed skill should appear in list"
    );

    // Remove
    let remove_output = Command::new(octos_binary())
        .args(["skills", "remove", skill_name])
        .output()
        .expect("Failed to execute remove");

    assert!(remove_output.status.success());
    let remove_stdout = String::from_utf8_lossy(&remove_output.stdout);
    assert!(remove_stdout.contains("Removed"), "should confirm removal");

    // Verify it's gone from list
    let list_after = Command::new(octos_binary())
        .args(["skills", "list"])
        .output()
        .expect("Failed to execute list");

    let list_after_stdout = String::from_utf8_lossy(&list_after.stdout);
    assert!(
        !list_after_stdout.contains(&format!("  {skill_name} ")),
        "removed skill should not appear in list"
    );
}
