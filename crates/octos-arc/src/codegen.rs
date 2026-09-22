//! Single-request code generation (`arc/codegen.py` plus the codegen parts of
//! `arc/main.py`): the model answers ONE request with the whole application
//! as delimited file blocks; the harness writes them and the normal
//! acceptance loop runs.
//!
//! ```text
//! <<<FILE backend/server.js>>>
//! ...file contents...
//! <<<END FILE>>>
//! ```

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use eyre::Result;
use regex::Regex;

use crate::prompts::Prompts;

static FILE_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)<<<FILE\s+([^\n>]+?)\s*>>>\r?\n(.*?)(?:\r?\n)?<<<END FILE>>>").unwrap()
});
static META_CHARSET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<meta[^>]+charset").unwrap());
static HEAD_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<head[^>]*>").unwrap());
static HTML_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<html[^>]*>").unwrap());
pub const CHARSET_META: &str = "<meta charset=\"utf-8\">";
pub const NAV_PLACEHOLDER: &str = "<!--NAV-->";

/// Extract path → contents; a later block for the same path wins. Paths are
/// normalised and confined to the project (no absolute, no `..`). A stray
/// ``` fence the model wrapped around the body is tolerated.
pub fn parse_file_blocks(text: &str) -> BTreeMap<String, String> {
    let mut files = BTreeMap::new();
    for captures in FILE_BLOCK.captures_iter(text) {
        let raw = captures[1]
            .trim()
            .trim_matches(|c| c == '`' || c == '\'' || c == '"');
        let normalized = raw.replace('\\', "/");
        let parts: Vec<&str> = normalized
            .split('/')
            .filter(|p| !p.is_empty() && *p != ".")
            .collect();
        if parts.is_empty() || parts.contains(&"..") || raw.starts_with('/') {
            continue;
        }
        let mut body = captures[2].to_string();
        let stripped = body.trim_matches('\n');
        if stripped.starts_with("```") && stripped.trim_end().ends_with("```") {
            let inner = stripped
                .split_once('\n')
                .map(|(_, rest)| rest)
                .unwrap_or("");
            body = inner
                .rsplit_once("```")
                .map(|(head, _)| head)
                .unwrap_or("")
                .to_string();
        }
        files.insert(
            parts.join("/"),
            format!("{}\n", body.trim_end_matches('\n')),
        );
    }
    files
}

/// Pages without a charset declaration were decoded as Latin-1 by Chromium
/// (the servers send `text/html` without charset), so every non-ASCII string
/// the specs look for turned into mojibake. Inject the meta tag.
pub fn ensure_charset(text: &str) -> String {
    if META_CHARSET.is_match(text) {
        return text.to_string();
    }
    if let Some(m) = HEAD_TAG.find(text) {
        return format!("{}{}{}", &text[..m.end()], CHARSET_META, &text[m.end()..]);
    }
    if let Some(m) = HTML_TAG.find(text) {
        return format!(
            "{}<head>{}</head>{}",
            &text[..m.end()],
            CHARSET_META,
            &text[m.end()..]
        );
    }
    format!("{CHARSET_META}\n{text}")
}

/// A file block occasionally arrives with its newlines JSON-escaped (one long
/// line full of literal `\n`). Restore it when the block is clearly
/// flattened; leave normal files alone.
pub fn unescape_flattened(text: &str) -> String {
    let real = text.matches('\n').count();
    let literal = text.matches("\\n").count();
    if literal >= 10 && literal > 5 * real.max(1) {
        text.replace("\\r\\n", "\n")
            .replace("\\n", "\n")
            .replace("\\t", "\t")
    } else {
        text.to_string()
    }
}

/// `node --check`; `None` when node is unavailable.
pub fn js_parses(path: &Path) -> Option<bool> {
    let output = std::process::Command::new("node")
        .arg("--check")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    Some(output.status.success())
}

/// Partially flattened blocks (some lines carry literal `\n` between
/// statements) are only rewritten when the unescaped version parses and the
/// original does not.
pub fn repair_flattened_js(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    if !text.contains("\\n") || js_parses(path) != Some(false) {
        return false;
    }
    let fixed: Vec<String> = text
        .split('\n')
        .map(|line| {
            let starts_response =
                line.trim_start().starts_with("res.") || line.trim_start().starts_with("return");
            if line.matches("\\n").count() >= 2 && !starts_response {
                line.replace("\\n", "\n")
            } else {
                line.to_string()
            }
        })
        .collect();
    let fixed = fixed.join("\n");
    if fixed == text {
        return false;
    }
    let Ok(backup) = std::fs::read(path) else {
        return false;
    };
    if std::fs::write(path, &fixed).is_err() {
        return false;
    }
    if js_parses(path) == Some(true) {
        return true;
    }
    let _ = std::fs::write(path, backup);
    false
}

/// Compatibility hook: matching hrefs does not prove redundant navigation.
/// Preserve application semantics; actual acceptance failures drive repairs.
pub fn dedupe_nav_links(_root: &Path) -> Vec<String> {
    vec![]
}

/// Write the parsed blocks under `root`, applying the deterministic repairs.
pub fn write_files(root: &Path, files: &BTreeMap<String, String>) -> Result<Vec<String>> {
    let mut written = Vec::new();
    for (rel, body) in files {
        let dest = root.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut text = body.clone();
        let ext = dest
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        if ext == "html" || ext == "htm" {
            text = ensure_charset(&text);
        }
        std::fs::write(&dest, text)?;
        if matches!(ext.as_str(), "js" | "cjs" | "mjs") {
            repair_flattened_js(&dest);
        }
        written.push(rel.clone());
    }
    Ok(written)
}

/// Codegen turns never emit package.json: the harness writes the two fixed
/// manifests. The build preserves the source tree without inventing route aliases;
/// application routing resolves clean URLs. The backend pins
/// the CommonJS loader so Node's module detection never loads a `require`
/// server as ESM.
pub const FRONTEND_MANIFEST: &str = "{\n  \"name\": \"f\",\n  \"private\": true,\n  \"scripts\": {\n    \"build\": \"node -e \\\"const f=require('fs');f.rmSync('dist',{recursive:true,force:true});f.cpSync('src','dist',{recursive:true})\\\"\"\n  }\n}\n";
pub const BACKEND_MANIFEST: &str = "{\n  \"name\": \"b\",\n  \"private\": true,\n  \"type\": \"commonjs\",\n  \"scripts\": {\n    \"start\": \"node server.js\"\n  }\n}\n";

/// Write the manifests that are missing; returns the ones written.
pub fn write_manifests(root: &Path) -> Result<Vec<String>> {
    let mut written = Vec::new();
    for (rel, body) in [
        ("frontend/package.json", FRONTEND_MANIFEST),
        ("backend/package.json", BACKEND_MANIFEST),
    ] {
        let path = root.join(rel);
        if path.exists() {
            continue;
        }
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, body)?;
        written.push(rel.to_string());
    }
    Ok(written)
}

const SOURCE_EXTS_ALL: &[&str] = &[".js", ".mjs", ".cjs", ".html", ".css", ".json"];

fn source_paths(root: &Path, exts: &[&str]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    fn walk(dir: &Path, exts: &[&str], files: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        entries.sort();
        for path in entries {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if matches!(name.as_str(), "node_modules" | "dist" | ".git") {
                continue;
            }
            if path.is_dir() {
                walk(&path, exts, files);
            } else if path.is_file() && exts.iter().any(|e| name.ends_with(e)) {
                files.push(path);
            }
        }
    }
    for part in ["frontend", "backend"] {
        let base = root.join(part);
        if base.is_dir() {
            walk(&base, exts, &mut files);
        }
    }
    files
}

/// Quote the app's source files so a repair turn edits immediately instead
/// of spending its request budget on reads. Bounded; files that would not
/// fit are listed with their size (smallest files first).
pub fn inline_sources(
    prompts: &Prompts,
    root: &Path,
    max_chars: usize,
    codegen_only: bool,
) -> String {
    let exts = if codegen_only {
        &[".html", ".js"][..]
    } else {
        SOURCE_EXTS_ALL
    };
    let mut files: Vec<(u64, PathBuf)> = source_paths(root, exts)
        .into_iter()
        .map(|p| (std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0), p))
        .collect();
    files.sort();
    let mut parts = String::new();
    let mut total = 0usize;
    for (_, path) in files {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let chars = text.chars().count();
        if total + chars > max_chars {
            parts.push_str(&format!(
                "--- {rel} --- (omitted, {chars} chars; read it if you must change it)\n"
            ));
            continue;
        }
        total += chars;
        parts.push_str(&format!("--- {rel} ---\n{}\n", text.trim_end()));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("{}{}", prompts.get("inline-sources-header"), parts)
    }
}

/// `main.spec_terms`: identifiers, paths and quoted strings a spec mentions (≥ 3 chars), lower-cased.
pub fn spec_terms(spec_text: &str) -> BTreeSet<String> {
    static IDENT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[A-Za-z_][A-Za-z0-9_-]{2,}").unwrap());
    static PATH: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"['"`](/[^'"`\s]{1,60})['"`]"#).unwrap());
    static QUOTED: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"['"`]([^'"`\n]{3,40})['"`]"#).unwrap());
    const STOP: &[&str] = &[
        "await",
        "page",
        "expect",
        "const",
        "test",
        "async",
        "import",
        "from",
        "playwright",
        "toBeVisible",
        "toHaveText",
        "getByRole",
        "getByTestId",
        "getByLabel",
        "getByText",
        "click",
        "fill",
        "goto",
        "name",
        "button",
        "link",
        "true",
        "false",
        "null",
        "let",
        "var",
        "return",
        "function",
    ];
    let mut terms: BTreeSet<String> = BTreeSet::new();
    for m in IDENT.find_iter(spec_text) {
        terms.insert(m.as_str().to_string());
    }
    for c in PATH.captures_iter(spec_text) {
        terms.insert(c[1].to_string());
    }
    for c in QUOTED.captures_iter(spec_text) {
        terms.insert(c[1].to_string());
    }
    terms
        .into_iter()
        .filter(|t| !STOP.contains(&t.as_str()))
        .map(|t| t.to_lowercase())
        .collect()
}

/// Complete-file generation must not proceed with omitted existing source bodies.
pub fn sources_fit(root: &Path, budget: usize) -> bool {
    let mut remaining = budget;
    for path in source_paths(root, SOURCE_EXTS_ALL) {
        let Ok(text) = std::fs::read(&path) else {
            return false;
        };
        let size = String::from_utf8_lossy(&text).chars().count();
        let Some(rest) = remaining.checked_sub(size) else {
            return false;
        };
        remaining = rest;
    }
    true
}

/// `main.relevant_sources` (round 35): quote the existing sources a node most
/// likely touches — every backend entry file first (the router every node
/// extends), then pages ranked by how many of the spec's terms (locators,
/// texts, routes) they contain, until the budget is spent; the rest are listed
/// by name so the model knows they exist. Stylesheets never decide a spec.
pub fn relevant_sources(
    prompts: &Prompts,
    root: &Path,
    spec_text: &str,
    max_chars: usize,
) -> String {
    let files = source_paths(root, &[".html", ".js", ".mjs", ".cjs", ".css", ".json"]);
    if files.is_empty() {
        return String::new();
    }
    let terms = spec_terms(spec_text);
    let mut scored: Vec<(u8, i64, usize, String, String)> = Vec::new();
    for path in files {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let low = text.to_lowercase();
        let hits = terms.iter().filter(|t| low.contains(t.as_str())).count() as i64;
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let is_backend = rel.starts_with("backend/");
        scored.push((
            if rel.ends_with(".json") {
                2
            } else {
                u8::from(!is_backend)
            },
            -hits,
            text.chars().count(),
            rel,
            text,
        ));
    }
    scored.sort_by(|a, b| (a.0, a.1, a.2, &a.3).cmp(&(b.0, b.1, b.2, &b.3)));
    let mut parts = String::new();
    let mut omitted: Vec<String> = Vec::new();
    let mut total = 0usize;
    for (_, neg_hits, size, rel, text) in scored {
        if total + size > max_chars {
            omitted.push(format!("{rel} ({size} chars, {} spec terms)", -neg_hits));
            continue;
        }
        total += size;
        parts.push_str(&format!("--- {rel} ---\n{}\n", text.trim_end()));
    }
    let mut out = format!("{}{}", prompts.get("relevant-sources-header"), parts);
    if !omitted.is_empty() {
        out.push_str(&format!(
            "{}{}\n",
            prompts.get("relevant-sources-omitted"),
            omitted.join("; ")
        ));
    }
    out
}

/// `main.looks_like_markup`: a bare page or page fragment (the tiny tier asks
/// for markup without doctype/head).
pub fn looks_like_markup(text: &str) -> bool {
    static TAG: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)<(html|body|main|div|section|form|button|script|span|p|h[1-6]|input|label|ul|table)\b")
            .unwrap()
    });
    TAG.is_match(text)
}

/// Short, stable listing of the app sources for evolution prompts.
pub fn source_listing(root: &Path, limit: usize) -> String {
    let mut lines = Vec::new();
    for part in ["frontend", "backend"] {
        let base = root.join(part);
        if !base.is_dir() {
            continue;
        }
        let mut all = Vec::new();
        fn walk(dir: &Path, all: &mut Vec<PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            let mut entries: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            entries.sort();
            for path in entries {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if matches!(name.as_str(), "node_modules" | "dist" | ".git") {
                    continue;
                }
                if path.is_dir() {
                    walk(&path, all);
                } else {
                    all.push(path);
                }
            }
        }
        walk(&base, &mut all);
        for path in all {
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            lines.push(format!("{rel} ({size} B)"));
            if lines.len() >= limit {
                lines.push("...".into());
                return lines.join("\n");
            }
        }
    }
    lines.join("\n")
}

/// Inputs of the compact codegen prompt (`main.CODEGEN_PROMPT`).
pub struct CodegenInputs<'a> {
    pub node_id: &'a str,
    pub description: &'a str,
    pub spec: &'a str,
    pub web_port: u16,
    pub extra_ports: &'a [u16],
    /// Total ATOMIC nodes in the tree.
    pub n_nodes: usize,
    /// Compact size rule (small spec: thinking off) instead of the multi-page mechanisms.
    pub small_rule: bool,
    /// Existing app (evolution or later nodes): quote the relevant sources and ask for
    /// complete changed files.
    pub existing_app: Option<&'a Path>,
    /// Codegen prompt budget in characters (`prompts.codegen_context_chars`).
    pub context_chars: usize,
}

pub fn ports_clause(prompts: &Prompts, extra_ports: &[u16]) -> Result<String> {
    if extra_ports.is_empty() {
        return Ok(String::new());
    }
    let ports = extra_ports
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    prompts.render("codegen-ports-clause", &[("ports", &ports)])
}

/// The compact implement prompt; the caller appends the format instructions.
pub fn implement_prompt(prompts: &Prompts, inputs: &CodegenInputs<'_>) -> Result<String> {
    let ports = ports_clause(prompts, inputs.extra_ports)?;
    let size_rule = if inputs.small_rule {
        prompts.get("codegen-size-small")
    } else {
        prompts.get("codegen-size-full")
    };
    let files_intro = if inputs.existing_app.is_some() {
        prompts.get("codegen-files-intro-evolution")
    } else {
        prompts.get("codegen-files-intro")
    };
    let port = inputs.web_port.to_string();
    let mut prompt = prompts.render(
        "codegen-prompt",
        &[
            ("node_id", inputs.node_id),
            ("description", inputs.description),
            ("spec", inputs.spec),
            ("files_intro", files_intro),
            ("port", &port),
            ("ports", &ports),
            ("size_rule", size_rule),
        ],
    )?;
    if let Some(root) = inputs.existing_app {
        let budget = inputs
            .context_chars
            .saturating_sub(inputs.spec.chars().count())
            .max(8000);
        prompt.push_str(&relevant_sources(prompts, root, inputs.spec, budget));
    }
    Ok(prompt)
}

/// User message for a codegen turn: prompt + format instructions. The
/// kernel session trims turn input, so the wire text is trimmed here too
/// (byte-identical requests keep the provider's prefix cache and token
/// counts identical to the Python path).
pub fn with_format(prompts: &Prompts, prompt: &str) -> String {
    format!("{prompt}\n{}", prompts.get("codegen-format"))
        .trim()
        .to_string()
}

/// `main.strip_code_fences`: the body of the first fenced block, else the trimmed text.
pub fn strip_code_fences(text: &str) -> String {
    static FENCE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?s)```[a-zA-Z]*\n(.*?)```").unwrap());
    let text = text.trim();
    match FENCE.captures(text) {
        Some(c) => c[1].trim().to_string(),
        None => text.to_string(),
    }
}

/// `main.compact_spec_lines`: the spec's statements without imports, blank
/// lines, `await` and closing braces — what a page must satisfy, in the spec's
/// own words.
pub fn compact_spec_lines(text: &str) -> String {
    static AWAIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^await\s+").unwrap());
    static TEST_HEAD: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"^test\((['"])(.*?)['"],\s*async\s*\(\{[^}]*\}\)\s*=>\s*\{$"#).unwrap()
    });
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty()
            || line.starts_with("import ")
            || line.starts_with("//")
            || line.starts_with("/*")
            || line.starts_with('*')
            || matches!(line, "});" | "})" | "}")
        {
            continue;
        }
        let line = AWAIT.replace(line, "");
        let line = TEST_HEAD.replace(&line, "test: $2");
        out.push(line.into_owned());
    }
    out.join("\n")
}

/// The fixed static server of the tiny tier (`main.TINY_SERVER_JS`): PORT plus the
/// spec default ports unless `ARC_EXTRA_PORTS=0`; no task logic.
pub fn tiny_server_js(prompts: &Prompts, web_port: u16, extra_ports: &[u16]) -> Result<String> {
    let port = web_port.to_string();
    let extra = serde_json::to_string(
        &extra_ports
            .iter()
            .filter(|p| **p != web_port)
            .collect::<Vec<_>>(),
    )?;
    prompts.render("tiny-server", &[("port", &port), ("extra_ports", &extra)])
}

/// Rewrite prompt when round 0 passed nothing (`Flow.node_cycle.rebuild_prompt`).
pub fn rewrite_prompt(
    prompts: &Prompts,
    codegen_prompt: &str,
    failures: &str,
    root: &Path,
    spec_text: &str,
    context_chars: usize,
) -> Result<String> {
    let budget = context_chars
        .saturating_sub(codegen_prompt.chars().count())
        .max(8000);
    let sources = relevant_sources(prompts, root, spec_text, budget);
    prompts.render(
        "codegen-rewrite",
        &[
            ("codegen_prompt", codegen_prompt),
            ("failures", failures),
            ("sources", &sources),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_preserve_escape_heavy_valid_source_and_data() {
        let dir = tempfile::tempdir().unwrap();
        let value = "line\n".repeat(20);
        let js = format!("const text = {};\n", serde_json::to_string(&value).unwrap());
        let data = serde_json::json!({"text": value}).to_string();
        let files = BTreeMap::from([
            ("backend/server.js".to_string(), js.clone()),
            ("backend/sample.json".to_string(), data.clone()),
        ]);
        write_files(dir.path(), &files).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("backend/server.js")).unwrap(),
            js
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("backend/sample.json")).unwrap(),
            data
        );
    }

    #[test]
    fn should_rank_sources_by_spec_terms_with_backend_first_and_list_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("backend")).unwrap();
        std::fs::create_dir_all(root.join("frontend/src")).unwrap();
        std::fs::write(
            root.join("backend/server.js"),
            "const http = require('http'); // router",
        )
        .unwrap();
        std::fs::write(
            root.join("frontend/src/index.html"),
            "<a href=\"/notes\">Notes</a>",
        )
        .unwrap();
        std::fs::write(
            root.join("frontend/src/login.html"),
            "<form id=\"login\"></form>",
        )
        .unwrap();
        std::fs::write(root.join("frontend/src/style.css"), "body{}").unwrap();
        let spec =
            "await page.goto('/notes'); await expect(page.getByText('Notes')).toBeVisible();";
        let terms = spec_terms(spec);
        assert!(terms.contains("/notes") && terms.contains("notes"));
        assert!(!terms.contains("await") && !terms.contains("page"));
        let text = relevant_sources(&Prompts::builtin(), root, spec, 1_000);
        let server = text.find("backend/server.js").unwrap();
        let index = text.find("frontend/src/index.html").unwrap();
        let login = text.find("frontend/src/login.html").unwrap();
        assert!(
            server < index && index < login,
            "backend first, then by spec-term hits"
        );
        assert!(
            text.contains("style.css"),
            "visibility failures can originate in CSS"
        );
        let tight = relevant_sources(&Prompts::builtin(), root, spec, 60);
        assert!(tight.contains("Other files, unchanged unless the requirement needs them: "));
        assert!(tight.contains("login.html (") && tight.contains("spec terms)"));
    }

    #[test]
    fn json_context_does_not_displace_code_or_overflow_first_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("backend/data")).unwrap();
        std::fs::create_dir_all(root.join("frontend")).unwrap();
        std::fs::write(root.join("backend/server.js"), "server").unwrap();
        std::fs::write(root.join("frontend/index.html"), "page").unwrap();
        std::fs::write(root.join("backend/data/state.json"), r#"{"count":17}"#).unwrap();
        let full = relevant_sources(&Prompts::builtin(), root, "count", 100);
        assert!(full.contains("--- backend/data/state.json ---"));
        assert!(full.contains(r#""count":17"#));
        let tight = relevant_sources(&Prompts::builtin(), root, "count", 10);
        assert!(tight.contains("--- backend/server.js ---"));
        assert!(tight.contains("--- frontend/index.html ---"));
        assert!(!tight.contains("--- backend/data/state.json ---"));
        std::fs::write(root.join("backend/server.js"), "x".repeat(101)).unwrap();
        let tight = relevant_sources(&Prompts::builtin(), root, "count", 100);
        assert!(!tight.contains("--- backend/server.js ---"));
        assert!(tight.contains("--- backend/data/state.json ---"));
    }

    #[test]
    fn should_accept_page_fragments_as_markup() {
        assert!(looks_like_markup("<main><button>+</button></main>"));
        assert!(looks_like_markup("<!DOCTYPE html><html></html>"));
        assert!(!looks_like_markup("Sure, here is the page."));
    }

    #[test]
    fn should_strip_code_fences_and_compact_spec_lines_like_the_python_helpers() {
        assert_eq!(
            strip_code_fences("```html\n<html></html>\n```"),
            "<html></html>"
        );
        assert_eq!(strip_code_fences("  <html></html>  "), "<html></html>");
        let spec = "import { test, expect } from '@playwright/test';\n\ntest('counts', async ({ page }) => {\n  await page.goto('/');\n  // note\n  await expect(page.getByTestId('count')).toHaveText('0');\n});\n";
        assert_eq!(
            compact_spec_lines(spec),
            "test: counts\npage.goto('/');\nexpect(page.getByTestId('count')).toHaveText('0');"
        );
    }

    #[test]
    fn should_render_the_tiny_server_with_the_spec_default_ports() {
        let prompts = Prompts::builtin();
        let js = tiny_server_js(&prompts, 3000, &[3000, 3301]).unwrap();
        assert!(js.contains("process.env.PORT || 3000"));
        assert!(js.contains("for (const p of [3301])"));
        assert!(
            js.contains("{ try {"),
            "double braces must render as single braces"
        );
        assert!(!js.contains("{{"));
    }

    #[test]
    fn should_preserve_conditional_navigation_links() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("backend")).unwrap();
        std::fs::create_dir_all(root.join("frontend/src")).unwrap();
        std::fs::write(
            root.join("backend/server.js"),
            "html.replace('<!--NAV-->', signedIn ? '<a href=\"/account\">Me</a> <a href=\"/logout\">Out</a>' : '<a href=\"/login\">In</a>')",
        )
        .unwrap();
        std::fs::write(
            root.join("frontend/src/index.html"),
            "<body><!--NAV--><a href=\"/login\">In</a><a href=\"/about\">About</a></body>",
        )
        .unwrap();
        let changed = dedupe_nav_links(root);
        assert!(changed.is_empty());
        let page = std::fs::read_to_string(root.join("frontend/src/index.html")).unwrap();
        assert!(page.contains("/login"), "legitimate entry point preserved");
        assert!(
            page.contains("/about"),
            "links the server does not render stay"
        );
    }

    #[test]
    fn should_extract_blocks_and_confine_paths() {
        let text = "Here you go.\n<<<FILE backend/server.js>>>\nconst x = 1;\n<<<END FILE>>>\n\
                    <<<FILE frontend/src/index.html >>>\n<p>hi</p>\n<<<END FILE>>>\n\
                    <<<FILE ../etc/passwd>>>\nno\n<<<END FILE>>>\n<<<FILE /abs/x>>>\nno\n<<<END FILE>>>\nDone.";
        let files = parse_file_blocks(text);
        assert_eq!(
            files.keys().collect::<Vec<_>>(),
            ["backend/server.js", "frontend/src/index.html"]
        );
        assert_eq!(files["backend/server.js"], "const x = 1;\n");
    }

    #[test]
    fn should_strip_a_stray_fence_and_keep_marker_like_code() {
        let files =
            parse_file_blocks("<<<FILE a.js>>>\n```js\nif (a <<< b) {}\n```\n<<<END FILE>>>");
        assert_eq!(files["a.js"], "if (a <<< b) {}\n");
        assert!(parse_file_blocks("just prose").is_empty());
    }

    #[test]
    fn should_inject_meta_charset_when_missing_and_keep_existing() {
        assert_eq!(
            ensure_charset("<html><head><title>x</title></head><body>账户</body></html>"),
            "<html><head><meta charset=\"utf-8\"><title>x</title></head><body>账户</body></html>"
        );
        assert_eq!(
            ensure_charset("<html><body>x</body></html>"),
            "<html><head><meta charset=\"utf-8\"></head><body>x</body></html>"
        );
        assert_eq!(
            ensure_charset("<p>x</p>"),
            "<meta charset=\"utf-8\">\n<p>x</p>"
        );
        let page = "<html><head><meta charset=\"UTF-8\"></head></html>";
        assert_eq!(ensure_charset(page), page);
    }

    #[test]
    fn should_restore_newlines_only_in_flattened_blocks() {
        let flat = format!("{}x", "const a = 1;\\n".repeat(12));
        let out = unescape_flattened(&flat);
        assert_eq!(out.matches('\n').count(), 12);
        assert!(!out.contains("\\n"));
        let normal = "res.end('a\\nb');\n".repeat(20);
        assert_eq!(unescape_flattened(&normal), normal);
    }

    #[test]
    fn should_unescape_js_only_when_it_makes_the_file_parse() {
        if js_parses(Path::new("/dev/null")).is_none() {
            return; // node not on PATH
        }
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("server.js");
        std::fs::write(
            &p,
            "const a = 1;\nfunction f() {\n  if (a) x = 1;\\n  if (!a) x = 2;\\n  return x;\n}\n",
        )
        .unwrap();
        assert!(repair_flattened_js(&p));
        assert!(!std::fs::read_to_string(&p).unwrap().contains("\\n"));
        let good = "const s = 'a\\nb';\nconsole.log(s);\n";
        std::fs::write(&p, good).unwrap();
        assert!(!repair_flattened_js(&p));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), good);
    }

    #[test]
    fn should_write_files_with_charset_and_manifests_once() {
        let dir = tempfile::tempdir().unwrap();
        let files = BTreeMap::from([
            (
                "frontend/src/index.html".to_string(),
                "<html><head></head><body></body></html>".to_string(),
            ),
            ("backend/server.js".to_string(), "x".to_string()),
        ]);
        let written = write_files(dir.path(), &files).unwrap();
        assert_eq!(written, ["backend/server.js", "frontend/src/index.html"]);
        assert!(
            std::fs::read_to_string(dir.path().join("frontend/src/index.html"))
                .unwrap()
                .contains(CHARSET_META)
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("backend/server.js")).unwrap(),
            "x"
        );
        assert_eq!(write_manifests(dir.path()).unwrap().len(), 2);
        assert!(write_manifests(dir.path()).unwrap().is_empty());
        let backend: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("backend/package.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(backend["type"], "commonjs");
        assert_eq!(backend["scripts"]["start"], "node server.js");
        let frontend: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("frontend/package.json")).unwrap(),
        )
        .unwrap();
        assert!(
            frontend["scripts"]["build"]
                .as_str()
                .unwrap()
                .contains("cpSync")
        );
    }

    #[test]
    fn should_preserve_links_even_when_server_has_a_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("frontend/src")).unwrap();
        std::fs::create_dir_all(dir.path().join("backend")).unwrap();
        let page =
            "<body><!--NAV--><a href=\"/login\">Login</a> <a href=\"/about\">About</a></body>";
        std::fs::write(dir.path().join("frontend/src/index.html"), page).unwrap();
        std::fs::write(dir.path().join("backend/server.js"), "no placeholder").unwrap();
        assert!(dedupe_nav_links(dir.path()).is_empty());
        // The server renders /login into the placeholder; /about is a page link it never renders.
        std::fs::write(
            dir.path().join("backend/server.js"),
            "const nav = '<a href=\"/login\">L</a>'; html.replace('<!--NAV-->', nav)",
        )
        .unwrap();
        assert!(dedupe_nav_links(dir.path()).is_empty());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("frontend/src/index.html")).unwrap(),
            page
        );
    }

    #[test]
    fn should_quote_small_sources_and_omit_those_over_budget() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("backend")).unwrap();
        std::fs::create_dir_all(dir.path().join("frontend/src")).unwrap();
        std::fs::create_dir_all(dir.path().join("frontend/node_modules")).unwrap();
        std::fs::write(dir.path().join("backend/server.js"), "x".repeat(100)).unwrap();
        std::fs::write(dir.path().join("frontend/src/index.html"), "<p>hi</p>").unwrap();
        std::fs::write(dir.path().join("frontend/node_modules/a.js"), "no").unwrap();
        let text = inline_sources(&Prompts::builtin(), dir.path(), 50, false);
        assert!(text.contains("--- frontend/src/index.html ---\n<p>hi</p>"));
        assert!(text.contains("backend/server.js --- (omitted, 100 chars"));
        assert!(!text.contains("node_modules"));
        let listing = source_listing(dir.path(), 60);
        assert!(listing.contains("backend/server.js (100 B)"));
    }

    #[test]
    fn should_build_the_compact_prompt_with_size_rule_and_port_clause() {
        let prompts = Prompts::builtin();
        let inputs = CodegenInputs {
            node_id: "REQ-1",
            description: "The home page shows a count.",
            spec: "test('x', ...)",
            web_port: 3000,
            extra_ports: &[3301],
            n_nodes: 1,
            small_rule: true,
            existing_app: None,
            context_chars: 90000,
        };
        let prompt = implement_prompt(&prompts, &inputs).unwrap();
        assert!(prompt.starts_with("Requirement REQ-1: The home page shows a count.\n"));
        assert!(prompt.contains("process.env.PORT||3000"));
        assert!(prompt.contains("ALSO listen on 3301"));
        assert!(prompt.contains(prompts.get("codegen-size-small")));
        assert!(prompt.contains("\nFiles: frontend/src/index.html"));
        let user = with_format(&prompts, &prompt);
        assert!(user.ends_with("<<<END FILE>>>"));
        assert!(!user.ends_with('\n'));
    }
}
