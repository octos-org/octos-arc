//! Prompt templates (`arc/prompts/*.md`).
//!
//! Every template the Python adapter kept as a constant lives in one file,
//! byte for byte, with Python `str.format` placeholders (`{name}`, `{{`
//! escapes). The binary carries the same files as built-in defaults; a
//! policy's `prompts.dir` overrides them one file at a time, so changing a
//! prompt never needs a rebuild.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use eyre::{Result, bail};

macro_rules! builtin {
    ($($name:literal),* $(,)?) => {
        &[$(($name, include_str!(concat!("../../../arc/prompts/", $name, ".md")))),*]
    };
}

/// (name, text) for every shipped template.
pub const BUILTIN: &[(&str, &str)] = builtin!(
    "acceptance-tests",
    "ancestors-note",
    "architecture-contract",
    "budget-notice",
    "codegen-files-intro",
    "codegen-files-intro-evolution",
    "codegen-format",
    "codegen-ports-clause",
    "codegen-prompt",
    "codegen-repair-suffix",
    "codegen-rewrite",
    "codegen-size-full",
    "codegen-size-small",
    "codegen-system",
    "corrections",
    "corrections-header",
    "current-files-note",
    "design",
    "design-contract-note",
    "evolution-note",
    "final-check",
    "inline-design-note",
    "inline-sources-header",
    "inline-spec-header",
    "node",
    "node-preamble-create",
    "node-preamble-extend",
    "nudge",
    "performance-contract",
    "port-contract",
    "port-rules",
    "rehearsal-repair",
    "repair",
    "rewrite",
    "skeleton",
    "skeleton-tests",
    "slow-tests",
    "relevant-sources-header",
    "relevant-sources-omitted",
    "startup-failure",
    "startup-failure-grader",
    "tiny-prompt",
    "tiny-prompt-evolution",
    "tiny-server",
    "tiny-system",
    "truncated-retry",
    "ui-contract-core",
    "ui-contract-data",
    "ui-contract-session",
    "verify-full",
    "verify-minimal",
);

#[derive(Debug, Clone)]
pub struct Prompts {
    texts: HashMap<String, String>,
    overrides: Vec<PathBuf>,
}

impl Prompts {
    /// Built-in templates, then `<dir>/<name>.md` overrides when present.
    pub fn load(dir: Option<&Path>) -> Result<Self> {
        let mut texts: HashMap<String, String> = BUILTIN
            .iter()
            .map(|(name, text)| (name.to_string(), text.to_string()))
            .collect();
        let mut overrides = Vec::new();
        if let Some(dir) = dir {
            for name in BUILTIN.iter().map(|(name, _)| *name) {
                let path = dir.join(format!("{name}.md"));
                if path.is_file() {
                    texts.insert(name.to_string(), std::fs::read_to_string(&path)?);
                    overrides.push(path);
                }
            }
        }
        Ok(Self { texts, overrides })
    }

    pub fn builtin() -> Self {
        Self::load(None).expect("built-in prompts")
    }

    /// Files that replaced a built-in template (for the run log).
    pub fn overrides(&self) -> &[PathBuf] {
        &self.overrides
    }

    /// Raw template text (values that are inserted verbatim, never formatted).
    pub fn get(&self, name: &str) -> &str {
        self.texts
            .get(name)
            .map(String::as_str)
            .unwrap_or_else(|| panic!("unknown prompt {name}"))
    }

    /// Format a template with `{name}` placeholders.
    pub fn render(&self, name: &str, vars: &[(&str, &str)]) -> Result<String> {
        render_template(self.get(name), vars)
    }

    /// One correction sentence from `corrections.md` (`## key` sections).
    pub fn correction(&self, key: &str, vars: &[(&str, &str)]) -> Result<String> {
        let text = self.get("corrections");
        let mut current: Option<&str> = None;
        let mut body: Vec<&str> = Vec::new();
        let mut found: Option<String> = None;
        for line in text.lines() {
            if let Some(heading) = line.strip_prefix("## ") {
                if current == Some(key) {
                    found = Some(body.join("\n"));
                }
                current = Some(heading.trim());
                body.clear();
            } else if current.is_some() {
                body.push(line);
            }
        }
        if current == Some(key) && found.is_none() {
            found = Some(body.join("\n"));
        }
        match found {
            Some(section) => render_template(section.trim(), vars),
            None => bail!("no correction named {key} in corrections.md"),
        }
    }
}

/// Python `str.format` subset: `{name}` → value, `{{` → `{`, `}}` → `}`.
/// Values are inserted verbatim (they are never re-scanned), which matches
/// the adapter: contract blocks with literal braces were always passed as
/// arguments, never as templates.
pub fn render_template(template: &str, vars: &[(&str, &str)]) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' => {
                if chars.peek() == Some(&'{') {
                    chars.next();
                    out.push('{');
                    continue;
                }
                let mut key = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(k) => key.push(k),
                        None => bail!("unterminated placeholder {{{key}"),
                    }
                }
                match vars.iter().find(|(name, _)| *name == key) {
                    Some((_, value)) => out.push_str(value),
                    None => bail!("missing template value for {{{key}}}"),
                }
            }
            '}' => {
                if chars.peek() == Some(&'}') {
                    chars.next();
                }
                out.push('}');
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_render_placeholders_and_escapes_like_python_format() {
        let out = render_template("a {x} {{lit}} {y}", &[("x", "1"), ("y", "{z}")]).unwrap();
        assert_eq!(out, "a 1 {lit} {z}");
        assert!(render_template("{missing}", &[]).is_err());
    }

    #[test]
    fn should_expose_every_builtin_template() {
        let prompts = Prompts::builtin();
        assert!(
            prompts
                .get("codegen-system")
                .starts_with("You write complete, minimal web apps.")
        );
        assert!(prompts.get("codegen-prompt").contains("{size_rule}"));
        let design = prompts
            .render(
                "design",
                &[
                    ("node_id", "REQ-1"),
                    ("node_spec", "s"),
                    ("ancestors", ""),
                    ("tests", ""),
                ],
            )
            .unwrap();
        assert!(design.contains("{\"routes\": [{\"method\": \"POST\""));
    }

    #[test]
    fn should_prefer_override_files_over_builtin_text() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("codegen-system.md"), "custom system").unwrap();
        let prompts = Prompts::load(Some(dir.path())).unwrap();
        assert_eq!(prompts.get("codegen-system"), "custom system");
        assert_eq!(prompts.overrides().len(), 1);
        assert!(
            prompts
                .get("codegen-format")
                .contains("<<<FILE relative/path>>>")
        );
    }

    #[test]
    fn should_look_up_corrections_by_section() {
        let prompts = Prompts::builtin();
        let text = prompts
            .correction("regressions_restored", &[("best", "3"), ("total", "6")])
            .unwrap();
        assert!(text.starts_with("Your last two repairs made the tests worse"));
        assert!(text.contains("(3/6)"));
        assert!(prompts.correction("nope", &[]).is_err());
    }

    #[test]
    fn should_carry_the_codegen_format_reminder_used_by_the_no_blocks_retry() {
        let prompts = Prompts::builtin();
        let text = prompts.correction("codegen_no_blocks", &[]).unwrap();
        assert!(text.contains("<<<FILE path>>>"));
        assert!(text.contains("<<<END FILE>>>"));
    }
}
