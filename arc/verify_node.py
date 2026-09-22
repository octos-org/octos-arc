#!/usr/bin/env python3
"""Acceptance command for ONE requirement node -- the `command validator` a
pipeline `shell_check` node runs: build the app, serve it, run that node's
public Playwright specs, exit 0 (pass) / non-zero (fail). The DAG scheduler
turns that status into Pass/Fail and hands stdout+stderr to the implement node
over the failure back-edge, so everything printed here is what the model sees
on retry. Playwright's exit code IS the verdict; nothing re-derives it.

The app dir is the CWD = the pipeline run dir, the only place this node's
write_file calls land (its file tools are fenced there).

usage: verify_node.py <tests_dir> <port> <spec.ts> [spec.ts ...]
"""
import json, os, shutil, signal, socket, subprocess, sys, time
from pathlib import Path

INSTALL = "npm install --no-audit --no-fund --no-package-lock"

# The harness owns the two manifests so the model never spends a turn on them
# (build copies src/* to dist; start runs server.js).
MANIFESTS = {
    "frontend/package.json": {"name": "f", "private": True, "scripts": {
        "build": "node -e \"const f=require('fs');f.rmSync('dist',{recursive:true,force:true});f.cpSync('src','dist',{recursive:true})\""}},
    "backend/package.json": {"name": "b", "private": True, "type": "commonjs",
                             "scripts": {"start": "node server.js"}},
}


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


def playwright_root(env: dict) -> Path | None:
    """A directory holding node_modules/@playwright/test. Preinstalled first
    (the platform image may ship one); otherwise a cached private install every
    later node check reuses."""
    for cand in filter(None, [os.environ.get("OCTOS_ARC_PLAYWRIGHT_ROOT"), "/workspace"]):
        if (Path(cand) / "node_modules" / "@playwright" / "test").is_dir():
            return Path(cand)
    root = Path(os.environ.get("TMPDIR", "/tmp")) / "arc-playwright"
    if (root / "node_modules" / "@playwright" / "test").is_dir():
        return root
    if os.environ.get("OCTOS_ARC_INSTALL_PLAYWRIGHT", "1") != "1":
        return None
    root.mkdir(parents=True, exist_ok=True)
    rc = subprocess.run("npm init -y >/dev/null 2>&1 && " + INSTALL + " @playwright/test "
                        "&& npx playwright install chromium",
                        cwd=root, env=env, shell=True).returncode
    return root if rc == 0 else None


def main(argv: list[str]) -> int:
    out = Path.cwd(); tests = Path(argv[0]).resolve(); port = int(argv[1]); specs = argv[2:]
    for rel, data in MANIFESTS.items():
        if not (out / rel).exists():
            (out / rel).parent.mkdir(parents=True, exist_ok=True)
            (out / rel).write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
    if not (out / "frontend" / "src").is_dir():
        print("[verify] no frontend/src: the implement node wrote nothing to verify")
        return 1
    env = os.environ.copy()
    if os.environ.get("NODE_BIN"):
        env["PATH"] = os.environ["NODE_BIN"] + ":" + env.get("PATH", "")
    for cwd, step in ((out / "frontend", f"{INSTALL} && npm run build"), (out / "backend", INSTALL)):
        r = subprocess.run(step, cwd=cwd, env=env, shell=True, capture_output=True, text=True)
        if r.returncode:
            print(f"[verify] {cwd.name}: {step!r} failed\n{(r.stdout + r.stderr)[-1500:]}")
            return 1
    root = playwright_root(env)
    if root is None:
        print("[verify] Playwright unavailable; cannot run the acceptance specs")
        return 1
    if not free(port):
        print(f"[verify] port {port} already serving; refusing to score another process")
        return 1

    work = root / "run"
    if work.exists():
        shutil.rmtree(work)
    (work / "tests").mkdir(parents=True)
    copied = 0
    for rel in specs:
        if (tests / rel).is_file():
            shutil.copy2(tests / rel, work / "tests" / Path(rel).name)
            copied += 1
    if not copied:
        # No specs means nothing was checked. Never report success for an empty
        # run -- that is exactly the "ships without being verified" path.
        print(f"[verify] no spec files found for {specs} under {tests}")
        return 1
    (work / "playwright.config.ts").write_text(
        "import { defineConfig } from '@playwright/test';\n"
        "export default defineConfig({ testDir: './tests', timeout: %s, retries: 0, workers: 4, "
        "reporter: [['list']], use: { headless: true, baseURL: process.env.E2E_BASE_URL } });\n"
        % os.environ.get("OCTOS_ARC_TEST_TIMEOUT_MS", "20000"))

    srv = subprocess.Popen("npm run start", cwd=out / "backend", env=dict(env, PORT=str(port)),
                           shell=True, stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT,
                           text=True, preexec_fn=os.setsid)
    try:
        for _ in range(60):
            if not free(port):
                break
            time.sleep(0.5)
        else:
            print(f"[verify] backend never bound port {port}")
            return 1
        run = subprocess.run(["npx", "playwright", "test", "-c", str(work / "playwright.config.ts")],
                             cwd=root, env=dict(env, E2E_BASE_URL=f"http://127.0.0.1:{port}"),
                             capture_output=True, text=True)
    finally:
        stop(srv)
    # Playwright's exit code IS the verdict and its list reporter already names
    # every failing assertion; print that verbatim for the repair round.
    print((run.stdout + run.stderr)[-4000:])
    return run.returncode


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
