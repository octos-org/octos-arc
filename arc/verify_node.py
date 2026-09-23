#!/usr/bin/env python3
"""Acceptance command for ONE requirement node -- the `command validator` a
pipeline `shell_check` node runs: build the app, serve it, run that node's
public Playwright specs, exit 0 (pass) / non-zero (fail). The DAG scheduler
turns that status into Pass/Fail and hands stdout+stderr to the implement node
over the failure back-edge, so everything printed here is what the model sees
on retry. Playwright's exit code IS the verdict; nothing re-derives it.

The app dir is the CWD = the pipeline run dir, the only place this node's
write_file calls land (its file tools are fenced there).

usage: verify_node.py <tests_dir> <port> [--tag ID --attempts N --deadline EPOCH --repair-window S --best 1] [spec.ts ...]
       verify_node.py --seed <deliverable_dir>

A failing run prints STOP when this tag has used its N attempts, has been
repairing for longer than its window, or the deadline has passed; the repair back-edge does not fire on that marker, so the
pipeline moves on instead of spending the budget of the requirements to come.
Every step has its own timeout below the node's, because a shell_check that
overruns its node timeout is an ERROR that aborts the whole pipeline.
"""
import json, os, re, shutil, signal, socket, subprocess, sys, tempfile, time
from pathlib import Path

STOP = "ARC_NO_MORE_REPAIRS"
PASSED = {"count": 0}               # tests the last check passed (for --best)
INSTALL = "npm install --no-audit --no-fund --no-package-lock"

# The harness owns the two manifests so the model never spends a turn on them
# (build copies src/* to dist; start runs server.js).
MANIFESTS = {
    "frontend/package.json": {"name": "f", "private": True, "scripts": {
        "build": "node -e \"const f=require('fs');f.rmSync('dist',{recursive:true,force:true});f.cpSync('src','dist',{recursive:true})\""}},
    "backend/package.json": {"name": "b", "private": True, "type": "commonjs",
                             "scripts": {"start": "node server.js"}},
}


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


PLAYWRIGHT_VERSION = "1.63.0"   # never `latest`: an unpinned install broke cloud grading once


def playwright_root(env: dict) -> tuple[Path | None, dict]:
    """A directory holding node_modules/@playwright/test, plus the env its
    browsers need. The runner image ships one (/opt/arcbench on the platform,
    seen in every cloud run); a private pinned install through the mirrors is
    the fallback, cached for every later check."""
    private = Path(os.environ.get("TMPDIR", "/tmp")) / "arc-playwright"
    cands = [os.environ.get("OCTOS_ARC_PLAYWRIGHT_ROOT"), "/opt/arcbench", "/workspace", "/workspace/tests"]
    rc, npm_root = sh(["npm", "root", "-g"], "/", env, 20)
    if rc == 0 and npm_root.strip():
        cands.append(str(Path(npm_root.strip().splitlines()[-1]).parent))
    has = lambda root: (Path(root) / "node_modules" / "@playwright" / "test").is_dir()  # noqa: E731
    for cand in filter(None, cands):
        if has(cand):
            return Path(cand), {}
    browsers = {"PLAYWRIGHT_BROWSERS_PATH": str(private / "browsers")}
    if has(private):
        return private, browsers if (private / "browsers").is_dir() else {}
    rc, hits = sh(["find", "/", "-maxdepth", "6", "-type", "d", "-path", "*/node_modules/@playwright/test",
                   "-not", "-path", "/proc/*", "-not", "-path", "/sys/*"], "/", env, 25)
    for hit in sorted(hits.split(), key=len):
        if hit.startswith("/") and has(Path(hit).parents[2]):
            return Path(hit).parents[2], {}
    if os.environ.get("OCTOS_ARC_INSTALL_PLAYWRIGHT", "1") != "1":
        return None, {}
    private.mkdir(parents=True, exist_ok=True)
    (private / "package.json").write_text('{"name": "arc-verify", "private": true}')
    mirror = dict(env, npm_config_registry="https://registry.npmmirror.com",
                  PLAYWRIGHT_DOWNLOAD_HOST="https://npmmirror.com/mirrors/playwright", **browsers)
    rc, log = sh(f"{INSTALL} @playwright/test@{PLAYWRIGHT_VERSION} && "
                 "./node_modules/.bin/playwright install chromium", private, mirror, 600)
    if rc:
        print(f"[verify] private Playwright install failed:\n{log[-800:]}")
    return (private, browsers) if rc == 0 else (None, {})


NO_BROWSER = "Executable doesn't exist"


def install_browser(pw: str, root: Path, env: dict) -> bool:
    """A Playwright whose browser build is missing (a wiped cache, a version
    bump) fails every spec in the same way -- a local run once burned 670
    nodes on that. Fetch chromium once, upstream then through the mirror."""
    for extra in ({}, {"PLAYWRIGHT_DOWNLOAD_HOST": "https://npmmirror.com/mirrors/playwright"}):
        rc, log = sh([pw, "install", "chromium"], root, dict(env, **extra), 600)
        if rc == 0:
            print("[verify] installed the missing Playwright browser")
            return True
    print(f"[verify] Playwright browser install failed:\n{log[-800:]}")
    return False


def check(tests: Path, port: int, specs: list[str]) -> int:
    out = Path.cwd()
    for rel, data in MANIFESTS.items():
        if not (out / rel).exists():
            (out / rel).parent.mkdir(parents=True, exist_ok=True)
            (out / rel).write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
    if not (out / "frontend" / "src").is_dir():
        print("[verify] no frontend/src: the implement node wrote nothing to verify")
        return 1
    env = os.environ.copy()
    env.pop("FORCE_COLOR", None)           # plain text for the model reading the failure
    if os.environ.get("NODE_BIN"):
        env["PATH"] = os.environ["NODE_BIN"] + ":" + env.get("PATH", "")
    # Verify a disposable copy: the specs create, edit and delete records, and
    # a store they leave behind in the workspace ships with the app -- the
    # grader then starts from test debris instead of the seeded state (keep:
    # 15 requirements passed their own checks, 6/32 at grading).
    app = Path(tempfile.mkdtemp(prefix="arc-app-"))
    for part in ("frontend", "backend"):
        if (out / part).is_dir():
            shutil.copytree(out / part, app / part, ignore=shutil.ignore_patterns("node_modules", "dist"))
    try:
        return run_app(app, out, env, tests, port, specs)
    finally:
        shutil.rmtree(app, ignore_errors=True)


def run_app(app: Path, out: Path, env: dict, tests: Path, port: int, specs: list[str]) -> int:
    for cwd, step in ((app / "frontend", f"{INSTALL} && npm run build"), (app / "backend", INSTALL)):
        rc, log = sh(step, cwd, env, 240)
        if rc:
            print(f"[verify] {cwd.name}: {step!r} failed\n{log[-1500:]}")
            return 1
    if not free(port):
        print(f"[verify] port {port} already serving; refusing to score another process")
        return 1
    root, pw_env = playwright_root(env) if specs else (None, {})
    if specs and root is None:
        # Nothing the model can fix: stop, do not spend repair rounds on it.
        print(f"[verify] Playwright unavailable; cannot run the acceptance specs\n{STOP}: no test runner")
        return 1

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
        if not specs:
            # No public example for this requirement: the app must still build,
            # boot and serve its home page.
            import urllib.request
            try:
                opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
                code = opener.open(f"http://127.0.0.1:{port}/", timeout=30).status
            except Exception as exc:  # noqa: BLE001 -- any failure is the verdict
                code = exc
            print(f"[verify] no public spec; GET / -> {code}")
            return 0 if code == 200 else 1
        # Specs `import '@playwright/test'` and Node resolves that upward from
        # the spec, so the copy sits under the install when it is writable
        # (NODE_PATH covers the temp-dir fallback).
        work = root / ".octos-acceptance" / "run"
        try:
            if work.exists():
                shutil.rmtree(work)
            (work / "tests").mkdir(parents=True)
        except OSError:
            work = Path(tempfile.mkdtemp(prefix="arc-verify-")) / "run"
            (work / "tests").mkdir(parents=True)
        # The node's specs plus the helpers they import (support/*.ts), at the
        # same relative paths so `../support/e2e` still resolves.
        helpers = [p.relative_to(tests) for p in tests.rglob("*")
                   if p.is_file() and "node_modules" not in p.parts and not p.name.endswith(".spec.ts")
                   and p.suffix in (".ts", ".js", ".mjs", ".cjs", ".json")]
        for rel in [*specs, *helpers]:
            if (tests / rel).is_file():
                (work / "tests" / rel).parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(tests / rel, work / "tests" / rel)
        # The platform grades with a 10 s per-test timeout; verify under the
        # same limit so a slow app fails here, where it can still be repaired.
        (work / "playwright.config.ts").write_text(
            "import { defineConfig } from '@playwright/test';\n"
            "export default defineConfig({ testDir: './tests', outputDir: './test-results', timeout: %s, retries: 0, workers: 4, "
            "reporter: [['list']], use: { headless: true, baseURL: process.env.E2E_BASE_URL } });\n"
            % os.environ.get("OCTOS_ARC_TEST_TIMEOUT_MS", "10000"))
        pw = str(root / "node_modules" / ".bin" / "playwright")
        run_env = dict(env, E2E_BASE_URL=f"http://127.0.0.1:{port}", CI="1",
                       NODE_PATH=str(root / "node_modules"), **pw_env)
        run = lambda: sh([pw, "test", "-c", str(work / "playwright.config.ts")], work, run_env,  # noqa: E731
                         int(os.environ.get("OCTOS_ARC_PLAYWRIGHT_TIMEOUT", "600")))
        rc, log = run()
        if rc and NO_BROWSER in log and install_browser(pw, root, run_env):
            rc, log = run()
    finally:
        stop(srv)
    if rc and NO_BROWSER in log:
        # The runner has no browser and none could be installed: nothing the
        # model can fix, so do not spend repair rounds on it.
        print(f"[verify] Playwright has no browser to run the specs\n{log[-800:]}\n{STOP}: no browser")
        return 1
    # Playwright's exit code IS the verdict and its list reporter already names
    # every failing assertion; print that verbatim for the repair round.
    print(log[-6000:])
    counts = re.findall(r"(\d+) passed", log)
    PASSED["count"] = int(counts[-1]) if counts else 0
    if rc:
        # What the page actually showed when each test failed (Playwright's
        # ARIA snapshot): the difference between "not found" and why.
        for ctx in sorted((work / "test-results").rglob("error-context.md"))[:3]:
            page = ctx.read_text(errors="replace").partition("```yaml")[2].split("```")[0]
            if page.strip():
                print(f"\n----- page at failure: {ctx.parent.name} -----\n{page.strip()[:1500]}")
    return rc


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


def keep_best(out: Path, rc: int) -> None:
    """Snapshot the app when this full-suite check passed more tests than any
    before it. A repair that breaks more than it fixes, or a run cut off in
    the middle of one, must not ship: the adapter delivers the snapshot."""
    best = out / ".arc-best"
    score = best / "score.json"
    prev = json.loads(score.read_text())["passed"] if score.is_file() else -1
    if PASSED["count"] <= prev:
        return
    snapshot(out, best / "app")
    score.write_text(json.dumps({"passed": PASSED["count"], "rc": rc}))
    print(f"[verify] best full-suite state so far: {PASSED['count']} passed (kept)")


def main(argv: list[str]) -> int:
    if argv[:1] == ["--seed"]:
        return seed(Path(argv[1]))
    opts, specs, it = {}, [], iter(argv[2:])
    for arg in it:
        if arg.startswith("--"):
            opts[arg[2:]] = next(it)
        else:
            specs.append(arg)
    if "regress" in opts:
        # Regression checkpoint: also re-run the specs of earlier requirements
        # whose last verdict was a pass.
        status = Path.cwd() / ".arc-status"
        earlier = [rel for tag, rels in json.loads(Path(opts["regress"]).read_text()).items()
                   if tag != opts.get("tag") and (status / tag).is_file()
                   and (status / tag).read_text().strip() == "0" for rel in rels]
        extra = [rel for rel in dict.fromkeys(earlier) if rel not in specs]
        if extra:
            print(f"[verify] regression checkpoint: also re-running {len(extra)} spec(s) of earlier "
                  "requirements that passed; a failure there is a regression to fix now")
            specs = [*specs, *extra]
    rc = check(Path(argv[0]).resolve(), int(argv[1]), specs)
    if "best" in opts:
        keep_best(Path.cwd(), rc)
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
