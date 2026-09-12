//! Embedded metadata for app-skill binaries that ship alongside the `octos` binary.
//!
//! Each entry contains: (dir_name, binary_name, SKILL.md content, manifest.json content).
//! The actual binaries are sibling executables in the same directory as the `octos` binary;
//! [`super::bootstrap`] copies them into `.octos/skills/` at gateway startup.
//!
//! This ARC downstream build ships no bundled skills: the upstream
//! app-skills/platform-skills crates are not part of the workspace, so both
//! tables are empty and skill bootstrap is a no-op.

/// (dir_name, binary_name, skill_md, manifest_json)
pub const BUNDLED_APP_SKILLS: &[(&str, &str, &str, &str)] = &[];

/// Platform skills: bootstrapped once by `octos serve` (admin bot) at startup,
/// shared across all gateway profiles. Only installed when their backend is reachable.
/// Same tuple format as BUNDLED_APP_SKILLS: (dir_name, binary_name, skill_md, manifest_json).
pub const PLATFORM_SKILLS: &[(&str, &str, &str, &str)] = &[];
