//! Unified event stream. The kernel appends one JSON object per line to
//! `.arc/octos-arc-events.jsonl` and mirrors it on stdout as
//! `@@arc-event {json}` so the Python glue can translate events into ARC's
//! runner-events / traceability records while the run is in progress. Plain
//! log lines go to stdout too; the glue relays them.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::Instant;

use eyre::Result;
use serde_json::{Map, Value, json};

pub const EVENT_PREFIX: &str = "@@arc-event ";

pub struct Events {
    file: File,
    started: Instant,
    quiet: bool,
}

impl Events {
    pub fn open(arc_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(arc_dir)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(arc_dir.join("octos-arc-events.jsonl"))?;
        Ok(Self {
            file,
            started: Instant::now(),
            quiet: false,
        })
    }

    /// Do not echo to stdout (tests).
    pub fn quiet(mut self) -> Self {
        self.quiet = true;
        self
    }

    pub fn emit(&mut self, kind: &str, fields: Value) {
        let mut object: Map<String, Value> = Map::new();
        object.insert(
            "ts".into(),
            json!(chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()),
        );
        object.insert("elapsed_s".into(), json!(self.started.elapsed().as_secs()));
        object.insert("event".into(), json!(kind));
        if let Value::Object(extra) = fields {
            for (key, value) in extra {
                object.insert(key, value);
            }
        }
        let line = Value::Object(object).to_string();
        let _ = writeln!(self.file, "{line}");
        let _ = self.file.flush();
        if !self.quiet {
            println!("{EVENT_PREFIX}{line}");
        }
    }

    /// Human-readable progress line (relayed by the glue to stdout and stderr).
    pub fn log(&mut self, line: impl AsRef<str>) {
        let line = line.as_ref();
        self.emit("log", json!({"line": line}));
        if !self.quiet {
            println!("{line}");
        }
    }

    /// ARC requirement lifecycle: phase design|implement|test,
    /// status running|completed|failed|passed. `aliases` are spec-side ids the
    /// glue mirrors the state onto.
    pub fn requirement_state(
        &mut self,
        node_id: &str,
        phase: &str,
        status: &str,
        message: Option<&str>,
        aliases: &[String],
    ) {
        self.emit(
            "requirement_state",
            json!({"node_id": node_id, "phase": phase, "status": status, "message": message, "aliases": aliases}),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_append_events_with_kind_and_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut events = Events::open(dir.path()).unwrap().quiet();
        events.requirement_state("REQ-1", "test", "passed", Some("ok"), &["REQ-1.1".into()]);
        events.log("[flow] hello");
        let text = std::fs::read_to_string(dir.path().join("octos-arc-events.jsonl")).unwrap();
        let lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines[0]["event"], "requirement_state");
        assert_eq!(lines[0]["aliases"][0], "REQ-1.1");
        assert_eq!(lines[1]["event"], "log");
        assert_eq!(lines[1]["line"], "[flow] hello");
    }
}
