"""Local acceptance testing for the ARC adapter.

Runs the public (or container-provided) Playwright specs that belong to one
requirement node against the generated app, and turns failures into the
compact four-field summary (Feature / Failed at / Observation / Steps) fed back
to the model. Spec discovery, spec->node mapping and report parsing are pure
functions covered by arc/tests/test_acceptance.py; process handling lives in
`AcceptanceRunner`.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import signal
import socket
import subprocess
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable

_ANSI = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
_SPEC_ID = re.compile(r"^(REQ-\d+(?:\.\d+)*)(?=[.\-_ ]|$)")


def spec_node_id(rel_path: str) -> str | None:
    """`REQ-1.2-user-login.spec.ts` -> `REQ-1.2`; non-spec files -> None."""
    name = Path(rel_path).name
    if not name.endswith(".spec.ts"):
        return None
    m = _SPEC_ID.match(name)
    return m.group(1) if m else None


def _version_key(req_id: str) -> tuple:
    return tuple(int(p) for p in re.findall(r"\d+", req_id))


def map_specs_to_nodes(spec_paths: list[str], node_ids: list[str]) -> tuple[dict, dict]:
    """Assign spec files to requirement nodes.

    Returns (mapping, aliases): mapping[node_id] -> [spec paths] with the key
    None holding specs that belong to no single node (regression set);
    aliases[spec_id] -> node_id for spec ids that are not literal node ids.

    Order of preference: literal id match; when the remaining distinct spec
    ids and the remaining nodes have the same count, pair them in numeric /
    document order; otherwise attach `REQ-1.x` to an existing `REQ-1` parent.
    """
    mapping: dict = {nid: [] for nid in node_ids}
    mapping[None] = []
    aliases: dict[str, str] = {}
    by_spec_id: dict[str, list[str]] = {}
    for path in spec_paths:
        sid = spec_node_id(path)
        if sid is None:
            continue
        by_spec_id.setdefault(sid, []).append(path)
    unmatched_ids = []
    for sid in sorted(by_spec_id, key=_version_key):
        if sid in mapping:
            mapping[sid].extend(by_spec_id[sid])
        else:
            unmatched_ids.append(sid)
    free_nodes = [nid for nid in node_ids if not mapping[nid]]
    if unmatched_ids and len(unmatched_ids) == len(free_nodes):
        for sid, nid in zip(unmatched_ids, free_nodes):
            mapping[nid].extend(by_spec_id[sid])
            aliases[sid] = nid
        return mapping, aliases
    for sid in unmatched_ids:
        parent = sid
        target = None
        while "." in parent:
            parent = parent.rsplit(".", 1)[0]
            if parent in mapping:
                target = parent
                break
        if target is None:
            mapping[None].extend(by_spec_id[sid])
        else:
            mapping[target].extend(by_spec_id[sid])
            aliases[sid] = target
    return mapping, aliases


@dataclass
class TestOutcome:
    title: str
    ok: bool
    status: str
    duration_ms: int
    file: str = ""          # the SPEC file the test lives in (node ownership)
    line: int | None = None
    location: str = ""      # where the error was raised (may be a helper file)
    message: str = ""
    steps: list[str] = field(default_factory=list)


@dataclass
class RunSummary:
    passed: int = 0
    total: int = 0
    results: list[TestOutcome] = field(default_factory=list)
    stdout_tail: str = ""
    error: str | None = None  # infrastructure error (no report)
    killed: bool = False      # the test runner itself was killed (OOM); not a verdict
    load_errors: list[str] = field(default_factory=list)  # Playwright top-level errors

    def slow(self, threshold_ms: int) -> list[str]:
        return [r.title for r in self.results if r.duration_ms >= threshold_ms]

    @property
    def all_passed(self) -> bool:
        return self.total > 0 and self.passed == self.total


def summarize_report(report: dict) -> RunSummary:
    """Collapse a Playwright JSON report into per-test outcomes."""
    summary = RunSummary()

    def walk(suites, parent_file=""):
        for suite in suites or []:
            file = suite.get("file") or parent_file
            for spec in suite.get("specs", []):
                tests = spec.get("tests", [])
                results = [r for t in tests for r in t.get("results", [])]
                last = results[-1] if results else {}
                ok = bool(tests) and all(t.get("status") == "expected" or t.get("ok") for t in tests)
                err = last.get("error") or (last.get("errors") or [{}])[0] or {}
                loc = err.get("location") or {}
                steps = [s.get("title", "") for s in last.get("steps", []) if s.get("title")]
                loc_file = Path(loc.get("file") or "").name
                # keep-local-4: failures raised inside support/e2e.ts were
                # attributed to the helper, so no node owned them.
                summary.results.append(TestOutcome(
                    title=spec.get("title", "?"), ok=ok, status=last.get("status", "unknown"),
                    duration_ms=int(sum(r.get("duration", 0) for r in results)),
                    file=Path(spec.get("file") or file or loc_file or "").name,
                    line=loc.get("line"),
                    location=f"{loc_file}:{loc.get('line')}" if loc_file and loc.get("line") else loc_file,
                    message=_ANSI.sub("", str(err.get("message") or "")),
                    steps=steps))
            walk(suite.get("suites", []), file)

    walk(report.get("suites", []))
    summary.load_errors = [_ANSI.sub("", str(e.get("message") or e))[:600] for e in report.get("errors") or []]
    summary.total = len(summary.results)
    summary.passed = sum(1 for r in summary.results if r.ok)
    return summary


def nodes_for_failures(results: list[TestOutcome], spec_map: dict) -> dict[str, list[TestOutcome]]:
    """Group failed outcomes by the requirement node that owns their spec file
    (matched on the spec file's basename); unmapped files land under None."""
    owner: dict[str, object] = {}
    for node_id, paths in spec_map.items():
        for path in paths or []:
            owner[Path(path).name] = node_id
    grouped: dict = {}
    for r in results:
        if r.ok:
            continue
        grouped.setdefault(owner.get(Path(r.file or "").name), []).append(r)
    return grouped


def _call_log_steps(message: str) -> list[str]:
    """Playwright's `Call log:` lines are the closest thing to a step trace
    when the spec has no test.step() blocks."""
    steps = []
    seen = False
    for line in message.splitlines():
        if line.strip().lower().startswith("call log"):
            seen = True
            continue
        if seen:
            stripped = line.strip().lstrip("-").strip()
            if not stripped:
                break
            steps.append(stripped[:120])
    return steps


def failure_summaries(summary: RunSummary, max_steps: int = 8, max_observation: int = 900) -> str:
    """Four-field digest of every failed test — the only thing the model sees."""
    blocks = []
    for r in summary.results:
        if r.ok:
            continue
        observation = r.message.strip() or f"status {r.status}"
        if r.status == "timedOut" or "timeout" in observation.lower()[:120]:
            observation = (f"TIMED OUT after {r.duration_ms} ms (the grader kills a test at 10 s; the "
                           f"page or a request never settled). " + observation)
        observation = observation[:max_observation]
        where = r.location or r.file or "?"
        if r.location and r.file and not r.location.startswith(r.file):
            where = f"{r.location} (called from {r.file})"
        steps_src = r.steps or _call_log_steps(r.message)
        steps = " -> ".join(steps_src[-max_steps:]) if steps_src else "(no step trace)"
        blocks.append(f"- Feature: {r.title}\n  Failed at: {where}\n  Observation: {observation}\n  Steps: {steps}")
    return "\n".join(blocks)


# ---------------------------------------------------------------- processes

def find_playwright_root(candidates: list[Path]) -> Path | None:
    """First directory that has @playwright/test installed."""
    for cand in candidates:
        if cand and (cand / "node_modules" / "@playwright" / "test").is_dir():
            return cand.resolve()
    return None


def acceptance_work_dir(root: Path) -> Path:
    """A writable scratch dir under the Playwright root (module resolution),
    falling back to the system temp dir."""
    preferred = root / ".octos-acceptance" / f"run-{os.getpid()}"
    try:
        preferred.mkdir(parents=True, exist_ok=True)
        return preferred
    except OSError:
        return Path(tempfile.mkdtemp(prefix="octos-acceptance-")) / "run"


def playwright_candidates(bundle_dir: Path, tests_dir: Path | None, output_dir: Path) -> list[Path]:
    """Places an existing @playwright/test may live, most specific first. The
    runner image ships one ("Using preinstalled Playwright package from runner
    image"); finding it matters because installing our own must never change
    what the platform's own `npx playwright test` resolves later."""
    cands: list[Path] = []
    env_root = os.environ.get("OCTOS_ARC_PLAYWRIGHT_ROOT")
    if env_root:
        cands.append(Path(env_root))
    cands.append(bundle_dir / "local-grader")
    if tests_dir:
        cands.extend([tests_dir, *tests_dir.parents][:4])
    cands.extend([output_dir, Path("/workspace"), Path("/workspace/tests"), Path("/app"), Path("/runner"),
                  Path("/opt/playwright"), Path("/ms-playwright"), Path.home()])
    try:  # global npm root: /usr/local/lib/node_modules -> parent holds node_modules/
        root = subprocess.run(["npm", "root", "-g"], capture_output=True, text=True, timeout=20).stdout.strip()
        if root:
            cands.append(Path(root).parent)
    except (OSError, subprocess.TimeoutExpired):
        pass
    cands.extend([Path("/usr/local/lib"), Path("/usr/lib"), Path("/usr/local"), Path("/opt")])
    return cands


def find_playwright_by_search(log: Callable[[str], None], max_depth: int = 6, timeout: int = 25) -> Path | None:
    """Bounded filesystem search for a preinstalled @playwright/test package."""
    cmd = ["find", "/", "-maxdepth", str(max_depth), "-type", "d", "-path", "*/node_modules/@playwright/test",
           "-not", "-path", "/proc/*", "-not", "-path", "/sys/*", "-not", "-path", "/tmp/*"]
    try:
        out = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout).stdout.split()
    except (OSError, subprocess.TimeoutExpired):
        return None
    for hit in sorted(out, key=len):
        root = Path(hit).parents[2]  # <root>/node_modules/@playwright/test
        if (root / "node_modules" / "@playwright" / "test" / "package.json").is_file():
            log(f"[acceptance] found preinstalled Playwright at {root}")
            return root
    return None


def playwright_version_hint(tests_dir: Path | None, fallback: str = "1.63.0") -> str:
    """Version to install when we must: the one the tests declare, else a pin.
    Never `latest` — an unpinned install is what broke cloud grading."""
    for base in ([tests_dir, *tests_dir.parents][:3] if tests_dir else []):
        for name in ("package-lock.json", "package.json"):
            path = base / name
            try:
                data = json.loads(path.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError):
                continue
            pkg = (data.get("packages") or {}).get("node_modules/@playwright/test") or {}
            version = pkg.get("version")
            if not version:
                for section in ("devDependencies", "dependencies"):
                    version = (data.get(section) or {}).get("@playwright/test")
                    if version:
                        break
            if version:
                return str(version).lstrip("^~=v")
    return fallback


def isolated_install_env(private_root: Path) -> dict:
    """Environment for a self-contained Playwright install: private npm cache
    and browser directory, mirrors for the runner network. Nothing outside
    `private_root` is written, so the platform's own Playwright is untouched."""
    env = dict(os.environ)
    env.update(
        npm_config_registry="https://registry.npmmirror.com",
        npm_config_cache=str(private_root / "npm-cache"),
        NPM_CONFIG_CACHE=str(private_root / "npm-cache"),
        npm_config_update_notifier="false",
        PLAYWRIGHT_DOWNLOAD_HOST="https://npmmirror.com/mirrors/playwright",
        PLAYWRIGHT_BROWSERS_PATH=str(private_root / "browsers"),
    )
    return env


def ensure_playwright(install_root: Path, log: Callable[[str], None], timeout: int = 540,
                      version: str = "1.63.0") -> tuple[Path, dict] | None:
    """Self-contained install of @playwright/test@<version> + chromium under
    `install_root`. Returns (root, env_extra) — env_extra must be passed to
    every run that uses this install (private PLAYWRIGHT_BROWSERS_PATH)."""
    install_root.mkdir(parents=True, exist_ok=True)
    env = isolated_install_env(install_root)
    (install_root / "package.json").write_text(json.dumps({"name": "octos-arc-acceptance", "private": True}))
    t0 = time.time()
    steps = [
        ["npm", "install", "--no-audit", "--no-fund", "--no-package-lock", f"@playwright/test@{version}"],
        [str(install_root / "node_modules" / ".bin" / "playwright"), "install", "chromium"],
    ]
    for cmd in steps:
        remaining = timeout - (time.time() - t0)
        if remaining <= 0:
            log("[acceptance] playwright install timed out")
            return None
        try:
            r = subprocess.run(cmd, cwd=install_root, env=env, capture_output=True, text=True, timeout=remaining)
        except (subprocess.TimeoutExpired, OSError) as exc:
            log(f"[acceptance] playwright install step {Path(cmd[0]).name} {cmd[1]} failed: {exc}")
            return None
        if r.returncode != 0:
            log(f"[acceptance] playwright install step {Path(cmd[0]).name} {cmd[1]} rc={r.returncode}: "
                f"{(r.stderr or r.stdout)[-300:]}")
            return None
    root = find_playwright_root([install_root])
    if root is None:
        return None
    log(f"[acceptance] playwright {version} installed privately into {install_root} in {time.time()-t0:.0f}s "
        f"(browsers under {env['PLAYWRIGHT_BROWSERS_PATH']})")
    return root, {"PLAYWRIGHT_BROWSERS_PATH": env["PLAYWRIGHT_BROWSERS_PATH"]}


def free_port(port: int) -> None:
    try:
        pids = subprocess.run(["lsof", "-ti", f":{port}"], capture_output=True, text=True, timeout=15).stdout.split()
    except (OSError, subprocess.TimeoutExpired):
        return
    for pid in pids:
        try:
            os.kill(int(pid), signal.SIGKILL)
        except (ProcessLookupError, PermissionError, ValueError):
            pass


def container_memory_limit() -> int | None:
    """cgroup memory limit in bytes (v2 memory.max or v1 limit_in_bytes), None
    when unlimited/unknown. The ARC runner container has 512 MiB (cloud
    29c840566f36: memory.max=536870912, 4 Chromium workers -> OOM, rc=-9)."""
    for path in ("/sys/fs/cgroup/memory.max", "/sys/fs/cgroup/memory/memory.limit_in_bytes"):
        try:
            raw = Path(path).read_text().strip()
        except OSError:
            continue
        if raw.isdigit():
            value = int(raw)
            if value < 1 << 50:  # v1 reports a huge number when unlimited
                return value
    return None


def workers_for_memory(limit: int | None, requested: int) -> int:
    """One Chromium worker per ~700 MiB of container memory, at least 1."""
    if not limit:
        return requested
    return max(1, min(requested, limit // (700 * 1024 * 1024)))


def workers_for_final(limit: int | None, requested: int) -> int:
    """Full-suite workers: the platform grades with --workers=4; mimic it when memory
    allows (~450 MiB per Chromium worker + app), so slow tests surface before grading.
    2 GiB -> 4, 512 MiB -> 1."""
    if not limit:
        return requested
    return max(1, min(requested, limit // (450 * 1024 * 1024)))


REAP_COMMANDS = ("node", "npm", "npx", "sh", "bash")


def should_reap(comm: str, cwd: str | None, root: Path) -> bool:
    """A leftover server/build process from a tool turn: a node/npm process whose
    cwd is inside the app (frontend/ or backend/), never the kernel or the harness."""
    if not cwd or Path(comm).name not in REAP_COMMANDS:
        return False
    try:
        rel = Path(cwd).resolve().relative_to(root.resolve())
    except (ValueError, OSError):
        return False
    return bool(rel.parts) and rel.parts[0] in ("frontend", "backend")


def process_cwd(pid: int) -> str | None:
    proc = Path(f"/proc/{pid}/cwd")
    try:
        return os.readlink(proc)
    except OSError:
        pass
    try:
        out = subprocess.run(["lsof", "-a", "-p", str(pid), "-d", "cwd", "-Fn"], capture_output=True, text=True, timeout=10).stdout
    except (OSError, subprocess.TimeoutExpired):
        return None
    for line in out.splitlines():
        if line.startswith("n"):
            return line[1:]
    return None


def reap_workspace_processes(root: Path, log: Callable[[str], None]) -> int:
    """Kill node/npm processes left running inside the app directories (cloud
    29c840566f36: 346 strays at postflight; on a 1-CPU grader they starve the
    acceptance runs). Returns how many were killed."""
    try:
        out = subprocess.run(["ps", "-axo", "pid=,comm="], capture_output=True, text=True, timeout=15).stdout
    except (OSError, subprocess.TimeoutExpired):
        return 0
    killed = 0
    for line in out.splitlines():
        parts = line.split(None, 1)
        if len(parts) != 2 or not parts[0].isdigit():
            continue
        pid, comm = int(parts[0]), parts[1].strip()
        if pid == os.getpid() or Path(comm).name not in REAP_COMMANDS:
            continue
        if should_reap(comm, process_cwd(pid), root):
            try:
                os.kill(pid, signal.SIGKILL)
                killed += 1
            except (ProcessLookupError, PermissionError):
                pass
    if killed:
        log(f"[reap] killed {killed} leftover process(es) inside frontend/ or backend/")
    return killed


def free_owned_ports(ports: list[int], root: Path) -> None:
    """Kill listeners on `ports` that were started from inside `root` (our own
    leftovers), leaving foreign processes alone. Works on macOS and Linux."""
    for port in ports:
        try:
            pids = subprocess.run(["lsof", "-ti", f":{port}"], capture_output=True, text=True, timeout=15).stdout.split()
        except (OSError, subprocess.TimeoutExpired):
            continue
        for pid in pids:
            cwd = ""
            try:
                cwd = os.readlink(f"/proc/{pid}/cwd")
            except OSError:
                try:
                    out = subprocess.run(["lsof", "-a", "-p", pid, "-d", "cwd", "-Fn"], capture_output=True,
                                         text=True, timeout=15).stdout
                    cwd = next((l[1:] for l in out.splitlines() if l.startswith("n/")), "")
                except (OSError, subprocess.TimeoutExpired):
                    pass
            if cwd.startswith(str(root)):
                try:
                    os.kill(int(pid), signal.SIGKILL)
                except (ProcessLookupError, PermissionError, ValueError):
                    pass


def port_open(port: int) -> bool:
    with socket.socket() as s:
        s.settimeout(1)
        return s.connect_ex(("127.0.0.1", port)) == 0


def tree_digest(root: Path) -> dict[str, str]:
    """rel path -> sha256 for every regular file under root (node_modules skipped)."""
    import hashlib
    out: dict[str, str] = {}
    for path in sorted(root.rglob("*")):
        if path.is_file() and "node_modules" not in path.parts:
            out[str(path.relative_to(root))] = hashlib.sha256(path.read_bytes()).hexdigest()
    return out


def restore_tree(live: Path, snapshot: Path, expected: dict[str, str]) -> list[str]:
    """Make `live` match `snapshot` again: rewrite changed/deleted files, remove
    added ones. Returns the relative paths that had to be fixed."""
    fixed: list[str] = []
    current = tree_digest(live)
    for rel, digest in expected.items():
        if current.get(rel) != digest:
            dest = live / rel
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(snapshot / rel, dest)
            fixed.append(rel)
    for rel in current:
        if rel not in expected:
            try:
                (live / rel).unlink()
            except OSError:
                pass
            fixed.append(rel)
    return fixed


def snapshot_worktree(git_run: Callable[[list[str]], object]) -> None:
    """Stage everything so `restore_worktree` can undo what a test run mutates
    (persisted JSON stores, uploaded files) without losing the model's edits."""
    git_run(["add", "-A"])


def restore_worktree(git_run: Callable[[list[str]], object], parts: tuple[str, ...] = ("frontend", "backend")) -> None:
    """Return tracked files to the staged snapshot and drop files a test run
    created; ignored build outputs (node_modules, dist) are left alone."""
    git_run(["checkout", "--", "."])
    git_run(["clean", "-fdq", "-e", "node_modules", "-e", "dist", "--", *parts])


def robustness_probe(port: int, proc: subprocess.Popen | None = None, timeout: float = 5.0) -> str | None:
    """Hit paths a browser or grader will request; the server must answer
    (any status) and stay alive. Cloud f9f0026819f1: an unhandled ENOENT on
    GET /favicon.ico killed the backend and 8 of 10 tests saw ECONNREFUSED."""
    import http.client
    for path in ("/favicon.ico", "/this-path-does-not-exist", "/api/this-route-does-not-exist"):
        try:
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=timeout)
            conn.request("GET", path)
            resp = conn.getresponse()
            resp.read()
            conn.close()
        except Exception as exc:  # noqa: BLE001
            alive = proc is None or proc.poll() is None
            return (f"GET {path} got no HTTP response ({exc.__class__.__name__}); "
                    f"backend {'still running' if alive else 'CRASHED (process exited)'} — unknown paths must "
                    f"return 404, never throw")
        time.sleep(0.2)
        if proc is not None and proc.poll() is not None:
            return f"backend process exited (rc={proc.returncode}) right after GET {path} — an unhandled exception " \
                   f"in the static/API handler; missing files must return 404 and the process must never die"
    return None


class AppServer:
    """Build the frontend once and run the backend on the smoke port."""

    def __init__(self, project: Path, port: int, log: Callable[[str], None], env_extra: dict | None = None,
                 grader_like: bool = False, extra_ports: list[int] | None = None):
        """`grader_like=True` starts the backend exactly as the platform does:
        only PORT is set, so any extra spec ports (e.g. 3301) get bound too.
        Per-node runs pass False (ARC_EXTRA_PORTS=0) to stay off shared ports."""
        self.project = project
        self.port = port
        self.log = log
        self.env_extra = env_extra or {}
        self.grader_like = grader_like
        self.extra_ports = extra_ports or []
        self.proc: subprocess.Popen | None = None
        self.log_file: Path | None = None

    def _run(self, cmd: list[str], cwd: Path, timeout: int) -> tuple[int, str]:
        try:
            r = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, timeout=timeout,
                               env=dict(os.environ, **self.env_extra))
        except subprocess.TimeoutExpired:
            return 124, f"timeout after {timeout}s"
        except OSError as exc:
            return 127, str(exc)
        return r.returncode, ((r.stdout or "") + "\n" + (r.stderr or "")).strip()[-1500:]

    def build(self) -> str | None:
        frontend, backend = self.project / "frontend", self.project / "backend"
        if not (frontend / "package.json").is_file() or not (backend / "package.json").is_file():
            return "frontend/package.json or backend/package.json missing"
        for part in (frontend, backend):
            deps = {}
            try:
                deps = json.loads((part / "package.json").read_text()).get("dependencies") or {}
            except Exception:  # noqa: BLE001
                pass
            if deps and not (part / "node_modules").is_dir():
                rc, out = self._run(["npm", "install", "--no-audit", "--no-fund"], part, 600)
                if rc != 0:
                    return f"{part.name} `npm install` failed:\n{out}"
        # Cloud c17bc1b44d26: a one-line copy build failed because dist/ did not
        # exist yet. The grader builds from a fresh checkout too, so make the
        # target directory exist before every build (harmless when it does).
        try:
            (frontend / "dist").mkdir(parents=True, exist_ok=True)
        except OSError:
            pass
        rc, out = self._run(["npm", "run", "build"], frontend, 600)
        if rc != 0:
            return f"frontend `npm run build` failed:\n{out}"
        return None

    def start(self, wait_seconds: int = 45) -> str | None:
        free_port(self.port)
        if self.grader_like:
            free_owned_ports(self.extra_ports, self.project)
        self.log_file = Path(tempfile.mkstemp(prefix="octos-app-", suffix=".log")[1])
        env = dict(os.environ, PORT=str(self.port), **self.env_extra)
        env.pop("ARC_EXTRA_PORTS", None)
        if not self.grader_like:
            env["ARC_EXTRA_PORTS"] = "0"
        try:
            fh = open(self.log_file, "w")
            self.proc = subprocess.Popen(["npm", "start"], cwd=self.project / "backend", env=env,
                                         stdin=subprocess.DEVNULL, stdout=fh, stderr=subprocess.STDOUT,
                                         start_new_session=True)
        except OSError as exc:
            return f"backend `npm start` could not launch: {exc}"
        deadline = time.time() + wait_seconds
        while time.time() < deadline:
            if self.proc.poll() is not None:
                return (f"backend `npm start` exited early (rc={self.proc.returncode}):\n"
                        f"{self.log_file.read_text(errors='replace')[-1500:]}")
            if port_open(self.port):
                err = robustness_probe(self.port, self.proc)
                if not err and self.grader_like and self.extra_ports:
                    # Cloud 3f0124e82113: the specs default to :3301, the grader sets only PORT,
                    # the backend bound PORT alone -> 10x ERR_CONNECTION_REFUSED. Same handler on both.
                    err = self.extra_ports_bound()
                if err:
                    tail = self.tail(800)
                    self.stop()
                    return f"{err}\nserver log tail:\n{tail}"
                return None
            time.sleep(0.5)
        self.stop()
        return f"backend did not bind port {self.port} within {wait_seconds}s:\n" + \
            (self.log_file.read_text(errors="replace")[-1500:] if self.log_file else "")

    def extra_ports_bound(self, wait_seconds: float = 5.0) -> str | None:
        """Grader-like start: every port the specs default to must answer too."""
        deadline = time.time() + wait_seconds
        missing = list(self.extra_ports)
        while missing and time.time() < deadline:
            missing = [p for p in missing if not port_open(p)]
            if missing:
                time.sleep(0.25)
        if not missing:
            return None
        ports = ", ".join(map(str, missing))
        return (f"PORT CONTRACT violated: the acceptance specs default to http://127.0.0.1:{ports} and the grader "
                f"starts the backend with only PORT={self.port}; the backend bound PORT but not port(s) {ports} "
                f"(every test would fail with ERR_CONNECTION_REFUSED). Serve the same handler on each of these "
                f"ports with a separate http.createServer(handler).listen(port) unless process.env.ARC_EXTRA_PORTS === '0'.")

    def tail(self, n: int = 1500) -> str:
        try:
            return self.log_file.read_text(errors="replace")[-n:] if self.log_file else ""
        except OSError:
            return ""

    def stop(self) -> None:
        if self.proc is not None:
            try:
                os.killpg(os.getpgid(self.proc.pid), signal.SIGKILL)
            except (ProcessLookupError, PermissionError, OSError):
                pass
            self.proc = None
        free_port(self.port)
        if self.grader_like:
            free_owned_ports(self.extra_ports, self.project)
        if self.log_file:
            try:
                self.log_file.unlink()
            except OSError:
                pass
            self.log_file = None


class AcceptanceRunner:
    """Run selected spec files from `tests_dir` with the Playwright install at `root`."""

    def __init__(self, root: Path, tests_dir: Path, work_dir: Path, log: Callable[[str], None],
                 timeout_ms: int = 10000, workers: int = 2, env_extra: dict | None = None):
        self.root = root
        self.env_extra = env_extra or {}
        self.tests_dir = tests_dir
        self.work_dir = work_dir
        self.log = log
        self.timeout_ms = timeout_ms
        self.workers = workers

    def _prepare(self, workers: int | None = None) -> Path:
        # Specs `import '@playwright/test'`; Node resolves that upward from the
        # spec file, so the copied tree must sit under the Playwright install
        # (NODE_PATH is set as well for the case where it cannot).
        if self.work_dir.exists():
            shutil.rmtree(self.work_dir)
        shutil.copytree(self.tests_dir, self.work_dir / "tests",
                        ignore=shutil.ignore_patterns("node_modules", "test-results", "playwright-report"))
        (self.work_dir / "playwright.config.ts").write_text(
            "import { defineConfig } from '@playwright/test';\n"
            f"export default defineConfig({{ testDir: './tests', timeout: {self.timeout_ms}, retries: 0, "
            f"fullyParallel: {'true' if os.environ.get('OCTOS_ARC_FULLY_PARALLEL') == '1' else 'false'}, "
            f"workers: {workers or self.workers}, reporter: [['json', {{ outputFile: 'report.json' }}]], "
            # Action/navigation/expect timeouts sit below the 10 s test timeout on
            # purpose: a hanging click then fails with the locator named in the
            # call log instead of an anonymous "Test timeout exceeded".
            f"expect: {{ timeout: {min(4000, self.timeout_ms // 2)} }}, "
            f"use: {{ headless: true, baseURL: process.env.E2E_BASE_URL, actionTimeout: {min(4000, self.timeout_ms // 2)}, "
            f"navigationTimeout: {min(6000, self.timeout_ms * 3 // 5)} }} }});\n")
        return self.work_dir / "playwright.config.ts"

    def run(self, spec_rel_paths: list[str], base_url: str, wall_timeout: int = 900,
            workers: int | None = None) -> RunSummary:
        config = self._prepare(workers)
        report_path = self.work_dir / "report.json"
        cmd = [str(self.root / "node_modules" / ".bin" / "playwright"), "test", "-c", str(config)]
        cmd += [str(self.work_dir / "tests" / p) for p in spec_rel_paths]
        env = dict(os.environ, E2E_BASE_URL=base_url, CI="1",
                   NODE_PATH=str(self.root / "node_modules"), **self.env_extra)
        env.pop("FORCE_COLOR", None)
        t0 = time.time()
        try:
            r = subprocess.run(cmd, cwd=self.work_dir, env=env, capture_output=True, text=True, timeout=wall_timeout)
            tail = ((r.stdout or "") + (r.stderr or ""))[-2000:]
        except subprocess.TimeoutExpired:
            return RunSummary(error=f"playwright run exceeded {wall_timeout}s")
        except OSError as exc:
            return RunSummary(error=f"playwright could not start: {exc}")
        if not report_path.exists():
            killed = r.returncode < 0 or "Killed" in tail
            return RunSummary(error=(f"playwright was killed (rc={r.returncode}); likely out of memory — "
                                     f"not an application failure" if killed else
                                     f"playwright produced no report (rc={r.returncode}): {_ANSI.sub('', tail)[-600:]}"),
                              killed=killed)
        try:
            summary = summarize_report(json.loads(report_path.read_text()))
        except (OSError, json.JSONDecodeError) as exc:
            return RunSummary(error=f"unreadable playwright report: {exc}")
        summary.stdout_tail = _ANSI.sub("", tail)
        if summary.total == 0:
            # Cloud run a6ccc437539f: the model had edited /workspace/tests, the
            # copied spec no longer loaded, and "0/0" looked like a verdict.
            detail = "; ".join(summary.load_errors) or summary.stdout_tail[-600:] or f"rc={r.returncode}"
            self.log(f"[acceptance] 0 tests collected from {', '.join(spec_rel_paths)}: {detail[:300]}")
            return RunSummary(error=f"Playwright collected 0 tests from {', '.join(spec_rel_paths)} "
                                    f"(spec files unreadable or broken): {detail}", load_errors=summary.load_errors)
        self.log(f"[acceptance] {summary.passed}/{summary.total} passed in {time.time()-t0:.0f}s "
                 f"({', '.join(spec_rel_paths)})")
        return summary
