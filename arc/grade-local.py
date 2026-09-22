#!/usr/bin/env python3
"""Local grader: build+start a generated app, run the public Playwright specs against it.

usage: grade-local.py <output_dir> <requirement_id> [port]
"""
import json, os, shutil, signal, socket, subprocess, sys, time
from pathlib import Path


# `--no-package-lock`: these installs run inside the app that is about to be
# scored, and the lock files they wrote were captured by the pre-test `git add
# -A` snapshot, which put them beyond the reach of the cleanup -- grading a
# bookstack run added two files to the deliverable it promises not to touch.
APP_INSTALL = "npm install --no-audit --no-fund --no-package-lock"


def app_install_steps(out) -> list[tuple]:
    """(working directory, shell command) for preparing the app to be graded."""
    return [(out / "frontend", f"{APP_INSTALL} && npm run build"),
            (out / "backend", APP_INSTALL)]


def port_is_free(port: int) -> bool:
    """True when nothing is listening on the loopback port."""
    with socket.socket() as probe:
        return probe.connect_ex(("127.0.0.1", port)) != 0


def stop_server(proc) -> None:
    """Tolerant teardown. The process group may already be gone, or belong to a
    session we cannot signal; either way the grader must not die here and leave
    the app listening for the next run to grade by mistake."""
    if proc is None or proc.poll() is not None:
        return
    for attempt in (lambda: os.killpg(os.getpgid(proc.pid), signal.SIGTERM), proc.terminate, proc.kill):
        try:
            attempt()
            proc.wait(timeout=10)
            return
        except (OSError, subprocess.TimeoutExpired):
            continue


def main(argv: list[str]) -> int:
    out = Path(argv[0]).resolve(); req = argv[1]; port = int(argv[2]) if len(argv) > 2 else 43300
    root = Path(__file__).resolve().parent
    specs = root / "public-tests" / req
    grader = root / "local-grader"
    env = os.environ.copy(); env["PATH"] = os.environ.get("NODE_BIN", "/opt/homebrew/opt/node@24/bin") + ":" + env["PATH"]
    if not (grader / "node_modules" / "@playwright").exists():
        grader.mkdir(exist_ok=True)
        subprocess.run("npm init -y >/dev/null && npm install --no-audit --no-fund @playwright/test && npx playwright install chromium", cwd=grader, env=env, shell=True, check=True)
    if not specs.is_dir():
        print(f"[grade] no public specs for {req} at {specs}"); return 4
    # A server left behind by an earlier crash answers on this port, and the
    # readiness probe below cannot tell it apart from ours -- that scores the
    # previous app under this app's name. Refuse instead of reporting a number.
    if not port_is_free(port):
        print(f"[grade] port {port} is already serving; free it or pass another port"); return 5
    def sh(cmd, cwd, **kw):
        r = subprocess.run(cmd, cwd=cwd, env=env, shell=True, capture_output=True, text=True, **kw)
        return r.returncode, (r.stdout + r.stderr)[-1500:]
    for cwd, step in app_install_steps(out):
        rc, log = sh(step, cwd)
        print(f"[grade] {cwd.name}: {step!r} -> {rc}")
        if rc: print(log); return 2
    # The specs mutate the app's persisted store; snapshot the (git-managed) output
    # dir and restore it afterwards so grading never changes what gets shipped.
    def git(*args): subprocess.run(["git", "-C", str(out), *args], capture_output=True)
    git("add", "-A")
    benv = dict(env, PORT=str(port))
    srv = subprocess.Popen("npm run start", cwd=out/"backend", env=benv, shell=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, preexec_fn=os.setsid)
    try:
        for _ in range(60):
            if not port_is_free(port): break
            time.sleep(0.5)
        else:
            print("[grade] backend never bound", port); return 3
        work = grader / "run" / f"{req}-{out.name}"
        if work.exists(): shutil.rmtree(work)
        shutil.copytree(specs, work / "tests")
        (work / "playwright.config.ts").write_text(
            "import { defineConfig } from '@playwright/test';\n"
            "export default defineConfig({ testDir: './tests', timeout: %s, retries: 0, workers: 4, reporter: [['json', { outputFile: 'report.json' }], ['line']], use: { headless: true, baseURL: process.env.E2E_BASE_URL } });\n"
            % os.environ.get("ARC_GRADE_TIMEOUT_MS", "10000"))
        tenv = dict(env, E2E_BASE_URL=f"http://127.0.0.1:{port}")
        t0 = time.time()
        r = subprocess.run(["npx", "playwright", "test", "-c", str(work/"playwright.config.ts")], cwd=grader, env=tenv, capture_output=True, text=True)
    finally:
        stop_server(srv)
        git("checkout", "--", "."); git("clean", "-fdq", "-e", "node_modules", "-e", "dist", "--", "frontend", "backend")
    rep = json.loads((work/"report.json").read_text()) if (work/"report.json").exists() else {}
    def walk(suites):
        for s in suites:
            for sp in s.get("specs", []):
                yield sp["title"], all(t.get("status") == "expected" or t.get("ok") for t in sp.get("tests", []))
            yield from walk(s.get("suites", []))
    res = list(walk(rep.get("suites", [])))
    passed = sum(1 for _, ok in res if ok)
    for title, ok in res: print(f"  {'PASS' if ok else 'FAIL'}  {title}")
    print(f"[grade] {req}: {passed}/{len(res)} passed in {time.time()-t0:.0f}s  score={100*passed/len(res) if res else 0:.0f}")
    (out / ".arc").mkdir(exist_ok=True)
    (out / ".arc" / "local-grade.json").write_text(json.dumps({"requirement": req, "passed": passed, "total": len(res),
        "tests": [{"title": t, "ok": ok} for t, ok in res]}, ensure_ascii=False, indent=1))
    if passed < len(res):
        print(r.stdout[-3000:])
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
