#!/usr/bin/env python3
"""Acceptance command for ONE requirement node (a pipeline `shell_check`):
build, boot the way the grader does (fresh dir, PORT), smoke GET /, audit the
served pages, run the self-check the implement node wrote FROM THE REQUIREMENT
TEXT; exit 0/1. Output goes back to the implement node on the repair back-edge.
Never touches the evaluation's test files.

usage: verify_node.py <port> [--tag ID --attempts N --deadline EPOCH --repair-window S]
                              [--e2e FILE | --e2e-dir DIR]
       verify_node.py --seed <deliverable_dir>

--e2e runs one self-check against the booted app; --e2e-dir runs every *.mjs
in the dir, each against its OWN app instance+port, OCTOS_ARC_CHECK_JOBS at a
time (default 2: one node+chromium lane is ~600MB, safe under the grader's
2g/1cpu). STOP on a failure means: attempts/window/deadline spent, move on.
"""
import json, os, re, shutil, signal, socket, subprocess, sys, tempfile, time
from concurrent.futures import ThreadPoolExecutor
from html.parser import HTMLParser
from pathlib import Path

STOP = "ARC_NO_MORE_REPAIRS"
INSTALL = "npm install --no-audit --no-fund --no-package-lock"
E2E_TIMEOUT = int(os.environ.get("OCTOS_ARC_SELF_CHECK_TIMEOUT", "180"))
CHECK_JOBS = int(os.environ.get("OCTOS_ARC_CHECK_JOBS", "2"))

# The harness owns the two manifests so the model never spends a turn on them
# (build copies src/* to dist; start runs server.js).
MANIFESTS = {
    "frontend/package.json": {"name": "f", "private": True, "scripts": {
        "build": "node -e \"const f=require('fs');f.rmSync('dist',{recursive:true,force:true});f.cpSync('src','dist',{recursive:true})\""}},
    "backend/package.json": {"name": "b", "private": True, "type": "commonjs",
                             "scripts": {"start": "node server.js"}},
}

# Server-log lines meaning the process is one request from dying (the
# ERR_HTTP_HEADERS_SENT class: an unhandled exception after a partial reply).
CRASH_RE = re.compile(r"Traceback \(most recent|Uncaught |unhandledRejection|ERR_HTTP_HEADERS_SENT"
                      r"|ReferenceError|TypeError|SyntaxError")

def seed(src: Path) -> int:
    """Start the run dir from the existing app, else the bundle's template."""
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
    """Tolerant teardown: never leave an app listening for the next node."""
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
    """One served page vs the rules requirements imply: one primary entry per
    action name, every control labeled, every button named."""

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
        if any(not b.strip() for b in audit.buttons):
            problems.append(f"{page.name}: a button has no accessible name")
        for a, wrapped in audit.inputs:
            t = (a.get("type") or "text").lower()
            if t in ("hidden", "submit", "button", "reset", "image", "file"):
                continue
            if not (wrapped or a.get("aria-label") or a.get("aria-labelledby")
                    or (a.get("id") and a["id"] in audit.label_for)):
                problems.append(f"{page.name}: {t} input has no visible label")
    return problems[:8]


def playwright_env(env: dict) -> dict | None:
    """The runner image ships @playwright/test + chromium; absent here means
    absent all run: STOP rather than spend repair rounds."""
    for root in (os.environ.get("OCTOS_ARC_PLAYWRIGHT_ROOT"), "/opt/arcbench"):
        if root and (Path(root) / "node_modules" / "@playwright" / "test" / "package.json").is_file():
            out = dict(env, NODE_PATH=str(Path(root) / "node_modules"))
            if not out.get("PLAYWRIGHT_BROWSERS_PATH") and Path("/ms-playwright").is_dir():
                out["PLAYWRIGHT_BROWSERS_PATH"] = "/ms-playwright"
            return out
    return None


def copy_app(out: Path, prefix: str) -> Path:
    """A disposable copy, so a check's boot writes no store debris that would
    ship with the app (the grader would then start dirty)."""
    app = Path(tempfile.mkdtemp(prefix=prefix))
    for part in ("frontend", "backend"):
        if (out / part).is_dir():
            shutil.copytree(out / part, app / part, ignore=shutil.ignore_patterns("node_modules", "dist"))
    return app


def prepare(app: Path, env: dict) -> bool:
    """Install+build; an untouched (zero-dep) manifest skips npm install."""
    for part, step in (("frontend", True), ("backend", False)):
        pj = app / part / "package.json"
        data = MANIFESTS.get(f"{part}/package.json")
        if not (pj.is_file() and data and pj.read_text() == json.dumps(data, indent=2) + "\n"):
            rc, log = sh(INSTALL, app / part, env, 240)
            if rc:
                print(f"[verify] {part}: npm install failed\n{log[-1200:]}")
                return False
        if step:
            rc, log = sh("npm run build", app / part, env, 240)
            if rc:
                print(f"[verify] frontend: npm run build failed\n{log[-1200:]}")
                return False
    return True


def boot(app: Path, env: dict, port: int, log_path: Path):
    """Serve the app; the process once it owns the port, else None."""
    srv = subprocess.Popen("npm run start", cwd=app / "backend", env=dict(env, PORT=str(port)),
                           shell=True, stdout=log_path.open("w"), stderr=subprocess.STDOUT,
                           text=True, preexec_fn=os.setsid)
    for _ in range(60):
        if not free(port) or srv.poll() is not None:
            break
        time.sleep(0.5)
    if free(port):
        stop(srv)
        print(f"[verify] backend never bound port {port}\n{log_path.read_text(errors='replace')[-1200:]}")
        return None
    return srv


def smoke(port: int):
    """The grader boots the app exactly like this; a failure here fails grading."""
    import urllib.request
    try:
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        return opener.open(f"http://127.0.0.1:{port}/", timeout=30).status
    except Exception as exc:  # noqa: BLE001 -- any failure is the verdict
        return exc


def playwright_run(files: list[Path], env: dict, port: int) -> int:
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


def isolated_checks(out: Path, env: dict, files: list[Path], port: int) -> int:
    """Final pass: every script against its OWN app instance+port, CHECK_JOBS
    at a time, so scripts may register/edit state without interfering."""

    def one(index_file):
        i, f = index_file
        app = copy_app(out, prefix=f"arc-iso{i}-")
        lane_port = port + 1 + i
        try:
            if not prepare(app, env):
                return 1
            srv = boot(app, env, lane_port, app / ".arc-server.log")
            if srv is None:
                return 1
            try:
                return playwright_run([f], env, lane_port)
            finally:
                stop(srv)
        finally:
            shutil.rmtree(app, ignore_errors=True)

    with ThreadPoolExecutor(max_workers=max(1, CHECK_JOBS)) as pool:
        results = pool.map(one, enumerate(files))
    return 1 if any(results) else 0


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

    app = copy_app(out, "arc-app-")
    try:
        if not prepare(app, env):
            return 1
        if not free(port):
            print(f"[verify] port {port} already serving; refusing to score another process")
            return 1
        problems = audit_pages(app / "frontend" / "dist")
        server_log = out / ".arc-server.log"   # a file, not a pipe: a chatty server never blocks
        srv = boot(app, env, port, server_log)
        if srv is None:
            return 1
        try:
            code = smoke(port)
            print(f"[verify] smoke; GET / -> {code}")
            rc = 0 if code == 200 else 1
            if e2e and rc == 0:
                rc = playwright_run([out / e2e], env, port)
            # A reply can succeed while the process is already doomed: a dead
            # server or an unhandled exception in its log fails the node too.
            time.sleep(0.5)
            log_text = server_log.read_text(errors="replace")[-3000:] if server_log.is_file() else ""
            if srv.poll() is not None or CRASH_RE.search(log_text):
                print(f"[verify] server unhealthy after checks (exit={srv.poll()}); log tail:\n{log_text[-1500:]}")
                rc = 1
            elif rc and log_text.strip():
                print(f"[verify] server log tail:\n{log_text[-800:]}")
        finally:
            stop(srv)
        if e2e_dir and (out / e2e_dir).is_dir():
            files = sorted((out / e2e_dir).glob("*.mjs"))
            if files:
                print(f"[verify] final pass: {len(files)} self-checks, {max(1, CHECK_JOBS)} lanes")
                rc = isolated_checks(out, env, files, port) or rc
        for p in problems:
            print(f"[verify] ui: {p}")
        return 1 if problems else rc
    finally:
        shutil.rmtree(app, ignore_errors=True)


def inventory(out: Path) -> str:
    """App source files with line counts: the next implement node's orientation."""
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
