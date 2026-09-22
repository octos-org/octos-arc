//! Local acceptance testing (`arc/acceptance.py`).
//!
//! Runs the public (or container-provided) Playwright specs that belong to a
//! requirement node against the generated app and turns failures into the
//! compact four-field digest (Feature / Failed at / Observation / Steps) the
//! model sees. Spec discovery, spec → node mapping and report parsing are
//! pure functions; process handling lives in [`AppServer`] and
//! [`AcceptanceRunner`].

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use eyre::Result;
use regex::Regex;
use serde_json::Value;

use crate::envs::{self, EnvVec};
use crate::process::{self, ManagedChild};

static ANSI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*[A-Za-z]").unwrap());
static SPEC_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(REQ-\d+(?:\.\d+)*)").unwrap());
static BASE_PORT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https?://(?:127\.0\.0\.1|localhost):(\d{2,5})").unwrap());

// ---------------------------------------------------------------- spec mapping

/// `REQ-1.2-user-login.spec.ts` → `REQ-1.2`; non-spec files → None.
pub fn spec_node_id(rel_path: &str) -> Option<String> {
    let name = Path::new(rel_path).file_name()?.to_str()?;
    if !name.ends_with(".spec.ts") {
        return None;
    }
    let captures = SPEC_ID.captures(name)?;
    let id = captures.get(1)?.as_str();
    let rest = &name[id.len()..];
    let boundary = rest.is_empty() || rest.starts_with(['.', '-', '_', ' ']);
    boundary.then(|| id.to_string())
}

fn version_key(id: &str) -> Vec<u64> {
    let mut key = Vec::new();
    let mut current = String::new();
    for c in id.chars().chain(std::iter::once('x')) {
        if c.is_ascii_digit() {
            current.push(c);
        } else if !current.is_empty() {
            key.push(current.parse().unwrap_or(0));
            current.clear();
        }
    }
    key
}

/// Spec files assigned to requirement nodes (`acceptance.map_specs_to_nodes`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpecMap {
    /// node id → spec paths (every node has an entry, possibly empty).
    pub by_node: BTreeMap<String, Vec<String>>,
    /// Specs that belong to no single node (regression set).
    pub unassigned: Vec<String>,
    /// spec id → node id for spec ids that are not literal node ids.
    pub aliases: BTreeMap<String, String>,
}

impl SpecMap {
    pub fn specs_for(&self, node_id: &str) -> &[String] {
        self.by_node.get(node_id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Spec-side ids that mirror `node_id` (`Flow.mark` alias mirroring).
    pub fn aliases_for(&self, node_id: &str) -> Vec<String> {
        self.aliases
            .iter()
            .filter(|(_, target)| target.as_str() == node_id)
            .map(|(alias, _)| alias.clone())
            .collect()
    }

    /// spec file basename → owning node id.
    pub fn owners(&self) -> BTreeMap<String, String> {
        let mut owner = BTreeMap::new();
        for (node_id, paths) in &self.by_node {
            for path in paths {
                if let Some(name) = Path::new(path).file_name() {
                    owner.insert(name.to_string_lossy().into_owned(), node_id.clone());
                }
            }
        }
        owner
    }
}

/// Order of preference: literal id match; when the remaining distinct spec
/// ids and the remaining nodes have the same count, pair them in numeric /
/// document order; otherwise attach `REQ-1.x` to an existing `REQ-1` parent.
pub fn map_specs_to_nodes(spec_paths: &[String], node_ids: &[String]) -> SpecMap {
    let mut map = SpecMap::default();
    for id in node_ids {
        map.by_node.entry(id.clone()).or_default();
    }
    let mut by_spec_id: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for path in spec_paths {
        if let Some(sid) = spec_node_id(path) {
            if !by_spec_id.contains_key(&sid) {
                order.push(sid.clone());
            }
            by_spec_id.entry(sid).or_default().push(path.clone());
        }
    }
    order.sort_by_key(|sid| version_key(sid));
    let mut unmatched: Vec<String> = Vec::new();
    for sid in &order {
        if node_ids.contains(sid) {
            map.by_node
                .get_mut(sid)
                .unwrap()
                .extend(by_spec_id[sid].clone());
        } else {
            unmatched.push(sid.clone());
        }
    }
    let free_nodes: Vec<String> = node_ids
        .iter()
        .filter(|id| map.by_node[*id].is_empty())
        .cloned()
        .collect();
    if !unmatched.is_empty() && unmatched.len() == free_nodes.len() {
        for (sid, nid) in unmatched.iter().zip(&free_nodes) {
            map.by_node
                .get_mut(nid)
                .unwrap()
                .extend(by_spec_id[sid].clone());
            map.aliases.insert(sid.clone(), nid.clone());
        }
        return map;
    }
    for sid in unmatched {
        let mut parent = sid.clone();
        let mut target: Option<String> = None;
        while let Some((head, _)) = parent.rsplit_once('.') {
            parent = head.to_string();
            if node_ids.contains(&parent) {
                target = Some(parent.clone());
                break;
            }
        }
        match target {
            None => map.unassigned.extend(by_spec_id[&sid].clone()),
            Some(target) => {
                map.by_node
                    .get_mut(&target)
                    .unwrap()
                    .extend(by_spec_id[&sid].clone());
                map.aliases.insert(sid, target);
            }
        }
    }
    map
}

fn walk_ts(dir: &Path, root: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if name != "node_modules" {
                walk_ts(&path, root, out);
            }
        } else if name.ends_with(".ts") {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push(rel);
        }
    }
}

/// Sorted relative paths of every `*.spec.ts` under the tests directory.
pub fn list_specs(tests_dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    walk_ts(tests_dir, tests_dir, &mut out);
    out.retain(|p| p.ends_with(".spec.ts"));
    out.sort();
    out
}

/// Sorted relative paths of the non-spec `*.ts` helpers.
pub fn support_files(tests_dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    walk_ts(tests_dir, tests_dir, &mut out);
    out.retain(|p| !p.ends_with(".spec.ts"));
    out.sort();
    out
}

/// Ports the specs hard-code as their default base URL (e.g. 3301).
pub fn spec_base_ports(tests_dir: &Path) -> Vec<u16> {
    let mut all = Vec::new();
    walk_ts(tests_dir, tests_dir, &mut all);
    let mut ports: Vec<u16> = Vec::new();
    for rel in all {
        let Ok(text) = std::fs::read_to_string(tests_dir.join(&rel)) else {
            continue;
        };
        for captures in BASE_PORT.captures_iter(&text) {
            if let Ok(port) = captures[1].parse::<u16>()
                && !ports.contains(&port)
            {
                ports.push(port);
            }
        }
    }
    ports.sort_unstable();
    ports
}

// ---------------------------------------------------------------- report parsing

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct TestOutcome {
    pub title: String,
    pub ok: bool,
    pub status: String,
    pub duration_ms: u64,
    /// The SPEC file the test lives in (node ownership).
    pub file: String,
    pub line: Option<u64>,
    /// Where the error was raised (may be a helper file).
    pub location: String,
    pub message: String,
    pub steps: Vec<String>,
    pub action_errors: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RunSummary {
    pub passed: usize,
    pub total: usize,
    pub results: Vec<TestOutcome>,
    pub stdout_tail: String,
    /// Infrastructure error (no report).
    pub error: Option<String>,
    /// The test runner itself was killed (OOM); not a verdict.
    pub killed: bool,
    /// Playwright top-level errors.
    pub load_errors: Vec<String>,
}

impl RunSummary {
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            error: Some(message.into()),
            ..Default::default()
        }
    }

    pub fn slow(&self, threshold_ms: u64) -> Vec<String> {
        self.results
            .iter()
            .filter(|r| r.duration_ms >= threshold_ms)
            .map(|r| r.title.clone())
            .collect()
    }

    pub fn all_passed(&self) -> bool {
        self.total > 0 && self.passed == self.total
    }

    pub fn from_results(results: Vec<TestOutcome>) -> Self {
        let total = results.len();
        let passed = results.iter().filter(|r| r.ok).count();
        Self {
            passed,
            total,
            results,
            ..Default::default()
        }
    }
}

fn basename(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn text_of(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

/// Collapse a Playwright JSON report into per-test outcomes.
pub fn summarize_report(report: &Value) -> RunSummary {
    let mut results = Vec::new();
    fn walk(
        suites: Option<&Value>,
        parent_file: &str,
        results: &mut Vec<TestOutcome>,
        actions: &Value,
    ) {
        for suite in suites.and_then(Value::as_array).into_iter().flatten() {
            let suite_file = suite.get("file").and_then(Value::as_str).unwrap_or("");
            let file = if suite_file.is_empty() {
                parent_file
            } else {
                suite_file
            };
            for spec in suite
                .get("specs")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let tests: Vec<&Value> = spec
                    .get("tests")
                    .and_then(Value::as_array)
                    .map(|t| t.iter().collect())
                    .unwrap_or_default();
                let all_results: Vec<&Value> = tests
                    .iter()
                    .flat_map(|t| {
                        t.get("results")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                    })
                    .collect();
                let empty = Value::Object(Default::default());
                let last = all_results.last().copied().unwrap_or(&empty);
                let ok = !tests.is_empty()
                    && tests.iter().all(|t| {
                        t.get("status").and_then(Value::as_str) == Some("expected")
                            || t.get("ok").and_then(Value::as_bool).unwrap_or(false)
                    });
                let err = last
                    .get("error")
                    .filter(|e| e.is_object())
                    .or_else(|| {
                        last.get("errors")
                            .and_then(Value::as_array)
                            .and_then(|e| e.first())
                    })
                    .cloned()
                    .unwrap_or(Value::Object(Default::default()));
                let loc = err.get("location").cloned().unwrap_or(Value::Null);
                let steps: Vec<String> = last
                    .get("steps")
                    .and_then(Value::as_array)
                    .map(|s| {
                        s.iter()
                            .filter_map(|step| step.get("title").and_then(Value::as_str))
                            .filter(|t| !t.is_empty())
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default();
                let loc_file = basename(&text_of(loc.get("file")));
                let line = loc.get("line").and_then(Value::as_u64);
                let spec_file = text_of(spec.get("file"));
                let owner = if !spec_file.is_empty() {
                    spec_file
                } else if !file.is_empty() {
                    file.to_string()
                } else {
                    loc_file.clone()
                };
                results.push(TestOutcome {
                    title: spec
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("?")
                        .to_string(),
                    ok,
                    status: last
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string(),
                    duration_ms: all_results
                        .iter()
                        .map(|r| r.get("duration").and_then(Value::as_f64).unwrap_or(0.0))
                        .sum::<f64>() as u64,
                    file: basename(&owner),
                    line,
                    location: match (loc_file.is_empty(), line) {
                        (false, Some(line)) => format!("{loc_file}:{line}"),
                        _ => loc_file.clone(),
                    },
                    message: ANSI
                        .replace_all(
                            &format!(
                                "{}\n{}",
                                text_of(err.get("message")),
                                text_of(err.get("stack"))
                                    .lines()
                                    .filter(|line| line.trim().starts_with("at "))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            ),
                            "",
                        )
                        .trim()
                        .to_string(),
                    steps,
                    action_errors: spec
                        .get("id")
                        .and_then(Value::as_str)
                        .and_then(|id| actions.get(id))
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .take(8)
                        .filter_map(Value::as_str)
                        .map(|s| ANSI.replace_all(s, "").chars().take(2000).collect())
                        .collect(),
                });
            }
            walk(suite.get("suites"), file, results, actions);
        }
    }
    walk(
        report.get("suites"),
        "",
        &mut results,
        report.get("action_errors").unwrap_or(&Value::Null),
    );
    let mut summary = RunSummary::from_results(results);
    summary.load_errors = report
        .get("errors")
        .and_then(Value::as_array)
        .map(|errors| {
            errors
                .iter()
                .map(|e| {
                    let text = match e.get("message") {
                        Some(Value::String(s)) => s.clone(),
                        _ => e.to_string(),
                    };
                    ANSI.replace_all(&text, "").chars().take(600).collect()
                })
                .collect()
        })
        .unwrap_or_default();
    summary
}

/// Group failed outcomes by the requirement node that owns their spec file
/// (matched on the spec file's basename); unmapped files land under `None`.
pub fn nodes_for_failures(
    results: &[TestOutcome],
    spec_map: &SpecMap,
) -> BTreeMap<Option<String>, Vec<TestOutcome>> {
    let owner = spec_map.owners();
    let mut grouped: BTreeMap<Option<String>, Vec<TestOutcome>> = BTreeMap::new();
    for r in results.iter().filter(|r| !r.ok) {
        grouped
            .entry(owner.get(&basename(&r.file)).cloned())
            .or_default()
            .push(r.clone());
    }
    grouped
}

/// Playwright's `Call log:` lines are the closest thing to a step trace when
/// the spec has no test.step() blocks.
fn call_log_steps(message: &str) -> Vec<String> {
    let mut steps = Vec::new();
    let mut seen = false;
    for line in message.lines() {
        if line.trim().to_ascii_lowercase().starts_with("call log") {
            seen = true;
            continue;
        }
        if seen {
            let stripped = line.trim().trim_start_matches('-').trim();
            if stripped.is_empty() {
                break;
            }
            steps.push(stripped.chars().take(120).collect());
        }
    }
    steps
}

/// Bounded source evidence from the read-only acceptance tree.
pub fn failure_source_context(
    summary: &RunSummary,
    tests_dir: Option<&Path>,
    max_chars: usize,
) -> String {
    let Some(root) = tests_dir.and_then(|p| p.canonicalize().ok()) else {
        return String::new();
    };
    let mut files = Vec::new();
    fn collect(dir: &Path, files: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() && !matches!(entry.file_name().to_str(), Some("node_modules" | ".git"))
            {
                collect(&path, files);
            } else if kind.is_file() && path.extension().is_some_and(|x| x == "ts") {
                files.push(path);
            }
        }
    }
    collect(&root, &mut files);
    let mut out = String::new();
    let mut seen = BTreeSet::new();
    static FRAME: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)^\s*at (?:[^ (][^(]*\()?([^\n()]+\.ts):(\d+):\d+\)?\s*$").unwrap()
    });
    let mut locations = Vec::new();
    for result in &summary.results {
        locations.push(result.clone());
        for captures in FRAME.captures_iter(&result.message) {
            let mut frame = result.clone();
            frame.location = format!("{}:{}", &captures[1], &captures[2]);
            frame.line = captures[2].parse().ok();
            locations.push(frame);
        }
    }
    for r in locations.iter().filter(|r| !r.ok) {
        let Some(line) = r
            .line
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n > 0)
        else {
            continue;
        };
        let name = r
            .location
            .rsplit_once(':')
            .map(|(p, _)| p)
            .unwrap_or(&r.file);
        let matches: Vec<_> = files
            .iter()
            .filter(|p| p.file_name() == Path::new(name).file_name())
            .collect();
        if matches.len() != 1 {
            continue;
        }
        let path = matches[0];
        if !seen.insert((path.clone(), line))
            || std::fs::metadata(path).map_or(true, |m| m.len() > 1_000_000)
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let lines: Vec<_> = text.lines().collect();
        if line > lines.len() {
            continue;
        }
        out.push_str(&format!(
            "\n\nRead-only failure source: {}\n",
            path.strip_prefix(&root).unwrap().display()
        ));
        for (n, text) in lines
            .iter()
            .enumerate()
            .take(line.saturating_add(4))
            .skip(line.saturating_sub(5))
        {
            out.push_str(&format!(
                "{} {}: {}\n",
                if n + 1 == line { ">" } else { " " },
                n + 1,
                text.chars().take(400).collect::<String>()
            ));
        }
        if out.chars().count() >= max_chars {
            break;
        }
    }
    out.chars().take(max_chars).collect()
}

/// Keep behavioral evidence while ignoring timing and repeated polling noise.
pub fn failure_signature(summary: &RunSummary) -> BTreeSet<Vec<String>> {
    static TIME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b\d+(?:\.\d+)?\s*ms\b").unwrap());
    static REPEATS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b\d+\s*×").unwrap());
    fn normalize(text: &str) -> String {
        let text = ANSI.replace_all(text, "");
        let text = TIME.replace_all(&text, "<time>");
        REPEATS
            .replace_all(&text, "<repeats>")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }
    summary
        .results
        .iter()
        .filter(|r| !r.ok)
        .map(|r| {
            vec![
                r.file.clone(),
                r.title.clone(),
                r.status.clone(),
                r.location.clone(),
                normalize(&r.message),
                normalize(&r.steps.join(" | ")),
            ]
        })
        .collect()
}

/// Four-field digest of every failed test — the only thing the model sees.
pub fn failure_summaries(
    summary: &RunSummary,
    max_steps: usize,
    max_observation: usize,
    test_timeout_ms: u64,
) -> String {
    let mut blocks = Vec::new();
    for r in summary.results.iter().filter(|r| !r.ok) {
        let mut observation = r.message.trim().to_string();
        if observation.is_empty() {
            observation = format!("status {}", r.status);
        }
        let head: String = observation
            .chars()
            .take(120)
            .collect::<String>()
            .to_ascii_lowercase();
        if r.status == "timedOut" || head.contains("timeout") {
            observation = format!(
                "TIMED OUT after {} ms (the grader kills a test at {} s; the page or a request never settled). {}",
                r.duration_ms,
                test_timeout_ms / 1000,
                observation
            );
        }
        let observation: String = observation.chars().take(max_observation).collect();
        let mut where_ = if !r.location.is_empty() {
            r.location.clone()
        } else if !r.file.is_empty() {
            r.file.clone()
        } else {
            "?".into()
        };
        if !r.location.is_empty() && !r.file.is_empty() && !r.location.starts_with(&r.file) {
            where_ = format!("{} (called from {})", r.location, r.file);
        }
        let steps_src = if r.steps.is_empty() {
            call_log_steps(&r.message)
        } else {
            r.steps.clone()
        };
        let steps = if steps_src.is_empty() {
            "(no step trace)".to_string()
        } else {
            let start = steps_src.len().saturating_sub(max_steps);
            steps_src[start..].join(" -> ")
        };
        blocks.push(format!(
            "- Feature: {}\n  Failed at: {where_}\n  Observation: {observation}\n  Steps: {steps}",
            r.title
        ));
        if !r.action_errors.is_empty() {
            let detail: String = r.action_errors.join("\n").chars().take(4000).collect();
            blocks.last_mut().unwrap().push_str(&format!("\n  Browser diagnostics (helpers may have recovered; correlate with the final failure):\n{detail}"));
        }
    }
    blocks.join("\n")
}

// ---------------------------------------------------------------- playwright discovery

/// First directory that has @playwright/test installed.
pub fn find_playwright_root(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates
        .iter()
        .find(|c| c.join("node_modules/@playwright/test").is_dir())
        .and_then(|c| c.canonicalize().ok())
}

/// Places an existing @playwright/test may live, most specific first. The
/// runner image ships one; finding it matters because installing our own
/// must never change what the platform's own `npx playwright test` resolves.
pub fn playwright_candidates(
    explicit_root: Option<&Path>,
    bundle_dir: Option<&Path>,
    tests_dir: Option<&Path>,
    output_dir: &Path,
) -> Vec<PathBuf> {
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Some(root) = explicit_root {
        cands.push(root.to_path_buf());
    }
    if let Some(bundle) = bundle_dir {
        cands.push(bundle.join("local-grader"));
    }
    if let Some(tests) = tests_dir {
        cands.push(tests.to_path_buf());
        cands.extend(tests.ancestors().skip(1).take(3).map(Path::to_path_buf));
    }
    cands.push(output_dir.to_path_buf());
    for fixed in [
        "/workspace",
        "/workspace/tests",
        "/app",
        "/runner",
        "/opt/playwright",
        "/ms-playwright",
    ] {
        cands.push(PathBuf::from(fixed));
    }
    if let Some(home) = std::env::var_os("HOME") {
        cands.push(PathBuf::from(home));
    }
    if let Ok(output) = process::run(
        Path::new("npm"),
        &[OsString::from("root"), OsString::from("-g")],
        output_dir,
        &envs::inherited(&[]),
        Instant::now() + Duration::from_secs(20),
    ) && output.succeeded()
        && let Some(parent) = Path::new(output.stdout.trim()).parent()
        && !output.stdout.trim().is_empty()
    {
        cands.push(parent.to_path_buf());
    }
    for fixed in ["/usr/local/lib", "/usr/lib", "/usr/local", "/opt"] {
        cands.push(PathBuf::from(fixed));
    }
    cands
}

/// Bounded filesystem search for a preinstalled @playwright/test package.
pub fn find_playwright_by_search(
    notes: &mut Vec<String>,
    max_depth: u32,
    timeout: Duration,
) -> Option<PathBuf> {
    let args: Vec<OsString> = [
        "/",
        "-maxdepth",
        &max_depth.to_string(),
        "-type",
        "d",
        "-path",
        "*/node_modules/@playwright/test",
        "-not",
        "-path",
        "/proc/*",
        "-not",
        "-path",
        "/sys/*",
        "-not",
        "-path",
        "/tmp/*",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    let output = process::run(
        Path::new("find"),
        &args,
        Path::new("/"),
        &envs::inherited(&[]),
        Instant::now() + timeout,
    )
    .ok()?;
    let mut hits: Vec<&str> = output.stdout.split_whitespace().collect();
    hits.sort_by_key(|h| h.len());
    for hit in hits {
        let root = Path::new(hit).ancestors().nth(3)?;
        if root
            .join("node_modules/@playwright/test/package.json")
            .is_file()
        {
            notes.push(format!(
                "[acceptance] found preinstalled Playwright at {}",
                root.display()
            ));
            return Some(root.to_path_buf());
        }
    }
    None
}

/// Version to install when we must: the one the tests declare, else a pin.
/// Never `latest` — an unpinned install is what broke cloud grading.
pub fn playwright_version_hint(tests_dir: Option<&Path>, fallback: &str) -> String {
    let bases: Vec<PathBuf> = tests_dir
        .map(|t| t.ancestors().take(3).map(Path::to_path_buf).collect())
        .unwrap_or_default();
    for base in bases {
        for name in ["package-lock.json", "package.json"] {
            let Ok(text) = std::fs::read_to_string(base.join(name)) else {
                continue;
            };
            let Ok(data) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let mut version = data
                .get("packages")
                .and_then(|p| p.get("node_modules/@playwright/test"))
                .and_then(|p| p.get("version"))
                .and_then(Value::as_str)
                .map(String::from);
            if version.is_none() {
                for section in ["devDependencies", "dependencies"] {
                    if let Some(v) = data
                        .get(section)
                        .and_then(|s| s.get("@playwright/test"))
                        .and_then(Value::as_str)
                    {
                        version = Some(v.to_string());
                        break;
                    }
                }
            }
            if let Some(v) = version {
                return v.trim_start_matches(['^', '~', '=', 'v']).to_string();
            }
        }
    }
    fallback.to_string()
}

/// Environment for a self-contained Playwright install: private npm cache and
/// browser directory, mirrors for the runner network. Nothing outside
/// `private_root` is written.
pub fn isolated_install_env(private_root: &Path) -> EnvVec {
    let cache = private_root
        .join("npm-cache")
        .to_string_lossy()
        .into_owned();
    let browsers = private_root.join("browsers").to_string_lossy().into_owned();
    envs::inherited(&[
        (
            "npm_config_registry",
            Some("https://registry.npmmirror.com"),
        ),
        ("npm_config_cache", Some(&cache)),
        ("NPM_CONFIG_CACHE", Some(&cache)),
        ("npm_config_update_notifier", Some("false")),
        (
            "PLAYWRIGHT_DOWNLOAD_HOST",
            Some("https://npmmirror.com/mirrors/playwright"),
        ),
        ("PLAYWRIGHT_BROWSERS_PATH", Some(&browsers)),
    ])
}

/// Self-contained install of @playwright/test@<version> + chromium under
/// `install_root`. Returns (root, env_extra) — env_extra must be passed to
/// every run that uses this install.
pub fn ensure_playwright(
    install_root: &Path,
    notes: &mut Vec<String>,
    timeout: Duration,
    version: &str,
) -> Option<(PathBuf, Vec<(String, String)>)> {
    std::fs::create_dir_all(install_root).ok()?;
    let env = isolated_install_env(install_root);
    std::fs::write(
        install_root.join("package.json"),
        "{\"name\": \"octos-arc-acceptance\", \"private\": true}",
    )
    .ok()?;
    let started = Instant::now();
    let deadline = started + timeout;
    let bin = install_root.join("node_modules/.bin/playwright");
    let steps: Vec<(PathBuf, Vec<OsString>)> = vec![
        (
            PathBuf::from("npm"),
            [
                "install",
                "--no-audit",
                "--no-fund",
                "--no-package-lock",
                &format!("@playwright/test@{version}"),
            ]
            .iter()
            .map(OsString::from)
            .collect(),
        ),
        (
            bin,
            ["install", "chromium"].iter().map(OsString::from).collect(),
        ),
    ];
    for (program, args) in steps {
        if Instant::now() >= deadline {
            notes.push("[acceptance] playwright install timed out".into());
            return None;
        }
        let label = format!(
            "{} {}",
            basename(&program.to_string_lossy()),
            args[0].to_string_lossy()
        );
        match process::run(&program, &args, install_root, &env, deadline) {
            Ok(output) if output.succeeded() => {}
            Ok(output) => {
                let tail: String = if output.stderr.is_empty() {
                    &output.stdout
                } else {
                    &output.stderr
                }
                .chars()
                .rev()
                .take(300)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
                notes.push(format!(
                    "[acceptance] playwright install step {label} rc={:?}: {tail}",
                    output.exit_code
                ));
                return None;
            }
            Err(error) => {
                notes.push(format!(
                    "[acceptance] playwright install step {label} failed: {error}"
                ));
                return None;
            }
        }
    }
    let root = find_playwright_root(&[install_root.to_path_buf()])?;
    let browsers = envs::lookup(&env, "PLAYWRIGHT_BROWSERS_PATH")?
        .to_string_lossy()
        .into_owned();
    notes.push(format!(
        "[acceptance] playwright {version} installed privately into {} in {}s (browsers under {browsers})",
        install_root.display(),
        started.elapsed().as_secs()
    ));
    Some((root, vec![("PLAYWRIGHT_BROWSERS_PATH".into(), browsers)]))
}

/// cgroup memory limit in bytes (v2 memory.max or v1 limit_in_bytes), None
/// when unlimited/unknown.
pub fn container_memory_limit() -> Option<u64> {
    for path in [
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
    ] {
        let Ok(raw) = std::fs::read_to_string(path) else {
            continue;
        };
        if let Ok(value) = raw.trim().parse::<u64>()
            && value < 1 << 50
        {
            return Some(value);
        }
    }
    None
}

/// One Chromium worker per `per_worker_mib` of container memory, at least 1.
pub fn workers_for_memory(limit: Option<u64>, requested: u32, per_worker_mib: u64) -> u32 {
    match limit {
        None | Some(0) => requested,
        Some(limit) => {
            let fit = (limit / (per_worker_mib * 1024 * 1024)) as u32;
            requested.min(fit).max(1)
        }
    }
}

// ---------------------------------------------------------------- ports & processes

pub(crate) fn lsof_pids(port: u16) -> Vec<u32> {
    let args: Vec<OsString> = ["-ti", &format!(":{port}")]
        .iter()
        .map(OsString::from)
        .collect();
    match process::run(
        Path::new("lsof"),
        &args,
        Path::new("/"),
        &envs::inherited(&[]),
        Instant::now() + Duration::from_secs(15),
    ) {
        Ok(output) => output
            .stdout
            .split_whitespace()
            .filter_map(|p| p.parse().ok())
            .collect(),
        Err(_) => Vec::new(),
    }
}

pub(crate) fn kill_pid(pid: u32) {
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Kill every listener on `port`.
pub fn free_port(port: u16) {
    for pid in lsof_pids(port) {
        kill_pid(pid);
    }
}

pub(crate) fn process_cwd(pid: u32) -> String {
    if let Ok(link) = std::fs::read_link(format!("/proc/{pid}/cwd")) {
        return link.to_string_lossy().into_owned();
    }
    let args: Vec<OsString> = ["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"]
        .iter()
        .map(OsString::from)
        .collect();
    if let Ok(output) = process::run(
        Path::new("lsof"),
        &args,
        Path::new("/"),
        &envs::inherited(&[]),
        Instant::now() + Duration::from_secs(15),
    ) {
        for line in output.stdout.lines() {
            if let Some(path) = line.strip_prefix("n/") {
                return format!("/{path}");
            }
        }
    }
    String::new()
}

/// Kill listeners on `ports` that were started from inside `root` (our own
/// leftovers), leaving foreign processes alone.
pub fn free_owned_ports(ports: &[u16], root: &Path) {
    let root = root.to_string_lossy().into_owned();
    for &port in ports {
        for pid in lsof_pids(port) {
            if process_cwd(pid).starts_with(&root) {
                kill_pid(pid);
            }
        }
    }
}

pub fn port_open(port: u16) -> bool {
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_ok()
}

fn http_get(port: u16, path: &str, timeout: Duration) -> std::io::Result<()> {
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
            .as_bytes(),
    )?;
    let mut buffer = [0u8; 4096];
    let mut received = Vec::new();
    loop {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        received.extend_from_slice(&buffer[..count]);
        if received.windows(2).any(|w| w == b"\r\n") {
            break;
        }
    }
    if received.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "empty response",
        ));
    }
    Ok(())
}

/// Hit paths a browser or grader will request; the server must answer (any
/// status) and stay alive. An unhandled error on GET /favicon.ico killed a
/// backend and every test saw ECONNREFUSED.
pub fn robustness_probe(
    port: u16,
    mut child: Option<&mut ManagedChild>,
    timeout: Duration,
) -> Option<String> {
    for (index, path) in [
        "/favicon.ico",
        "/this-path-does-not-exist",
        "/api/this-route-does-not-exist",
    ]
    .into_iter()
    .enumerate()
    {
        let started = Instant::now();
        for attempt in 0..2 {
            let remaining = timeout
                .saturating_sub(started.elapsed())
                .max(Duration::from_millis(1));
            let Err(error) = http_get(port, path, remaining) else {
                break;
            };
            let mut alive = match child.as_deref_mut() {
                None => true,
                Some(child) => child.running().unwrap_or(false),
            };
            let transient = matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::WouldBlock
            );
            let delay = Duration::from_millis(200);
            if index == 0
                && attempt == 0
                && alive
                && transient
                && timeout.saturating_sub(started.elapsed()) > delay
            {
                std::thread::sleep(delay);
                alive = child
                    .as_deref_mut()
                    .is_none_or(|child| child.running().unwrap_or(false));
                if alive {
                    continue;
                }
            }
            return Some(format!(
                "GET {path} got no HTTP response ({}); backend {} — unknown paths must return 404, never throw",
                error.kind(),
                if alive {
                    "still running"
                } else {
                    "CRASHED (process exited)"
                }
            ));
        }
        std::thread::sleep(Duration::from_millis(200));
        if let Some(child) = child.as_deref_mut()
            && !child.running().unwrap_or(false)
        {
            return Some(format!(
                "backend process exited right after GET {path} — an unhandled exception in the static/API handler; missing files must return 404 and the process must never die"
            ));
        }
    }
    None
}

// ---------------------------------------------------------------- app server

/// Build the frontend once and run the backend on the smoke port.
/// `grader_like` starts the backend exactly as the platform does: only PORT
/// is set, so any extra spec ports get bound too. Per-node runs pass false
/// (`ARC_EXTRA_PORTS=0`) to stay off shared ports.
pub struct AppServer {
    pub project: PathBuf,
    pub port: u16,
    pub grader_like: bool,
    pub extra_ports: Vec<u16>,
    pub env_extra: Vec<(String, String)>,
    pub build_timeout: Duration,
    pub start_wait: Duration,
    child: Option<ManagedChild>,
    last_output: String,
}

fn tail(text: &str, n: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(n)).collect()
}

impl AppServer {
    pub fn new(project: &Path, port: u16, grader_like: bool, extra_ports: Vec<u16>) -> Self {
        Self {
            project: project.to_path_buf(),
            port,
            grader_like,
            extra_ports,
            env_extra: Vec::new(),
            build_timeout: Duration::from_secs(600),
            start_wait: Duration::from_secs(45),
            child: None,
            last_output: String::new(),
        }
    }

    fn env(&self, overrides: &[(&str, Option<&str>)]) -> EnvVec {
        let mut all: Vec<(&str, Option<&str>)> = self
            .env_extra
            .iter()
            .map(|(k, v)| (k.as_str(), Some(v.as_str())))
            .collect();
        all.extend_from_slice(overrides);
        envs::inherited(&all)
    }

    fn npm(&self, args: &[&str], cwd: &Path, timeout: Duration) -> (Option<i32>, String) {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        match process::run(
            Path::new("npm"),
            &args,
            cwd,
            &self.env(&[]),
            Instant::now() + timeout,
        ) {
            Ok(output) => {
                let text = format!("{}\n{}", output.stdout, output.stderr);
                let code = if output.timed_out {
                    Some(124)
                } else {
                    output.exit_code
                };
                (code, tail(text.trim(), 1500))
            }
            Err(error) => (Some(127), error.to_string()),
        }
    }

    /// `npm install` only when the manifest declares dependencies and
    /// node_modules is missing; create dist/ before the build so a one-line
    /// copy build never fails on a fresh checkout.
    pub fn build(&self) -> Option<String> {
        let frontend = self.project.join("frontend");
        let backend = self.project.join("backend");
        if !frontend.join("package.json").is_file() || !backend.join("package.json").is_file() {
            return Some("frontend/package.json or backend/package.json missing".into());
        }
        for part in [&frontend, &backend] {
            let deps = std::fs::read_to_string(part.join("package.json"))
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .and_then(|v| v.get("dependencies").cloned())
                .and_then(|d| d.as_object().map(|o| !o.is_empty()))
                .unwrap_or(false);
            if deps && !part.join("node_modules").is_dir() {
                let (rc, out) = self.npm(
                    &["install", "--no-audit", "--no-fund"],
                    part,
                    self.build_timeout,
                );
                if rc != Some(0) {
                    return Some(format!(
                        "{} `npm install` failed:\n{out}",
                        basename(&part.to_string_lossy())
                    ));
                }
            }
        }
        let _ = std::fs::create_dir_all(frontend.join("dist"));
        let (rc, out) = self.npm(&["run", "build"], &frontend, self.build_timeout);
        if rc != Some(0) {
            return Some(format!("frontend `npm run build` failed:\n{out}"));
        }
        None
    }

    /// Start the backend; None when it binds and survives the probes.
    pub fn start(&mut self) -> Option<String> {
        free_port(self.port);
        if self.grader_like {
            free_owned_ports(&self.extra_ports, &self.project);
        }
        let port = self.port.to_string();
        let extra: Option<&str> = if self.grader_like { None } else { Some("0") };
        let env = self.env(&[("PORT", Some(&port)), ("ARC_EXTRA_PORTS", extra)]);
        let child = match ManagedChild::spawn(
            Path::new("npm"),
            &[OsString::from("start")],
            &self.project.join("backend"),
            &env,
        ) {
            Ok(child) => child,
            Err(error) => return Some(format!("backend `npm start` could not launch: {error}")),
        };
        self.child = Some(child);
        let deadline = Instant::now() + self.start_wait;
        while Instant::now() < deadline {
            let running = self
                .child
                .as_mut()
                .map(|c| c.running().unwrap_or(false))
                .unwrap_or(false);
            if !running {
                let output = self.finish_child();
                return Some(format!(
                    "backend `npm start` exited early (rc={:?}):\n{}",
                    output.0,
                    tail(&output.1, 1500)
                ));
            }
            if port_open(self.port) {
                let mut error =
                    robustness_probe(self.port, self.child.as_mut(), Duration::from_secs(5));
                if error.is_none() && self.grader_like && !self.extra_ports.is_empty() {
                    error = self.extra_ports_bound(Duration::from_secs(5));
                }
                if let Some(error) = error {
                    self.stop();
                    return Some(format!(
                        "{error}\nserver log tail:\n{}",
                        tail(&self.last_output, 800)
                    ));
                }
                return None;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        self.stop();
        Some(format!(
            "backend did not bind port {} within {}s:\n{}",
            self.port,
            self.start_wait.as_secs(),
            tail(&self.last_output, 1500)
        ))
    }

    /// Grader-like start: every port the specs default to must answer too.
    pub fn extra_ports_bound(&self, wait: Duration) -> Option<String> {
        let deadline = Instant::now() + wait;
        let mut missing: Vec<u16> = self.extra_ports.clone();
        while !missing.is_empty() && Instant::now() < deadline {
            missing.retain(|p| !port_open(*p));
            if !missing.is_empty() {
                std::thread::sleep(Duration::from_millis(250));
            }
        }
        if missing.is_empty() {
            return None;
        }
        let ports = missing
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!(
            "PORT CONTRACT violated: the acceptance specs default to http://127.0.0.1:{ports} and the grader starts the backend with only PORT={}; the backend bound PORT but not port(s) {ports} (every test would fail with ERR_CONNECTION_REFUSED). Serve the same handler on each of these ports with a separate http.createServer(handler).listen(port) unless process.env.ARC_EXTRA_PORTS === '0'.",
            self.port
        ))
    }

    fn finish_child(&mut self) -> (Option<i32>, String) {
        let Some(child) = self.child.take() else {
            return (None, String::new());
        };
        match child.finish(Instant::now()) {
            Ok(output) => {
                self.last_output = format!("{}{}", output.stdout, output.stderr);
                (output.exit_code, self.last_output.clone())
            }
            Err(error) => (None, error.to_string()),
        }
    }

    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            child.stop();
            if let Ok(output) = child.finish(Instant::now()) {
                self.last_output = format!("{}{}", output.stdout, output.stderr);
            }
        }
        free_port(self.port);
        if self.grader_like {
            free_owned_ports(&self.extra_ports, &self.project);
        }
    }
}

impl Drop for AppServer {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------- playwright runner

/// A writable scratch dir under the Playwright root (module resolution),
/// falling back to the system temp dir.
pub fn acceptance_work_dir(root: &Path) -> PathBuf {
    let preferred = root
        .join(".octos-acceptance")
        .join(format!("run-{}", std::process::id()));
    if std::fs::create_dir_all(&preferred).is_ok() {
        preferred
    } else {
        std::env::temp_dir()
            .join(format!("octos-acceptance-{}", std::process::id()))
            .join("run")
    }
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if matches!(
            name_str.as_ref(),
            "node_modules" | "test-results" | "playwright-report"
        ) {
            continue;
        }
        let source = entry.path();
        let dest = to.join(&name);
        if source.is_dir() {
            copy_tree(&source, &dest)?;
        } else {
            std::fs::copy(&source, &dest)?;
        }
    }
    Ok(())
}

/// Run selected spec files from `tests_dir` with the Playwright install at `root`.
pub struct AcceptanceRunner {
    pub root: PathBuf,
    pub tests_dir: PathBuf,
    pub work_dir: PathBuf,
    pub timeout_ms: u64,
    pub workers: u32,
    pub fully_parallel: bool,
    pub env_extra: Vec<(String, String)>,
    pub wall_timeout: Duration,
}

impl AcceptanceRunner {
    /// Specs `import '@playwright/test'`; Node resolves that upward from the
    /// spec file, so the copied tree must sit under the Playwright install
    /// (NODE_PATH is set as well). Action/navigation/expect timeouts sit
    /// below the test timeout on purpose: a hanging click then fails with the
    /// locator named in the call log instead of an anonymous timeout.
    fn prepare(&self, workers: Option<u32>) -> Result<PathBuf> {
        if self.work_dir.exists() {
            std::fs::remove_dir_all(&self.work_dir)?;
        }
        copy_tree(&self.tests_dir, &self.work_dir.join("tests"))?;
        std::fs::write(
            self.work_dir.join("page_errors.ts"),
            include_str!("../../../arc/page_errors.ts"),
        )?;
        for relative in list_specs(&self.work_dir.join("tests")) {
            let spec = self.work_dir.join("tests").join(&relative);
            let source = std::fs::read_to_string(&spec)?;
            let mut alias = "__octosObservePageErrors".to_string();
            while source.contains(&alias) {
                alias.push('_');
            }
            let observer = format!(
                "{}page_errors",
                "../".repeat(Path::new(&relative).components().count())
            );
            std::fs::write(
                spec,
                format!(
                    "{source}\nimport {{ register as {alias} }} from {};\n{alias}();\n",
                    serde_json::to_string(&observer)?
                ),
            )?;
        }

        std::fs::write(
            self.work_dir.join("action_errors.cjs"),
            include_str!("../../../arc/action_errors.cjs"),
        )?;
        let sub = 4000.min(self.timeout_ms / 2);
        let nav = 6000.min(self.timeout_ms * 3 / 5);
        let config = format!(
            "import {{ defineConfig }} from '@playwright/test';\nexport default defineConfig({{ testDir: './tests', timeout: {}, retries: 0, fullyParallel: {}, workers: {}, reporter: [['list'], ['json', {{ outputFile: 'report.json' }}], ['./action_errors.cjs', {{ output: 'action-errors.json' }}]], expect: {{ timeout: {sub} }}, use: {{ headless: true, baseURL: process.env.E2E_BASE_URL, actionTimeout: {sub}, navigationTimeout: {nav} }} }});\n",
            self.timeout_ms,
            if self.fully_parallel { "true" } else { "false" },
            workers.unwrap_or(self.workers)
        );
        let path = self.work_dir.join("playwright.config.ts");
        std::fs::write(&path, config)?;
        Ok(path)
    }

    pub fn run(
        &self,
        spec_rel_paths: &[String],
        base_url: &str,
        workers: Option<u32>,
    ) -> RunSummary {
        let config = match self.prepare(workers) {
            Ok(path) => path,
            Err(error) => {
                return RunSummary::error(format!("could not prepare the acceptance run: {error}"));
            }
        };
        let report_path = self.work_dir.join("report.json");
        let mut args: Vec<OsString> = vec!["test".into(), "-c".into(), config.into_os_string()];
        args.extend(
            spec_rel_paths
                .iter()
                .map(|p| self.work_dir.join("tests").join(p).into_os_string()),
        );
        let node_path = self
            .root
            .join("node_modules")
            .to_string_lossy()
            .into_owned();
        let mut overrides: Vec<(&str, Option<&str>)> = vec![
            ("E2E_BASE_URL", Some(base_url)),
            ("CI", Some("1")),
            ("NODE_PATH", Some(&node_path)),
            ("FORCE_COLOR", None),
        ];
        overrides.extend(
            self.env_extra
                .iter()
                .map(|(k, v)| (k.as_str(), Some(v.as_str()))),
        );
        let env = envs::inherited(&overrides);
        let output = match process::run(
            &self.root.join("node_modules/.bin/playwright"),
            &args,
            &self.work_dir,
            &env,
            Instant::now() + self.wall_timeout,
        ) {
            Ok(output) => output,
            Err(error) => return RunSummary::error(format!("playwright could not start: {error}")),
        };
        if output.timed_out {
            return RunSummary::error(format!(
                "playwright run exceeded {}s",
                self.wall_timeout.as_secs()
            ));
        }
        let combined = format!("{}{}", output.stdout, output.stderr);
        let tail_text = tail(&combined, 2000);
        if !report_path.exists() {
            let killed = output.exit_code.is_none() || tail_text.contains("Killed");
            let message = if killed {
                format!(
                    "playwright was killed (rc={:?}); likely out of memory — not an application failure",
                    output.exit_code
                )
            } else {
                format!(
                    "playwright produced no report (rc={:?}): {}",
                    output.exit_code,
                    tail(&ANSI.replace_all(&tail_text, ""), 600)
                )
            };
            let mut summary = RunSummary::error(message);
            summary.killed = killed;
            return summary;
        }
        let mut report: Value = match std::fs::read_to_string(&report_path)
            .map_err(|e| e.to_string())
            .and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string()))
        {
            Ok(report) => report,
            Err(error) => {
                return RunSummary::error(format!("unreadable playwright report: {error}"));
            }
        };
        if let Some(actions) = std::fs::read_to_string(self.work_dir.join("action-errors.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .filter(Value::is_object)
        {
            if let Some(object) = report.as_object_mut() {
                object.insert("action_errors".into(), actions);
            }
        }
        let mut summary = summarize_report(&report);
        summary.stdout_tail = ANSI.replace_all(&tail_text, "").into_owned();
        if summary.total == 0 {
            // The model had edited the tests, the copied spec no longer loaded, and "0/0" looked like a verdict.
            let detail = if !summary.load_errors.is_empty() {
                summary.load_errors.join("; ")
            } else if !summary.stdout_tail.is_empty() {
                tail(&summary.stdout_tail, 600)
            } else {
                format!("rc={:?}", output.exit_code)
            };
            let mut error = RunSummary::error(format!(
                "Playwright collected 0 tests from {} (spec files unreadable or broken): {detail}",
                spec_rel_paths.join(", ")
            ));
            error.load_errors = summary.load_errors;
            return error;
        }
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn report_preserves_caller_source_evidence() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("helper.ts"), "assert_matches(expected);").unwrap();
        std::fs::write(
            dir.path().join("caller.spec.ts"),
            "const expected = /status ready/;\nawait check(expected);",
        )
        .unwrap();
        let report = serde_json::json!({"suites": [{"specs": [{"file": "caller.spec.ts", "title": "state", "tests": [{"results": [{"status": "failed", "error": {
            "message": "No matches", "location": {"file": "/tests/helper.ts", "line": 1},
            "stack": "Error: No matches\n    at check (/tests/helper.ts:1:1)\n    at /tests/caller.spec.ts:2:1"
        }}]}]}]}]});
        let summary = summarize_report(&report);
        let text = failure_source_context(&summary, Some(dir.path()), 4000);
        assert!(text.contains("assert_matches(expected)"));
        assert!(text.contains("const expected = /status ready/"));
        assert_eq!(
            text.matches("Read-only failure source: helper.ts").count(),
            1
        );
    }

    #[test]
    fn failure_source_uses_helper_line_and_skips_ambiguous_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("support")).unwrap();
        let source = (1..=20)
            .map(|n| format!("operation_{n}();"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.path().join("support/helper.ts"), &source).unwrap();
        let summary = RunSummary::from_results(vec![TestOutcome {
            title: "submit later".into(),
            location: "helper.ts:10".into(),
            line: Some(10),
            ..Default::default()
        }]);
        let text = failure_source_context(&summary, Some(dir.path()), 4000);
        assert!(text.contains("support/helper.ts"));
        assert!(text.contains("> 10: operation_10();"));
        assert!(!text.contains("operation_1();"));
        assert!(
            failure_source_context(&summary, Some(dir.path()), 50)
                .chars()
                .count()
                <= 50
        );
        std::fs::write(dir.path().join("helper.ts"), source).unwrap();
        assert!(failure_source_context(&summary, Some(dir.path()), 4000).is_empty());
    }

    #[test]
    fn stalled_repair_preserves_numeric_behavior_and_location() {
        let sample = |message: &str, location: &str| {
            RunSummary::from_results(vec![TestOutcome {
                title: "same test".into(),
                message: message.into(),
                location: location.into(),
                ..Default::default()
            }])
        };
        assert_eq!(
            failure_signature(&sample("Timeout 4000ms 2 × retry", "a:10")),
            failure_signature(&sample("Timeout 4100ms 3 × retry", "a:10"))
        );
        assert_ne!(
            failure_signature(&sample("Expected 200 received 404", "a:10")),
            failure_signature(&sample("Expected 200 received 500", "a:10"))
        );
        assert_ne!(
            failure_signature(&sample("missing", "a:10")),
            failure_signature(&sample("missing", "a:20"))
        );
    }

    #[test]
    fn should_extract_leading_requirement_id() {
        assert_eq!(spec_node_id("REQ-1.spec.ts").as_deref(), Some("REQ-1"));
        assert_eq!(
            spec_node_id("REQ-1.1-user-registration.spec.ts").as_deref(),
            Some("REQ-1.1")
        );
        assert_eq!(
            spec_node_id("sub/REQ-12.3.4-x.spec.ts").as_deref(),
            Some("REQ-12.3.4")
        );
        assert_eq!(spec_node_id("support/e2e.ts"), None);
        assert_eq!(spec_node_id("smoke.spec.ts"), None);
        assert_eq!(spec_node_id("REQ-1x.spec.ts"), None);
    }

    #[test]
    fn should_match_exact_ids() {
        let map = map_specs_to_nodes(
            &strings(&["REQ-1.spec.ts", "REQ-2.spec.ts"]),
            &strings(&["REQ-1", "REQ-2"]),
        );
        assert_eq!(map.specs_for("REQ-1"), ["REQ-1.spec.ts"]);
        assert_eq!(map.specs_for("REQ-2"), ["REQ-2.spec.ts"]);
        assert!(map.unassigned.is_empty() && map.aliases.is_empty());
    }

    #[test]
    fn should_map_in_order_when_spec_ids_differ_but_counts_match() {
        let specs = strings(&[
            "REQ-1.1-user-registration.spec.ts",
            "REQ-1.2-user-login.spec.ts",
            "support/e2e.ts",
        ]);
        let map = map_specs_to_nodes(&specs, &strings(&["REQ-1", "REQ-2"]));
        assert_eq!(
            map.specs_for("REQ-1"),
            ["REQ-1.1-user-registration.spec.ts"]
        );
        assert_eq!(map.specs_for("REQ-2"), ["REQ-1.2-user-login.spec.ts"]);
        assert_eq!(map.aliases_for("REQ-1"), ["REQ-1.1"]);
        assert_eq!(map.aliases_for("REQ-2"), ["REQ-1.2"]);
    }

    #[test]
    fn should_fall_back_to_parent_prefix_and_leave_rest_unassigned() {
        let specs = strings(&["REQ-1.1-a.spec.ts", "REQ-1.2-b.spec.ts", "REQ-9.spec.ts"]);
        let map = map_specs_to_nodes(&specs, &strings(&["REQ-1", "REQ-2"]));
        assert_eq!(
            map.specs_for("REQ-1"),
            ["REQ-1.1-a.spec.ts", "REQ-1.2-b.spec.ts"]
        );
        assert!(map.specs_for("REQ-2").is_empty());
        assert_eq!(map.unassigned, ["REQ-9.spec.ts"]);
        assert_eq!(map.aliases["REQ-1.1"], "REQ-1");
        assert_eq!(map.aliases["REQ-1.2"], "REQ-1");
    }

    #[test]
    fn should_sort_spec_ids_numerically_when_mapping_in_order() {
        let map = map_specs_to_nodes(
            &strings(&["REQ-1.10-x.spec.ts", "REQ-1.2-y.spec.ts"]),
            &strings(&["A", "B"]),
        );
        assert_eq!(map.specs_for("A"), ["REQ-1.2-y.spec.ts"]);
        assert_eq!(map.specs_for("B"), ["REQ-1.10-x.spec.ts"]);
    }

    /// (title, status, error, steps, duration_ms)
    type Case<'a> = (&'a str, &'a str, Option<&'a str>, &'a [&'a str], u64);

    fn report(tests: &[Case<'_>]) -> Value {
        let specs: Vec<Value> = tests
            .iter()
            .map(|(title, status, error, steps, duration)| {
                let mut result = json!({"status": status, "duration": duration,
                    "steps": steps.iter().map(|s| json!({"title": s, "category": "pw:api"})).collect::<Vec<_>>()});
                if let Some(error) = error {
                    let err = json!({"message": error, "location": {"file": "/w/tests/REQ-1.spec.ts", "line": 12}});
                    result["error"] = err.clone();
                    result["errors"] = json!([err]);
                }
                json!({"title": title, "file": "REQ-1.spec.ts",
                    "tests": [{"status": if *status == "passed" {"expected"} else {"unexpected"}, "results": [result]}]})
            })
            .collect();
        json!({"suites": [{"title": "REQ-1.spec.ts", "specs": specs}]})
    }

    #[test]
    fn should_count_passed_and_collect_durations() {
        let summary = summarize_report(&report(&[
            ("a", "passed", None, &[], 800),
            ("b", "failed", Some("boom"), &[], 10500),
        ]));
        assert_eq!((summary.passed, summary.total), (1, 2));
        assert_eq!(
            summary
                .results
                .iter()
                .filter(|r| !r.ok)
                .map(|r| r.title.as_str())
                .collect::<Vec<_>>(),
            ["b"]
        );
        assert_eq!(summary.slow(3000), ["b"]);
        let empty = summarize_report(&json!({}));
        assert_eq!((empty.passed, empty.total), (0, 0));
    }

    #[test]
    fn should_build_four_field_summary_without_ansi_and_with_last_steps() {
        let msg = "\x1b[31mError: expect(locator).toHaveText(expected)\x1b[39m\n\nLocator: getByTestId('count')\nExpected string: \"2\"\nReceived string: \"1\"";
        let steps = [
            "page.goto(/)",
            "locator.click",
            "locator.click",
            "expect.toHaveText",
        ];
        let summary = summarize_report(&report(&[(
            "REQ-1: increments",
            "failed",
            Some(msg),
            &steps,
            5000,
        )]));
        let text = failure_summaries(&summary, 3, 900, 10000);
        assert!(text.contains("Feature: REQ-1: increments"));
        assert!(text.contains("Failed at: REQ-1.spec.ts:12"));
        assert!(text.contains("Observation: Error: expect(locator).toHaveText(expected)"));
        assert!(!text.contains('\x1b'));
        assert!(text.contains("Steps: locator.click -> locator.click -> expect.toHaveText"));
    }

    #[test]
    fn should_use_call_log_lines_and_flag_timeouts() {
        let msg = "Error: page.goto: net::ERR_CONNECTION_REFUSED\nCall log:\n  - navigating to \"http://x/\", waiting until \"load\"\n\nmore";
        let summary = summarize_report(&report(&[("t", "failed", Some(msg), &[], 100)]));
        assert!(
            failure_summaries(&summary, 8, 900, 10000)
                .contains("Steps: navigating to \"http://x/\", waiting until \"load\"")
        );
        let slow = summarize_report(&report(&[(
            "slow one",
            "timedOut",
            Some("Test timeout of 10000ms exceeded."),
            &["page.reload"],
            10000,
        )]));
        let text = failure_summaries(&slow, 8, 900, 10000);
        assert!(text.to_lowercase().contains("timed out"));
        assert!(text.contains("kills a test at 10 s"));
    }

    #[test]
    fn should_group_failed_tests_by_owning_node_via_spec_basename() {
        let summary = summarize_report(&json!({"suites": [
            {"title": "a", "file": "REQ-1.spec.ts", "specs": [
                {"title": "one", "file": "REQ-1.spec.ts", "tests": [{"status": "unexpected", "results": [{"status": "failed", "duration": 1,
                    "error": {"message": "x", "location": {"file": "/w/tests/REQ-1.spec.ts", "line": 3}}}]}]}]},
            {"title": "b", "file": "sub/REQ-2.spec.ts", "specs": [
                {"title": "two", "file": "sub/REQ-2.spec.ts", "tests": [{"status": "expected", "results": [{"status": "passed", "duration": 1}]}]},
                {"title": "three", "file": "sub/REQ-2.spec.ts", "tests": [{"status": "unexpected", "results": [{"status": "timedOut", "duration": 1}]}]}]}]}));
        let map = map_specs_to_nodes(
            &strings(&["REQ-1.spec.ts", "sub/REQ-2.spec.ts"]),
            &strings(&["REQ-1", "REQ-2"]),
        );
        let grouped = nodes_for_failures(&summary.results, &map);
        let titles = |node: &str| -> Vec<String> {
            grouped[&Some(node.to_string())]
                .iter()
                .map(|r| r.title.clone())
                .collect()
        };
        assert_eq!(titles("REQ-1"), ["one"]);
        assert_eq!(titles("REQ-2"), ["three"]);
    }

    #[test]
    fn should_attribute_failure_raised_in_helper_to_the_spec_file() {
        let rep = json!({"suites": [{"title": "REQ-2.3.1-x.spec.ts", "file": "REQ-2.3.1-x.spec.ts", "specs": [
            {"title": "REQ-2.3.1: trash view", "file": "REQ-2.3.1-x.spec.ts", "tests": [{"status": "unexpected", "results": [
                {"status": "failed", "duration": 900, "error": {"message": "boom", "location": {"file": "/w/tests/support/e2e.ts", "line": 48}}}]}]}]}]});
        let summary = summarize_report(&rep);
        let map = map_specs_to_nodes(&strings(&["REQ-2.3.1-x.spec.ts"]), &strings(&["REQ-2.3.1"]));
        let grouped = nodes_for_failures(&summary.results, &map);
        assert_eq!(
            grouped.keys().collect::<Vec<_>>(),
            [&Some("REQ-2.3.1".to_string())]
        );
        assert!(
            failure_summaries(&summary, 8, 900, 10000)
                .contains("Failed at: e2e.ts:48 (called from REQ-2.3.1-x.spec.ts)")
        );
    }

    #[test]
    fn should_report_zero_tests_as_load_error() {
        let summary = summarize_report(
            &json!({"suites": [], "errors": [{"message": "SyntaxError: Unexpected token"}]}),
        );
        assert_eq!(summary.total, 0);
        assert_eq!(summary.load_errors, ["SyntaxError: Unexpected token"]);
    }

    #[test]
    fn should_scale_workers_to_container_memory() {
        assert_eq!(workers_for_memory(None, 4, 700), 4);
        assert_eq!(workers_for_memory(Some(512 * 1024 * 1024), 4, 700), 1);
        assert_eq!(workers_for_memory(Some(2 * 1024 * 1024 * 1024), 4, 700), 2);
        assert_eq!(workers_for_memory(Some(8 * 1024 * 1024 * 1024), 4, 700), 4);
    }

    #[test]
    fn should_pin_version_from_tests_package_lock_or_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let tests = dir.path().join("tests");
        std::fs::create_dir_all(&tests).unwrap();
        assert_eq!(playwright_version_hint(Some(&tests), "1.63.0"), "1.63.0");
        std::fs::write(
            dir.path().join("package-lock.json"),
            "{\"packages\": {\"node_modules/@playwright/test\": {\"version\": \"1.55.1\"}}}",
        )
        .unwrap();
        assert_eq!(playwright_version_hint(Some(&tests), "1.63.0"), "1.55.1");
        std::fs::write(
            tests.join("package.json"),
            "{\"devDependencies\": {\"@playwright/test\": \"^1.52.0\"}}",
        )
        .unwrap();
        assert_eq!(playwright_version_hint(Some(&tests), "1.63.0"), "1.52.0");
    }

    #[test]
    fn should_keep_every_write_inside_the_private_root() {
        let env = isolated_install_env(Path::new("/private/x"));
        for key in [
            "npm_config_cache",
            "NPM_CONFIG_CACHE",
            "PLAYWRIGHT_BROWSERS_PATH",
        ] {
            assert!(
                envs::lookup(&env, key)
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("/private/x"),
                "{key}"
            );
        }
        assert!(
            envs::lookup(&env, "PLAYWRIGHT_DOWNLOAD_HOST")
                .unwrap()
                .to_string_lossy()
                .contains("npmmirror")
        );
    }

    #[test]
    fn should_find_spec_ports_and_list_specs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("support")).unwrap();
        std::fs::write(dir.path().join("REQ-1.spec.ts"), "goto('/')").unwrap();
        std::fs::write(
            dir.path().join("support/e2e.ts"),
            "process.env.E2E_BASE_URL ?? 'http://127.0.0.1:3301'",
        )
        .unwrap();
        assert_eq!(spec_base_ports(dir.path()), [3301]);
        assert_eq!(list_specs(dir.path()), ["REQ-1.spec.ts"]);
        assert_eq!(support_files(dir.path()), ["support/e2e.ts"]);
    }

    #[test]
    fn should_retry_initial_connection_close_but_not_later_failures() {
        use std::net::TcpListener;
        for fail_at in [0, 1] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            std::thread::spawn(move || {
                for (i, stream) in listener
                    .incoming()
                    .take(if fail_at == 0 { 4 } else { 2 })
                    .enumerate()
                {
                    let mut stream = stream.unwrap();
                    let mut buf = [0u8; 1024];
                    let _ = stream.read(&mut buf);
                    if i != fail_at {
                        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                }
            });
            assert_eq!(
                robustness_probe(port, None, Duration::from_secs(2)).is_none(),
                fail_at == 0
            );
        }
    }

    #[test]
    fn should_pass_probe_for_a_server_that_answers_and_fail_for_a_dead_port() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(3) {
                let mut stream = stream.unwrap();
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
        });
        assert!(robustness_probe(port, None, Duration::from_secs(3)).is_none());
        let free = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let error = robustness_probe(free, None, Duration::from_secs(2)).unwrap();
        assert!(error.contains("no HTTP response"));
    }
}

#[cfg(test)]
mod action_error_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn should_preserve_verdict_when_adding_caught_action_errors() {
        let mut report = json!({"action_errors":{"case":["Click: overlay intercepts pointer events"]},
            "suites":[{"specs":[{"id":"case","title":"flow","tests":[{"status":"unexpected",
                "results":[{"status":"failed","error":{"message":"missing item"}}]}]}]}]});
        let summary = summarize_report(&report);
        assert_eq!((summary.passed, summary.total), (0, 1));
        let message = failure_summaries(&summary, 8, 900, 10000);
        assert!(message.contains("overlay intercepts pointer events"));
        assert!(message.contains("may have recovered"));
        report["suites"][0]["specs"][0]["tests"][0]["status"] = json!("expected");
        let summary = summarize_report(&report);
        assert_eq!((summary.passed, summary.total), (1, 1));
        assert!(failure_summaries(&summary, 8, 900, 10000).is_empty());
    }
}
