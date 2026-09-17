//! Workspace skills loader.
//!
//! Loads skills from `.octos/skills/{name}/SKILL.md` with simple frontmatter.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};

/// Crate-agnostic skill selection filter.
///
/// This is the lowered form of the CLI's per-profile skill-selection layer
/// (`octos_cli::profiles::ProfileSkillsConfig`). It is intentionally free of
/// any CLI dependency so both the [`SkillsLoader`] (prompt / content injection)
/// and the plugin loader (tool specs) can consult the same selection decision.
///
/// A skill's `id` is its package identifier: the `manifest.json` `name`/`id`,
/// which equals the `SKILL.md` `name` and the skill directory name.
#[derive(Debug, Clone)]
pub enum SkillFilter {
    /// Load every discovered skill except these ids (`AllDiscovered` mode).
    AllExcept(HashSet<String>),
    /// Load only these ids (`AllowList` mode).
    Only(HashSet<String>),
}

impl SkillFilter {
    /// Whether a skill with `id` is permitted to load.
    pub fn allows(&self, id: &str) -> bool {
        match self {
            SkillFilter::AllExcept(disabled) => !disabled.contains(id),
            SkillFilter::Only(enabled) => enabled.contains(id),
        }
    }
}

/// Information about a loaded skill.
#[derive(Debug, Clone)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub version: Option<String>,
    pub author: Option<String>,
    pub path: PathBuf,
    pub available: bool,
    pub always: bool,
    /// True for system skills compiled into the binary.
    pub builtin: bool,
    /// True if this skill package includes a manifest.json (provides tools).
    pub has_tools: bool,
}

/// Loads workspace skills from `.octos/skills/`.
///
/// Supports multiple skills directories (e.g. per-profile + global).
/// Earlier directories take priority over later ones for skills with the same name.
pub struct SkillsLoader {
    skills_dirs: Vec<PathBuf>,
    /// Optional skill-selection filter (skill layering v1). `None` ⇒ no
    /// filtering: every discovered skill is listed / loadable exactly as
    /// before this feature existed (backwards-compatible).
    filter: Option<SkillFilter>,
}

impl SkillsLoader {
    /// Create a new loader for the given data directory.
    pub fn new(data_dir: impl AsRef<Path>) -> Self {
        Self {
            skills_dirs: vec![data_dir.as_ref().join("skills")],
            filter: None,
        }
    }

    /// Attach a skill-selection filter (builder form). Disabled skills are
    /// absent from `list_skills` (hence `build_skills_summary` /
    /// `get_always_skills`) and cannot be loaded by name.
    pub fn with_skill_filter(mut self, filter: Option<SkillFilter>) -> Self {
        self.filter = filter;
        self
    }

    /// Attach a skill-selection filter in place.
    pub fn set_skill_filter(&mut self, filter: Option<SkillFilter>) {
        self.filter = filter;
    }

    /// True when `name` is permitted by the active filter (or no filter).
    fn skill_allowed(&self, name: &str) -> bool {
        self.filter
            .as_ref()
            .is_none_or(|filter| filter.allows(name))
    }

    /// Add an additional skills directory (appends `/skills` to the given dir).
    /// Skills from earlier-added directories take priority over later ones.
    pub fn add_skills_dir(&mut self, dir: impl AsRef<Path>) {
        let path = dir.as_ref().join("skills");
        // Avoid duplicates
        if !self.skills_dirs.contains(&path) {
            self.skills_dirs.push(path);
        }
    }

    /// Add a raw skills directory path (no `/skills` suffix appended).
    /// Used for layered dirs like `platform-skills/` and `bundled-app-skills/`.
    pub fn add_skills_path(&mut self, path: impl AsRef<Path>) {
        let path = path.as_ref().to_path_buf();
        if !self.skills_dirs.contains(&path) {
            self.skills_dirs.push(path);
        }
    }

    /// List all installed workspace skills.
    ///
    /// Priority (highest first): first skills_dir, second skills_dir, ....
    pub async fn list_skills(&self) -> Result<Vec<SkillInfo>> {
        let mut skills = Vec::new();

        // Load workspace skills from all directories (later dirs first so earlier
        // dirs can override them, since we use retain to remove duplicates).
        for skills_dir in self.skills_dirs.iter().rev() {
            let entries = match tokio::fs::read_dir(skills_dir).await {
                Ok(entries) => Some(entries),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => {
                    return Err(e).wrap_err_with(|| {
                        format!("failed to read skills directory: {}", skills_dir.display())
                    });
                }
            };

            if let Some(mut entries) = entries {
                while let Some(entry) = entries.next_entry().await? {
                    let path = entry.path();
                    if !path.is_dir() {
                        continue;
                    }

                    let skill_file = path.join("SKILL.md");
                    if let Ok(content) = tokio::fs::read_to_string(&skill_file).await {
                        if let Some(info) = parse_skill(&skill_file, &content, false) {
                            // Override any existing skill with the same name
                            skills.retain(|s: &SkillInfo| s.name != info.name);
                            skills.push(info);
                        }
                    }
                }
            }
        }

        // Skill layering v1: drop skills disabled by the active selection
        // filter so they are absent from the prompt summary and never treated
        // as always-on. A `None` filter retains everything (backwards-compatible).
        if self.filter.is_some() {
            skills.retain(|s: &SkillInfo| self.skill_allowed(&s.name));
        }

        skills.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(skills)
    }

    /// Load a specific skill's full content (without frontmatter).
    ///
    /// Checks skills directories in priority order (first added = highest priority).
    pub async fn load_skill(&self, name: &str) -> Result<Option<String>> {
        // Skill layering v1: a disabled skill is not loadable by name — its
        // content must never be injected into the prompt.
        if !self.skill_allowed(name) {
            return Ok(None);
        }
        // Check workspace directories in priority order
        for skills_dir in &self.skills_dirs {
            let skill_file = skills_dir.join(name).join("SKILL.md");
            match tokio::fs::read_to_string(&skill_file).await {
                Ok(content) => return Ok(Some(strip_frontmatter(&content))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).wrap_err_with(|| format!("failed to read skill: {name}")),
            }
        }

        Ok(None)
    }

    /// Build an XML summary of all skills for the system prompt.
    pub async fn build_skills_summary(&self) -> Result<String> {
        let skills = self.list_skills().await?;
        if skills.is_empty() {
            return Ok(String::new());
        }

        let mut xml = String::from("<skills>\n");
        for s in &skills {
            let tools_attr = if s.has_tools { " tools=\"true\"" } else { "" };
            xml.push_str(&format!(
                "  <skill available=\"{}\"{}>\n    <name>{}</name>\n    <description>{}</description>\n    <location>{}</location>\n  </skill>\n",
                s.available, tools_attr, s.name, s.description, s.path.display()
            ));
        }
        xml.push_str("</skills>");
        Ok(xml)
    }

    /// Get names of always-on skills that meet their requirements.
    pub async fn get_always_skills(&self) -> Result<Vec<String>> {
        let skills = self.list_skills().await?;
        Ok(skills
            .into_iter()
            .filter(|s| s.always && s.available)
            .map(|s| s.name)
            .collect())
    }

    /// Load full content (minus frontmatter) for the given skill names, joined by `---`.
    pub async fn load_skills_for_context(&self, names: &[String]) -> Result<String> {
        let mut sections = Vec::new();
        for name in names {
            if let Some(content) = self.load_skill(name).await? {
                sections.push(content);
            }
        }
        Ok(sections.join("\n---\n"))
    }
}

/// Parse skill frontmatter and check requirements.
fn parse_skill(path: &Path, content: &str, builtin: bool) -> Option<SkillInfo> {
    let (fm, _) = split_frontmatter(content);
    let fm = fm?;

    let name = fm_value(&fm, "name").unwrap_or_else(|| {
        path.parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string()
    });

    let description = fm_value(&fm, "description").unwrap_or_default();
    let always = fm_value(&fm, "always")
        .map(|v| v == "true")
        .unwrap_or(false);

    let bins_ok = fm_value(&fm, "requires_bins")
        .map(|v| {
            v.split(',')
                .map(|b| b.trim())
                .filter(|b| !b.is_empty())
                .all(which_exists)
        })
        .unwrap_or(true);

    let env_ok = fm_value(&fm, "requires_env")
        .map(|v| {
            v.split(',')
                .map(|e| e.trim())
                .filter(|e| !e.is_empty())
                .all(|var| std::env::var(var).is_ok())
        })
        .unwrap_or(true);

    let version = fm_value(&fm, "version");
    let author = fm_value(&fm, "author");
    let has_tools = !builtin
        && path
            .parent()
            .map(|p| p.join("manifest.json").exists())
            .unwrap_or(false);

    Some(SkillInfo {
        name,
        description,
        version,
        author,
        path: path.to_path_buf(),
        available: bins_ok && env_ok,
        always,
        builtin,
        has_tools,
    })
}

/// Split content into (Option<frontmatter_lines>, body).
fn split_frontmatter(content: &str) -> (Option<Vec<String>>, &str) {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return (None, content);
    }

    // Find second ---
    let after_first = &trimmed[3..].trim_start_matches(['\r', '\n']);
    if let Some(end) = after_first.find("\n---") {
        let fm_text = &after_first[..end];
        let lines: Vec<String> = fm_text.lines().map(|l| l.to_string()).collect();
        let body_start = end + 4; // skip \n---
        let body = after_first[body_start..].trim_start_matches(['\r', '\n']);
        (Some(lines), body)
    } else {
        (None, content)
    }
}

/// Extract a key from frontmatter lines (simple `key: value` format).
/// Returns `None` for missing keys and YAML empty values (`[]`, `""`, `~`).
fn fm_value(lines: &[String], key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    lines.iter().find_map(|line| {
        let trimmed = line.trim();
        if trimmed.starts_with(&prefix) {
            let mut val = trimmed[prefix.len()..].trim();
            // Strip YAML inline comments (e.g. `[] # comment`)
            if let Some(hash_pos) = val.find('#') {
                val = val[..hash_pos].trim();
            }
            // Treat YAML empty markers as absent
            if val.is_empty() || val == "[]" || val == "\"\"" || val == "~" {
                None
            } else {
                Some(val.to_string())
            }
        } else {
            None
        }
    })
}

/// Strip frontmatter from content, returning only the body.
fn strip_frontmatter(content: &str) -> String {
    let (_, body) = split_frontmatter(content);
    body.to_string()
}

/// Check if a binary exists on PATH.
fn which_exists(bin: &str) -> bool {
    #[cfg(windows)]
    let prog = "where";
    #[cfg(not(windows))]
    let prog = "which";

    std::process::Command::new(prog)
        .arg(bin)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn setup_skills_dir(dir: &TempDir) -> PathBuf {
        let skills = dir.path().join("skills");
        tokio::fs::create_dir_all(&skills).await.unwrap();
        skills
    }

    #[tokio::test]
    async fn test_empty_dir_lists_no_skills() {
        let dir = tempfile::tempdir().unwrap();
        let loader = SkillsLoader::new(dir.path());
        let skills = loader.list_skills().await.unwrap();
        // Built-in system skills are retired; an empty workspace dir lists nothing.
        assert!(skills.is_empty());
    }

    #[tokio::test]
    async fn test_list_and_load_skill() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = setup_skills_dir(&dir).await;

        let skill_dir = skills_dir.join("greet");
        tokio::fs::create_dir_all(&skill_dir).await.unwrap();
        tokio::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: greet\ndescription: Say hello\nalways: false\n---\nYou are a greeter.\n",
        )
        .await
        .unwrap();

        let loader = SkillsLoader::new(dir.path());
        let skills = loader.list_skills().await.unwrap();
        let greet = skills.iter().find(|s| s.name == "greet").unwrap();
        assert_eq!(greet.description, "Say hello");
        assert!(!greet.always);
        assert!(greet.available);
        assert!(!greet.builtin);

        let content = loader.load_skill("greet").await.unwrap().unwrap();
        assert_eq!(content, "You are a greeter.\n");
    }

    #[tokio::test]
    async fn test_always_filtering() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = setup_skills_dir(&dir).await;

        for (name, always) in &[("a", "true"), ("b", "false"), ("c", "true")] {
            let sd = skills_dir.join(name);
            tokio::fs::create_dir_all(&sd).await.unwrap();
            tokio::fs::write(
                sd.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: d\nalways: {always}\n---\nbody\n"),
            )
            .await
            .unwrap();
        }

        let loader = SkillsLoader::new(dir.path());
        let always = loader.get_always_skills().await.unwrap();
        assert!(always.contains(&"a".to_string()));
        assert!(always.contains(&"c".to_string()));
        assert!(!always.contains(&"b".to_string()));
    }

    #[test]
    fn test_frontmatter_parsing() {
        let content = "---\nname: foo\ndescription: bar\nalways: true\n---\nBody here.\n";
        let (fm, body) = split_frontmatter(content);
        let fm = fm.unwrap();
        assert_eq!(fm_value(&fm, "name").unwrap(), "foo");
        assert_eq!(fm_value(&fm, "description").unwrap(), "bar");
        assert_eq!(fm_value(&fm, "always").unwrap(), "true");
        assert_eq!(body, "Body here.\n");
    }

    #[test]
    fn test_split_frontmatter_unclosed() {
        // Only one --- means no valid frontmatter
        let content = "---\nname: foo\nno closing fence";
        let (fm, body) = split_frontmatter(content);
        assert!(fm.is_none());
        assert_eq!(body, content);
    }

    #[test]
    fn test_fm_value_colon_in_value() {
        let lines = vec!["description: key: value pair".to_string()];
        assert_eq!(fm_value(&lines, "description").unwrap(), "key: value pair");
    }

    #[test]
    fn test_strip_frontmatter_with_fm() {
        let content = "---\nname: foo\n---\nBody text\n";
        assert_eq!(strip_frontmatter(content), "Body text\n");
    }

    #[test]
    fn test_parse_skill_minimal() {
        let content = "---\ndescription: A skill\n---\nBody\n";
        let path = PathBuf::from("/fake/my-skill/SKILL.md");
        let info = parse_skill(&path, content, false).unwrap();
        // Name falls back to parent directory name
        assert_eq!(info.name, "my-skill");
        assert_eq!(info.description, "A skill");
        assert!(!info.always);
        assert!(info.available);
        assert!(!info.builtin);
    }

    #[test]
    fn test_parse_skill_no_frontmatter_returns_none() {
        let content = "Just text, no frontmatter";
        let path = PathBuf::from("/fake/skill/SKILL.md");
        assert!(parse_skill(&path, content, false).is_none());
    }

    #[test]
    fn test_parse_skill_requires_env_missing() {
        let content = "---\nname: envskill\ndescription: d\nrequires_env: OCTOS_NONEXISTENT_VAR_XYZ_99\n---\nB\n";
        let path = PathBuf::from("/fake/envskill/SKILL.md");
        let info = parse_skill(&path, content, false).unwrap();
        assert!(!info.available);
    }

    #[test]
    fn test_parse_skill_name_fallback_from_path() {
        // No name in frontmatter, should use parent dir name
        let content = "---\ndescription: fallback test\n---\nBody\n";
        let path = PathBuf::from("/data/skills/my-cool-skill/SKILL.md");
        let info = parse_skill(&path, content, false).unwrap();
        assert_eq!(info.name, "my-cool-skill");
    }

    // --- skill layering v1: SkillFilter ---

    #[test]
    fn skill_filter_all_except_and_only_semantics() {
        let deny = SkillFilter::AllExcept(HashSet::from(["weather".to_string()]));
        assert!(deny.allows("news"));
        assert!(!deny.allows("weather"));

        let allow = SkillFilter::Only(HashSet::from(["news".to_string()]));
        assert!(allow.allows("news"));
        assert!(!allow.allows("weather"));
    }

    #[tokio::test]
    async fn test_multi_dir_priority() {
        // Set up two directories: "global" and "profile"
        let global_dir = tempfile::tempdir().unwrap();
        let profile_dir = tempfile::tempdir().unwrap();
        let global_skills = setup_skills_dir(&global_dir).await;
        let profile_skills = setup_skills_dir(&profile_dir).await;

        // Global has skill "shared" with description "global version"
        let sd = global_skills.join("shared");
        tokio::fs::create_dir_all(&sd).await.unwrap();
        tokio::fs::write(
            sd.join("SKILL.md"),
            "---\nname: shared\ndescription: global version\n---\nGlobal body\n",
        )
        .await
        .unwrap();

        // Global has skill "global-only"
        let sd = global_skills.join("global-only");
        tokio::fs::create_dir_all(&sd).await.unwrap();
        tokio::fs::write(
            sd.join("SKILL.md"),
            "---\nname: global-only\ndescription: only in global\n---\nBody\n",
        )
        .await
        .unwrap();

        // Profile has skill "shared" with description "profile version" (overrides global)
        let sd = profile_skills.join("shared");
        tokio::fs::create_dir_all(&sd).await.unwrap();
        tokio::fs::write(
            sd.join("SKILL.md"),
            "---\nname: shared\ndescription: profile version\n---\nProfile body\n",
        )
        .await
        .unwrap();

        // Profile has skill "profile-only"
        let sd = profile_skills.join("profile-only");
        tokio::fs::create_dir_all(&sd).await.unwrap();
        tokio::fs::write(
            sd.join("SKILL.md"),
            "---\nname: profile-only\ndescription: only in profile\n---\nBody\n",
        )
        .await
        .unwrap();

        // Profile dir first (higher priority), then global
        let mut loader = SkillsLoader::new(profile_dir.path());
        loader.add_skills_dir(global_dir.path());
        let skills = loader.list_skills().await.unwrap();

        // "shared" should use profile version
        let shared = skills.iter().find(|s| s.name == "shared").unwrap();
        assert_eq!(shared.description, "profile version");

        // Both unique skills should be present
        assert!(skills.iter().any(|s| s.name == "global-only"));
        assert!(skills.iter().any(|s| s.name == "profile-only"));

        // load_skill should return profile version
        let content = loader.load_skill("shared").await.unwrap().unwrap();
        assert_eq!(content, "Profile body\n");

        // load_skill should find global-only skill
        let content = loader.load_skill("global-only").await.unwrap().unwrap();
        assert_eq!(content, "Body\n");
    }
}
