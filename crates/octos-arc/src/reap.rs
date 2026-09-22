//! Process hygiene around the run (`main._reap_stray_processes`,
//! `main._port_watchdog`): browsers, servers and runtimes the agent left
//! behind eat the memory the grader's Playwright needs (cloud 0764e8d77c54 /
//! e60fb3545eae: four workers SIGKILLed one second after spawning), and a
//! process of ours bound to the grading port ends the run early.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::acceptance::{kill_pid, lsof_pids, process_cwd};

/// Command-line fragments that mark a process as ours to reap at the end of
/// the run (the Python list, verbatim).
const STRAY_MARKERS: &[&str] = &[
    "chrom",
    "headless_shell",
    "playwright",
    "octos serve",
    "node ",
    "npm ",
    "/node",
];

/// Command names of per-node workspace leftovers (`acceptance.REAP_COMMANDS`):
/// a server or build the turn started from inside frontend/ or backend/.
const REAP_COMMANDS: &[&str] = &["node", "npm", "npx", "sh", "bash"];

/// `acceptance.should_reap`: a leftover server/build process from a tool turn —
/// a node/npm process whose cwd is inside the app (frontend/ or backend/), never
/// the kernel or the harness.
pub fn should_reap(comm: &str, cwd: Option<&str>, root: &Path) -> bool {
    let Some(cwd) = cwd.filter(|c| !c.is_empty()) else {
        return false;
    };
    let name = Path::new(comm)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !REAP_COMMANDS.contains(&name.as_str()) {
        return false;
    }
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| Path::new(cwd).to_path_buf());
    let Ok(rel) = cwd.strip_prefix(&root) else {
        return false;
    };
    matches!(
        rel.components().next().map(|c| c.as_os_str().to_string_lossy().into_owned()),
        Some(first) if first == "frontend" || first == "backend"
    )
}

fn run(program: &str, args: &[&str]) -> String {
    match Command::new(program).args(args).output() {
        Ok(output) => String::from_utf8_lossy(&output.stdout).into_owned(),
        Err(error) => format!("<{program} unavailable: {error}>"),
    }
}

/// `ps` rows (pid, ppid, rss KiB, etime, args) sorted by RSS, Linux or macOS syntax.
fn ps_rows() -> Vec<String> {
    let linux = run("ps", &["-eo", "pid,ppid,rss,etime,args", "--sort=-rss"]);
    let text = if linux.starts_with('<') || linux.trim().is_empty() || linux.contains("illegal") {
        run("ps", &["-eo", "pid,ppid,rss,etime,args", "-r"])
    } else {
        linux
    };
    text.lines().map(str::to_string).collect()
}

/// One parsed `ps` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcRow {
    pub pid: u32,
    pub ppid: u32,
    pub args: String,
}

pub fn parse_ps(lines: &[String]) -> Vec<ProcRow> {
    lines
        .iter()
        .skip(1)
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?.parse().ok()?;
            let ppid = parts.next()?.parse().ok()?;
            let _rss = parts.next()?;
            let _etime = parts.next()?;
            let args = parts.collect::<Vec<_>>().join(" ");
            Some(ProcRow { pid, ppid, args })
        })
        .collect()
}

/// Processes to kill at the end of the run: anything matching the stray
/// markers except ourselves and our parent (the platform runner), and only
/// when `owned` attributes the process to this run (working directory or
/// command line inside the workspace, or a descendant of ours) — a shared host
/// keeps its own browsers and servers.
pub fn select_victims(
    rows: &[ProcRow],
    me: u32,
    parent: u32,
    owned: impl Fn(&ProcRow) -> bool,
) -> Vec<u32> {
    let descendants = descendants_of(rows, me);
    rows.iter()
        .filter(|row| row.pid != me && row.pid != parent)
        .filter(|row| {
            let low = row.args.to_lowercase();
            STRAY_MARKERS.iter().any(|m| low.contains(m))
        })
        .filter(|row| descendants.contains(&row.pid) || owned(row))
        .map(|row| row.pid)
        .collect()
}

/// Transitive children of `root` in the process table.
pub fn descendants_of(rows: &[ProcRow], root: u32) -> Vec<u32> {
    let mut out = vec![root];
    let mut index = 0;
    while index < out.len() {
        let parent = out[index];
        for row in rows {
            if row.ppid == parent && !out.contains(&row.pid) {
                out.push(row.pid);
            }
        }
        index += 1;
    }
    out.retain(|pid| *pid != root);
    out
}

/// Report lines (memory, cgroup, top processes) — the diagnostics the cloud
/// post-mortems needed.
pub fn report() -> Vec<String> {
    let mut out = Vec::new();
    let free = run("free", &["-m"]);
    if !free.starts_with('<') {
        out.push(format!("memory:\n{}", free.trim_end()));
    }
    let mut cg = Vec::new();
    for file in [
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory.current",
        "/sys/fs/cgroup/memory.peak",
        "/sys/fs/cgroup/memory.events",
        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
        "/sys/fs/cgroup/memory/memory.max_usage_in_bytes",
        "/sys/fs/cgroup/memory/memory.failcnt",
        "/sys/fs/cgroup/pids.max",
    ] {
        if let Ok(text) = std::fs::read_to_string(file) {
            cg.push(format!("{file}={}", text.trim().replace('\n', " ")));
        }
    }
    out.push(format!(
        "cgroup: {}",
        if cg.is_empty() {
            "<no cgroup files>".to_string()
        } else {
            cg.join("; ")
        }
    ));
    let rows = ps_rows();
    out.push(format!(
        "top processes by RSS:\n{}",
        rows.iter().take(20).cloned().collect::<Vec<_>>().join("\n")
    ));
    out
}

/// Attribute a working directory by path components, never textual prefixes.
fn cwd_owned(cwd: &str, root: &Path) -> bool {
    let cwd = Path::new(cwd);
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    cwd.is_absolute()
        && !cwd
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
        && cwd.starts_with(root)
}

/// End-of-run sweep: stop matching descendants or workspace processes.
pub fn sweep_all(root: &Path) -> Vec<u32> {
    let rows = parse_ps(&ps_rows());
    let me = std::process::id();
    let parent = parent_pid().unwrap_or(0);
    let victims = select_victims(&rows, me, parent, |row| {
        cwd_owned(&process_cwd(row.pid), root)
    });
    for pid in &victims {
        kill_pid(*pid);
    }
    victims
}

/// Per-node sweep (`acceptance.reap_workspace_processes`): kill node/npm
/// processes left running inside the app directories (cloud 29c840566f36: 346
/// strays at postflight; on a 1-CPU grader they starve the acceptance runs).
pub fn sweep_workspace(root: &Path) -> Vec<u32> {
    let me = std::process::id();
    let mut killed = Vec::new();
    for line in run("ps", &["-axo", "pid=,comm="]).lines() {
        let mut parts = line.trim().splitn(2, char::is_whitespace);
        let Some(pid) = parts.next().and_then(|p| p.parse::<u32>().ok()) else {
            continue;
        };
        let comm = parts.next().unwrap_or("").trim();
        if pid == me {
            continue;
        }
        let name = Path::new(comm)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !REAP_COMMANDS.contains(&name.as_str()) {
            continue;
        }
        let cwd = process_cwd(pid);
        if should_reap(comm, Some(&cwd), root) {
            kill_pid(pid);
            killed.push(pid);
        }
    }
    killed
}

fn parent_pid() -> Option<u32> {
    #[cfg(unix)]
    {
        // SAFETY-free: read our own ppid from ps rather than libc.
        let text = run(
            "ps",
            &["-o", "ppid=", "-p", &std::process::id().to_string()],
        );
        return text.trim().parse().ok();
    }
    #[allow(unreachable_code)]
    None
}

/// Kills OUR processes that bind the grading port during generation (the
/// runner terminates a run that serves the grading port early); foreign
/// listeners are left alone and reported once.
pub struct PortWatchdog {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl PortWatchdog {
    pub fn start(port: u16, root: &Path, interval: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let root = root.to_string_lossy().into_owned();
        let handle = std::thread::spawn(move || {
            let mut reported: Vec<u32> = Vec::new();
            while !flag.load(Ordering::Acquire) {
                for pid in lsof_pids(port) {
                    let cwd = process_cwd(pid);
                    if cwd_owned(&cwd, Path::new(&root)) {
                        println!(
                            "[watchdog] port {port} bound by our process {pid} (cwd={cwd}); killing"
                        );
                        kill_pid(pid);
                    } else if !reported.contains(&pid) {
                        println!(
                            "[watchdog] port {port} held by foreign process {pid} (cwd={}); leaving it",
                            if cwd.is_empty() { "?" } else { &cwd }
                        );
                        reported.push(pid);
                    }
                }
                let mut waited = Duration::ZERO;
                while waited < interval && !flag.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(200));
                    waited += Duration::from_millis(200);
                }
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for PortWatchdog {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<ProcRow> {
        parse_ps(&[
            "  PID  PPID   RSS     ELAPSED ARGS".to_string(),
            "  100     1 90000    01:00:00 /opt/pw/chromium/headless_shell --headless".to_string(),
            "  101   100 20000       00:10 node server.js".to_string(),
            "  102     1 15000       00:05 octos serve --stdio --solo".to_string(),
            "  103     1 10000       00:05 python3 main.py".to_string(),
            "  104   103  9000       00:05 npm run build".to_string(),
            "  105     1  5000       00:05 /usr/bin/node /x/playwright test".to_string(),
        ])
    }

    #[test]
    fn should_match_workspace_directories_by_path_components() {
        let root = Path::new("/work/app");
        assert!(cwd_owned("/work/app", root));
        assert!(cwd_owned("/work/app/backend", root));
        assert!(!cwd_owned("/work/app-other", root));
        assert!(!cwd_owned("", root));
        assert!(!cwd_owned("/work/app/../foreign", root));
    }

    #[test]
    fn should_parse_ps_rows_and_select_the_python_marker_set() {
        let rows = rows();
        assert_eq!(rows.len(), 6);
        assert_eq!(rows[1].args, "node server.js");
        // 103 is "me": its descendant 104 is ours; everything else needs attribution.
        assert_eq!(select_victims(&rows, 103, 1, |_| false), vec![104]);
        assert_eq!(
            select_victims(&rows, 103, 1, |row| row.args.contains("server.js")),
            vec![101, 104]
        );
        // Our own process is never a victim even when it matches; the parent neither.
        assert_eq!(
            select_victims(&rows, 102, 100, |_| true),
            vec![101, 104, 105]
        );
        assert_eq!(descendants_of(&rows, 100), vec![101]);
    }

    #[test]
    fn should_reap_only_app_processes_inside_frontend_or_backend() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("backend")).unwrap();
        std::fs::create_dir_all(root.join(".arc/codegen")).unwrap();
        let backend = root.join("backend").to_string_lossy().into_owned();
        assert!(should_reap("node", Some(&backend), root));
        assert!(should_reap("/usr/bin/npm", Some(&backend), root));
        assert!(
            !should_reap("python3", Some(&backend), root),
            "not a build/server command"
        );
        assert!(
            !should_reap(
                "node",
                Some(&root.join(".arc/codegen").to_string_lossy()),
                root
            ),
            "harness dirs are not app dirs"
        );
        assert!(
            !should_reap("node", Some(&root.to_string_lossy()), root),
            "the workspace root itself is the kernel's cwd"
        );
        assert!(!should_reap("node", Some("/tmp"), root));
        assert!(!should_reap("node", None, root));
    }

    #[test]
    fn should_stop_the_watchdog_promptly() {
        let dir = tempfile::tempdir().unwrap();
        let mut dog = PortWatchdog::start(1, dir.path(), Duration::from_secs(60));
        let started = std::time::Instant::now();
        dog.stop();
        assert!(started.elapsed() < Duration::from_secs(20));
    }
}
