#!/usr/bin/env python3
"""Acceptance command for ONE requirement node -- the `command validator` a
pipeline `shell_check` node runs: build the app, boot it the way the grader
does (fresh dir, PORT), smoke GET /, audit the served pages, and run the
self-check the implement node wrote from the requirement text. Exit 0 (pass)
/ non-zero (fail). The DAG scheduler turns that status into Pass/Fail and
hands stdout+stderr to the implement node over the failure back-edge, so
everything printed here is what the model sees on retry.

Every check derives from the requirement text. This command never sees the
evaluation's test files: none are located, copied or executed here, and the
adapter passes none in. The per-node browser check is written by the model
itself, from the requirement, at implement time.

The app dir is the CWD = the pipeline run dir, the only place this node's
write_file calls land (its file tools are fenced there).

usage: verify_node.py <port> [--tag ID --attempts N --deadline EPOCH --repair-window S]
                              [--e2e FILE | --e2e-dir DIR]
       verify_node.py --seed <deliverable_dir>

A failing run prints STOP when this tag has used its N attempts, has been
repairing for longer than its window, or the deadline has passed; the repair back-edge does not fire on that marker, so the
pipeline moves on instead of spending the budget of the requirements to come.
Every step has its own timeout below the node's, because a shell_check that
overruns its node timeout is an ERROR that aborts the whole pipeline.
"""
import json, os, re, shutil, signal, socket, subprocess, sys, tempfile, time
from html.parser import HTMLParser
from pathlib import Path

STOP = "ARC_NO_MORE_REPAIRS"
INSTALL = "npm install --no-audit --no-fund --no-package-lock"
E2E_TIMEOUT = int(os.environ.get("OCTOS_ARC_SELF_CHECK_TIMEOUT", "180"))

# The harness owns the two manifests so the model never spends a turn on them
# (build copies src/* to dist; start runs server.js).
MANIFESTS = {
    "frontend/package.json": {"name": "f", "private": True, "scripts": {
        "build": "node -e \"const f=require('fs');f.rmSync('dist',{recursive:true,force:true});f.cpSync('src','dist',{recursive:true})\""}},
    "backend/package.json": {"name": "b", "private": True, "type": "commonjs",
                             "scripts": {"start": "node server.js"}},
}

# Server-log lines that mean the process is one request away from dying (the
# ERR_HTTP_HEADERS_SENT class: an unhandled exception after a partial reply).
CRASH_RE = re.compile(r"Traceback \(most recent|Uncaught |unhandledRejection|ERR_HTTP_HEADERS_SENT"
                      r"|ReferenceError|TypeError|SyntaxError")


def seed(src: Path) -> int:
    """Start the run dir from the existing app (evolution tasks, the platform
    template), else the bundle's own template: the model extends real files
    instead of rebuilding from nothing, and nothing stale survives collection."""
    out = Path.cwd()
    for base in (src, Path(__file__).resolve().parent / "template"):
        if (base / "frontend").is_dir() and not (out / "frontend").exists():
            for part in ("frontend", "backend"):
                if (base / part).is_dir():
                    shutil.copytree(base / part, out / part, dirs_exist_ok=True,
                                    ignore=shutil.ignore_patterns("node_modules", "dist", ".git"))
            print(f"[seed] workspace seeded from {base}")
    return 0


def free(port: int) -> bool:
    with socket.socket() as probe:
        return probe.connect_ex(("127.0.0.1", port)) != 0


def stop(proc) -> None:
    """Tolerant teardown: never die here and leave the app listening, or the
    next node's check would score this node's server."""
    if proc is None or proc.poll() is not None:
        return
    for attempt in (lambda: os.killpg(os.getpgid(proc.pid), signal.SIGTERM), proc.terminate, proc.kill):
        try:
            attempt(); proc.wait(timeout=10); return
        except (OSError, subprocess.TimeoutExpired):
            continue


def sh(cmd, cwd, env, timeout):
    try:
        r = subprocess.run(cmd, cwd=cwd, env=env, shell=isinstance(cmd, str),
                           capture_output=True, text=True, timeout=timeout)
        return r.returncode, r.stdout + r.stderr
    except subprocess.TimeoutExpired as exc:
        out = (exc.stdout or b"") + (exc.stderr or b"")
        return 124, (out.decode(errors="replace") if isinstance(out, bytes) else out) + f"\n[timed out after {timeout}s]"


class PageAudit(HTMLParser):
    """One served page, checked against the generic UI rules the requirements
    imply: one primary entry per action name per page, every form control
    labeled, every button named."""

    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.links: list[str] = []
        self.buttons: list[str] = []
        self.inputs: list[tuple[dict, bool]] = []
        self.label_for: set[str] = set()
        self.stack: list[list] = []

    def handle_starttag(self, tag, attrs):
        a = dict(attrs)
        if tag == "label":
            if a.get("for"):
                self.label_for.add(a["for"])
            self.stack.append(["label", [], a])
        elif tag in ("a", "button"):
            self.stack.append([tag, [], a])
        elif tag in ("input", "select", "textarea"):
            self.inputs.append((a, any(f[0] == "label" for f in self.stack)))

    def handle_startendtag(self, tag, attrs):
        if tag in ("input", "select", "textarea"):
            self.inputs.append((dict(attrs), any(f[0] == "label" for f in self.stack)))

    def handle_endtag(self, tag):
        for i in range(len(self.stack) - 1, -1, -1):
            if self.stack[i][0] == tag:
                t, text, a = self.stack.pop(i)
                name = " ".join("".join(text).split())
                if t == "a":
                    self.links.append(name)
                elif t == "button":
                    self.buttons.append(name or a.get("aria-label", ""))
                break

    def handle_data(self, data):
        for frame in self.stack:
            frame[1].append(data)


def audit_pages(dist: Path) -> list[str]:
    problems = []
    for page in sorted(dist.glob("*.html")):
        audit = PageAudit()
        try:
            audit.feed(page.read_text(encoding="utf-8", errors="replace"))
        except Exception:  # noqa: BLE001 -- malformed HTML is the model's to fix
            problems.append(f"{page.name}: unparseable HTML")
            continue
        seen: dict[str, int] = {}
        for name in audit.links:
            if len(name) >= 2:
                seen[name] = seen.get(name, 0) + 1
        for name, count in seen.items():
            if count > 1:
                problems.append(f"{page.name}: {count} links named {name!r} (one primary entry per action)")
        for b in audit.buttons:
            if not b.strip():
                problems.append(f"{page.name}: a button has no accessible name")
        for a, wrapped in audit.inputs:
            t = (a.get("type") or "text").lower()
            if t in ("hidden", "submit", "button", "reset", "image", "file"):
                continue
            labeled = (wrapped or a.get("aria-label") or a.get("aria-labelledby")
                       or (a.get("id") and a["id"] in audit.label_for))
            if not labeled:
                problems.append(f"{page.name}: {t} input has no visible label")
    return problems[:8]


def playwright_env(env: dict) -> dict | None:
    """The runner image ships @playwright/test plus a chromium build; use that
    library for our own requirement-derived checks. Absent here means absent
    for the whole run: STOP rather than spend repair rounds."""
    for root in (os.environ.get("OCTOS_ARC_PLAYWRIGHT_ROOT"), "/opt/arcbench"):
        if root and (Path(root) / "node_modules" / "@playwright" / "test" / "package.json").is_file():
            out = dict(env, NODE_PATH=str(Path(root) / "node_modules"))
            if not out.get("PLAYWRIGHT_BROWSERS_PATH") and Path("/ms-playwright").is_dir():
                out["PLAYWRIGHT_BROWSERS_PATH"] = "/ms-playwright"
            return out
    return None


def run_self_checks(files: list[Path], env: dict, port: int) -> int:
    pw_env = playwright_env(env)
    if pw_env is None:
        print(f"[verify] no Playwright library available for self-checks\n{STOP}: no test runner")
        return 1
    rc_all = 0
    for f in files:
        rc, log = sh(["node", str(f)], f.parent, dict(pw_env, E2E_BASE_URL=f"http://127.0.0.1:{port}", CI="1"),
                     E2E_TIMEOUT)
        print(f"[verify] self-check {f.name}: {'ok' if rc == 0 else 'FAILED'}\n{log[-2500:]}")
        rc_all = rc_all or rc
    return rc_all


def check(port: int, e2e: str | None, e2e_dir: str | None) -> int:
    out = Path.cwd()
    for rel, data in MANIFESTS.items():
        if not (out / rel).exists():
            (out / rel).parent.mkdir(parents=True, exist_ok=True)
            (out / rel).write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
    if not (out / "frontend" / "src").is_dir():
        print("[verify] no frontend/src: the implement node wrote nothing to verify")
        return 1
    if e2e and not (out / e2e).is_file():
        print(f"[verify] {e2e} missing: the implement node must write this self-check "
              "from the requirement text (open pages, click, fill, assert the result)")
        return 1
    env = os.environ.copy()
    env.pop("FORCE_COLOR", None)           # plain text for the model reading the failure
    if os.environ.get("NODE_BIN"):
        env["PATH"] = os.environ["NODE_BIN"] + ":" + env.get("PATH", "")
    # Verify a disposable copy: even a smoke run can leave a store behind in
    # the workspace, and that debris would ship with the app -- the grader
    # then starts from test debris instead of the seeded state.
    app = Path(tempfile.mkdtemp(prefix="arc-app-"))
    for part in ("frontend", "backend"):
        if (out / part).is_dir():
            shutil.copytree(out / part, app / part, ignore=shutil.ignore_patterns("node_modules", "dist"))
    checks = [(out / e2e)] if e2e else sorted((out / e2e_dir).glob("*.mjs")) if e2e_dir and (out / e2e_dir).is_dir() else []
    try:
        return run_app(app, out, env, port, checks)
    finally:
        shutil.rmtree(app, ignore_errors=True)


def run_app(app: Path, out: Path, env: dict, port: int, checks: list[Path]) -> int:
    for cwd, step in ((app / "frontend", f"{INSTALL} && npm run build"), (app / "backend", INSTALL)):
        rc, log = sh(step, cwd, env, 240)
        if rc:
            print(f"[verify] {cwd.name}: {step!r} failed\n{log[-1500:]}")
            return 1
    if not free(port):
        print(f"[verify] port {port} already serving; refusing to score another process")
        return 1
    problems = audit_pages(app / "frontend" / "dist")

    server_log = out / ".arc-server.log"      # a file, not a pipe: a chatty server never blocks
    srv = subprocess.Popen("npm run start", cwd=app / "backend", env=dict(env, PORT=str(port)),
                           shell=True, stdout=server_log.open("w"), stderr=subprocess.STDOUT,
                           text=True, preexec_fn=os.setsid)
    try:
        for _ in range(60):
            if not free(port) or srv.poll() is not None:
                break
            time.sleep(0.5)
        if free(port):
            stop(srv)
            print(f"[verify] backend never bound port {port}\n{server_log.read_text(errors='replace')[-1500:]}")
            return 1
        # Boot smoke: the grader starts the app exactly this way, so a crash
        # here is a crash at grading time.
        import urllib.request
        try:
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            code = opener.open(f"http://127.0.0.1:{port}/", timeout=30).status
        except Exception as exc:  # noqa: BLE001 -- any failure is the verdict
            code = exc
        print(f"[verify] smoke; GET / -> {code}")
        rc = 0 if code == 200 else 1
        if checks and rc == 0:
            rc = run_self_checks(checks, env, port)
        # A reply can succeed while the process is already doomed: dead server
        # or an unhandled exception in its log fails the node either way.
        time.sleep(0.5)
        log_text = server_log.read_text(errors="replace")[-3000:] if server_log.is_file() else ""
        crash = CRASH_RE.search(log_text)
        if srv.poll() is not None or crash:
            print(f"[verify] server unhealthy after checks (exit={srv.poll()}); log tail:\n{log_text[-1500:]}")
            rc = 1
        elif rc and log_text.strip():
            print(f"[verify] server log tail:\n{log_text[-800:]}")
        for p in problems:
            print(f"[verify] ui: {p}")
        return 1 if problems else rc
    finally:
        stop(srv)


def inventory(out: Path) -> str:
    """The app's source files with line counts. This output is the next
    implement node's input, so it starts oriented instead of spending its
    first turns on list_dir/glob."""
    rows = []
    for part in ("frontend", "backend"):
        for f in sorted((out / part).rglob("*")) if (out / part).is_dir() else []:
            rel = f.relative_to(out)
            if f.is_file() and not {"node_modules", "dist"} & set(rel.parts) and f.stat().st_size < 1_000_000:
                rows.append(f"{rel} ({len(f.read_bytes().splitlines())} lines)")
    return "Workspace files: " + ", ".join(rows[:80])


def snapshot(out: Path, dest: Path) -> None:
    shutil.rmtree(dest, ignore_errors=True)
    for part in ("frontend", "backend"):
        if (out / part).is_dir():
            shutil.copytree(out / part, dest / part, ignore=shutil.ignore_patterns("node_modules", "dist"))


def main(argv: list[str]) -> int:
    if argv[:1] == ["--seed"]:
        return seed(Path(argv[1]))
    opts, it = {}, iter(argv[1:])
    for arg in it:
        if arg.startswith("--"):
            opts[arg[2:]] = next(it)
        else:
            print(f"[verify] unexpected positional argument: {arg}")
            return 2
    rc = check(int(argv[0]), opts.get("e2e"), opts.get("e2e-dir"))
    if rc == 0 and "tag" in opts:
        # Latest state a check passed: the adapter copies it into the output
        # dir as the run goes, so a run killed from outside still delivers.
        snapshot(Path.cwd(), Path.cwd() / ".arc-good" / "app")
        (Path.cwd() / ".arc-good" / "stamp").write_text(str(time.time()))
    print(inventory(Path.cwd()))
    if "tag" in opts:                           # the adapter reads the last verdict
        (Path.cwd() / ".arc-status").mkdir(exist_ok=True)
        (Path.cwd() / ".arc-status" / opts["tag"]).write_text(str(rc))
    if rc and "tag" in opts:
        counter = Path.cwd() / ".arc-attempts" / opts["tag"]
        counter.parent.mkdir(exist_ok=True)
        attempts = int(counter.read_text() or 0) + 1 if counter.is_file() else 1
        counter.write_text(str(attempts))
        first = counter.with_suffix(".first")          # when this requirement first failed
        if not first.is_file():
            first.write_text(str(time.time()))
        spent = time.time() - float(first.read_text())
        if (attempts >= int(opts.get("attempts", 6)) or time.time() >= float(opts.get("deadline", "inf"))
                or spent >= float(opts.get("repair-window", "inf"))):
            print(f"{STOP}: attempt {attempts} for {opts['tag']}; moving on")
    return rc


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
