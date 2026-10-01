#!/usr/bin/env python3
"""Acceptance command for ONE requirement node -- the `command validator` a
pipeline `shell_check` node runs: build the app, serve it, smoke GET /, exit 0
(pass) / non-zero (fail). The DAG scheduler turns that status into Pass/Fail
and hands stdout+stderr to the implement node over the failure back-edge, so
everything printed here is what the model sees on retry.

The check is driven by the requirement text only. This command never sees the
evaluation's test files: no specs are located, copied or executed here, and
the adapter passes none in. (A requirement-driven checker beyond the boot
smoke is future work; this version deliberately does not build one.)

The app dir is the CWD = the pipeline run dir, the only place this node's
write_file calls land (its file tools are fenced there).

usage: verify_node.py <port> [--tag ID --attempts N --deadline EPOCH --repair-window S]
       verify_node.py --seed <deliverable_dir>

A failing run prints STOP when this tag has used its N attempts, has been
repairing for longer than its window, or the deadline has passed; the repair back-edge does not fire on that marker, so the
pipeline moves on instead of spending the budget of the requirements to come.
Every step has its own timeout below the node's, because a shell_check that
overruns its node timeout is an ERROR that aborts the whole pipeline.
"""
import json, os, shutil, signal, socket, subprocess, sys, tempfile, time
from pathlib import Path

STOP = "ARC_NO_MORE_REPAIRS"
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


def check(port: int) -> int:
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
    # Verify a disposable copy: even a smoke run can leave a store behind in
    # the workspace, and that debris would ship with the app -- the grader
    # then starts from test debris instead of the seeded state.
    app = Path(tempfile.mkdtemp(prefix="arc-app-"))
    for part in ("frontend", "backend"):
        if (out / part).is_dir():
            shutil.copytree(out / part, app / part, ignore=shutil.ignore_patterns("node_modules", "dist"))
    try:
        return run_app(app, out, env, port)
    finally:
        shutil.rmtree(app, ignore_errors=True)


def run_app(app: Path, out: Path, env: dict, port: int) -> int:
    for cwd, step in ((app / "frontend", f"{INSTALL} && npm run build"), (app / "backend", INSTALL)):
        rc, log = sh(step, cwd, env, 240)
        if rc:
            print(f"[verify] {cwd.name}: {step!r} failed\n{log[-1500:]}")
            return 1
    if not free(port):
        print(f"[verify] port {port} already serving; refusing to score another process")
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
        # Requirement-text-only acceptance: the app must build, boot and serve
        # its home page.
        import urllib.request
        try:
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            code = opener.open(f"http://127.0.0.1:{port}/", timeout=30).status
        except Exception as exc:  # noqa: BLE001 -- any failure is the verdict
            code = exc
        print(f"[verify] smoke; GET / -> {code}")
        return 0 if code == 200 else 1
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
    rc = check(int(argv[0]))
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
