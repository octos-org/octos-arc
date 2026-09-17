#!/usr/bin/env python3
"""Local grader: build+start a generated app, run the public Playwright specs against it.

usage: grade-local.py <output_dir> <requirement_id> [port]
"""
import json, os, shutil, signal, socket, subprocess, sys, time
from pathlib import Path
out = Path(sys.argv[1]).resolve(); req = sys.argv[2]; port = int(sys.argv[3]) if len(sys.argv) > 3 else 43300
root = Path(__file__).resolve().parent
specs = root / "public-tests" / req
grader = root / "local-grader"
if not (grader / "node_modules" / "@playwright").exists():
    grader.mkdir(exist_ok=True)
    subprocess.run("npm init -y >/dev/null && npm install --no-audit --no-fund @playwright/test && npx playwright install chromium", cwd=grader, env=env, shell=True, check=True)
env = os.environ.copy(); env["PATH"] = os.environ.get("NODE_BIN", "/opt/homebrew/opt/node@24/bin") + ":" + env["PATH"]
def sh(cmd, cwd, **kw):
    r = subprocess.run(cmd, cwd=cwd, env=env, shell=True, capture_output=True, text=True, **kw)
    return r.returncode, (r.stdout + r.stderr)[-1500:]
for step, cwd in (("npm install --no-audit --no-fund && npm run build", out/"frontend"), ("npm install --no-audit --no-fund", out/"backend")):
    rc, log = sh(step, cwd)
    print(f"[grade] {cwd.name}: {step!r} -> {rc}"); 
    if rc: print(log); sys.exit(2)
benv = dict(env, PORT=str(port))
srv = subprocess.Popen("npm run start", cwd=out/"backend", env=benv, shell=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, preexec_fn=os.setsid)
for _ in range(60):
    with socket.socket() as s:
        if s.connect_ex(("127.0.0.1", port)) == 0: break
    time.sleep(0.5)
else:
    print("[grade] backend never bound", port); os.killpg(srv.pid, signal.SIGTERM); sys.exit(3)
work = grader / "run" / req
if work.exists(): shutil.rmtree(work)
shutil.copytree(specs, work / "tests")
(work / "playwright.config.ts").write_text(
    "import { defineConfig } from '@playwright/test';\n"
    "export default defineConfig({ testDir: './tests', timeout: 60000, retries: 0, workers: 4, reporter: [['json', { outputFile: 'report.json' }], ['line']], use: { headless: true } });\n")
tenv = dict(env, E2E_BASE_URL=f"http://127.0.0.1:{port}")
t0 = time.time()
r = subprocess.run(["npx", "playwright", "test", "-c", str(work/"playwright.config.ts")], cwd=grader, env=tenv, capture_output=True, text=True)
os.killpg(srv.pid, signal.SIGTERM)
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
if passed < len(res):
    print(r.stdout[-3000:])
