#!/usr/bin/env python3
"""ARC-Bench custom agent bundle: Octos as the coding agent.

The ARC-Bench platform invokes this as:

    python main.py <requirement_path> [--output-dir DIR] [--web-port N]

Flow (one requirement node at a time, dependencies first):

    skeleton turn (create mode only)
    for node in topological order:
        design turn      -> .arc/design/<node>.json + traceability contract
        implement turn   -> code
        acceptance loop  -> run the node's Playwright specs locally, feed the
                            four-field failure digest back, K <= 5 repairs,
                            commit on improvement, roll back on regression
        traceability     -> design_done / implementation_done / test_passed|failed
    startup rehearsal (build + start exactly like the grader)

Evolution mode (ARCBENCH_TEMPLATE_DIR already holds frontend/ + backend/):
skip the skeleton, diff the requirement tree against the previous run's
`.arc/traceability/requirements.json`, implement only new/changed nodes and
regression-test the unchanged ones.

Environment (all optional):
    OPENAI_API_KEY / OPENAI_BASE_URL / MODEL   OpenAI-compatible endpoint
    OCTOS_BIN                 octos binary (default: ./bin/octos, PATH, download)
    OCTOS_NODE_TIMEOUT        seconds per model turn (default 1200)
    OCTOS_TIME_BUDGET         seconds for the whole generation (default max(3600, 1500 x nodes))
    OCTOS_SECONDS_PER_NODE    per-node allowance used for that default (1500)
    OCTOS_MIN_REPAIR_SECONDS  do not start a repair turn with less than this left (300)
    OCTOS_NODE_TIME_BUDGET    cap per node incl. repairs (default 1500)
    OCTOS_REPAIR_ROUNDS       K, acceptance repair rounds per node (default 5)
    OCTOS_DESIGN_TURN         "0" disables the design turn
    OCTOS_DESIGN_MODE         inline (default) | separate (own read-only design turn)
    OCTOS_DESIGN_MIN_NODES    design only for trees with at least this many nodes (3)
    OCTOS_SKELETON_MIN_NODES  separate skeleton turn only for trees with at least this many nodes (3)
    OCTOS_SMALL_TASK_NODES    trees up to this size get the minimal self-verification text (2)
    OCTOS_VERIFY_MODE         auto (default) | minimal | full
    OCTOS_ARC_REASONING       auto (default: none for <=1 node to implement, else low) | low | medium | high | none | passthrough
    OCTOS_ARC_IMPLEMENT_REASONING  optional override for first implement turns of small tasks (default: base mode)
    OCTOS_ARC_INLINE_SPECS    "0" stops quoting the node's spec files into the prompt (default: quote up to 24k chars)
    OCTOS_ARC_DESTREAM        "0" lets streaming requests reach the platform as SSE (default: one JSON response upstream)
    OCTOS_ARC_TRIM_PROMPT     "0" keeps the kernel system prompt and all tool schemas (default: drop ARC-irrelevant sections/tools)
    OCTOS_ARC_DROP_SHELL      "0" leaves bash/shell available in minimal-verification turns (default: removed)
    OCTOS_ARC_IMPLEMENT_REQUESTS / OCTOS_ARC_REPAIR_REQUESTS  hard per-turn request caps enforced at the proxy (20 for small tasks / 10; 0 = off)
    OCTOS_ARC_REWRITE_ON_ZERO "0" disables the single full-rewrite turn when round 0 passes nothing
    OCTOS_ARC_INLINE_SOURCE_CHARS  budget for quoting the app's sources into repair/rewrite prompts (40000; 0 = off)
    OCTOS_ARC_MAX_TOKENS      minimum max_tokens the proxy enforces on chat requests (32768; kernel arc.11 sends 4096)
    OCTOS_ARC_CODEGEN         "0" disables one-request codegen turns for one-node tasks (default on)
    OCTOS_SESSION_SCOPE       turn (default) | node | run — when a fresh octos session starts
    OCTOS_ARC_INSTALL_PLAYWRIGHT  "0" never installs Playwright on the fly
    OCTOS_ARC_ALIAS_SPEC_IDS  "0" stops mirroring node states onto spec ids
    OCTOS_PERF_CONTRACT       "0" drops the performance rules from prompts
    OCTOS_GUARD               "0" logs guard findings without injecting them
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parent))
from arcbench_agent_runtime import AgentRuntime  # noqa: E402
from acceptance import (  # noqa: E402
    workers_for_memory,
    AcceptanceRunner, AppServer, RunSummary, acceptance_work_dir, container_memory_limit, ensure_playwright,
    failure_summaries, find_playwright_by_search, find_playwright_root, map_specs_to_nodes,
    nodes_for_failures, playwright_candidates, playwright_version_hint, restore_tree,
    restore_worktree, snapshot_worktree, tree_digest, workers_for_final, reap_workspace_processes)
from codegen import FORMAT_INSTRUCTIONS, dedupe_nav_links, parse_file_blocks, write_files  # noqa: E402
from guard import TurnMonitor  # noqa: E402
from llm_proxy import LlmProxy  # noqa: E402
from requirement_order import ancestors_of, node_fingerprint, topo_order  # noqa: E402

BUNDLE_DIR = Path(__file__).resolve().parent


def log(msg: str) -> None:
    """Progress lines go to BOTH stdout and stderr (the platform truncates
    stdout on long runs but keeps stderr as a separate field)."""
    print(msg, flush=True)
    print(msg, file=sys.stderr, flush=True)


# ---------------------------------------------------------------- postflight

def _postflight_structure_check(output_dir: Path) -> None:
    """Log the deliverable tree; lift a one-level-nested app into place."""
    tree_lines = []
    for root, dirs, files in os.walk(output_dir):
        dirs[:] = [d for d in dirs if d not in ("node_modules", ".git", "dist", "__pycache__")]
        depth = Path(root).relative_to(output_dir).parts
        if len(depth) > 2:
            dirs[:] = []
            continue
        indent = "  " * len(depth)
        tree_lines.append(f"{indent}{Path(root).name}/")
        for f in sorted(files)[:8]:
            tree_lines.append(f"{indent}  {f}")
        if len(tree_lines) > 60:
            tree_lines.append("... (truncated)")
            break
    log("[postflight] workspace tree:\n" + "\n".join(tree_lines))
    if (output_dir / "frontend").is_dir() and (output_dir / "backend").is_dir():
        log("[postflight] frontend/ and backend/ present at workspace root")
        return
    for child in [p for p in output_dir.iterdir() if p.is_dir() and p.name not in (".git", ".arc", "requirements")]:
        if (child / "frontend").is_dir() and (child / "backend").is_dir():
            log(f"[postflight] app found nested at {child.name}/; lifting to root")
            for item in child.iterdir():
                dest = output_dir / item.name
                if not dest.exists():
                    shutil.move(str(item), str(dest))
            return
    log("[postflight] WARNING: no frontend/+backend/ found anywhere; runner will reject the template")


def _reap_stray_processes(tag: str) -> None:
    """Log memory + the fattest processes, then kill whatever the agent left
    behind (browsers it launched to run the specs itself, servers, the
    octos runtime). Cloud runs 0764e8d77c54 / e60fb3545eae (2026-09-12):
    generation finished cleanly, then the grader's Playwright process was
    SIGKILLed one second after spawning 4 workers and every test was
    reported as skipped — the container had no memory left for it."""
    me = os.getpid()
    def _run(cmd: list[str]) -> str:
        try:
            return subprocess.run(cmd, capture_output=True, text=True,
                                  timeout=20).stdout
        except (OSError, subprocess.TimeoutExpired) as exc:
            return f"<{cmd[0]} unavailable: {exc}>"
    log(f"[reap:{tag}] memory:\n" + _run(["free", "-m"]).rstrip())
    # `free` shows the host, not the container's cgroup limit — that limit
    # is what SIGKILLs the grader's 4-worker Playwright run.
    cg = []
    for f in ("/sys/fs/cgroup/memory.max", "/sys/fs/cgroup/memory.current",
              "/sys/fs/cgroup/memory.peak", "/sys/fs/cgroup/memory.events",
              "/sys/fs/cgroup/memory/memory.limit_in_bytes",
              "/sys/fs/cgroup/memory/memory.max_usage_in_bytes",
              "/sys/fs/cgroup/memory/memory.failcnt", "/sys/fs/cgroup/pids.max"):
        try:
            cg.append(f"{f}={Path(f).read_text().strip().replace(chr(10), ' ')}")
        except OSError:
            pass
    log(f"[reap:{tag}] cgroup: " + ("; ".join(cg) or "<no cgroup files>"))
    ps = _run(["ps", "-eo", "pid,ppid,rss,etime,args", "--sort=-rss"])
    log(f"[reap:{tag}] top processes by RSS:\n"
        + "\n".join(ps.splitlines()[:20]))
    victims: list[int] = []
    for line in ps.splitlines()[1:]:
        parts = line.split(None, 4)
        if len(parts) < 5:
            continue
        pid, args_ = int(parts[0]), parts[4]
        if pid == me or pid == os.getppid():
            continue
        low = args_.lower()
        if any(k in low for k in ("chrom", "headless_shell", "playwright",
                                  "octos serve", "node ", "npm ", "/node")):
            victims.append(pid)
    for sig in (signal.SIGTERM, signal.SIGKILL):
        for pid in victims:
            try:
                os.kill(pid, sig)
            except OSError:
                pass
        time.sleep(2 if sig == signal.SIGTERM else 0)
    if victims:
        log(f"[reap:{tag}] killed {len(victims)} stray process(es): {victims}")
        log(f"[reap:{tag}] memory after:\n" + _run(["free", "-m"]).rstrip())
    else:
        log(f"[reap:{tag}] nothing to kill")




def _free_web_port(web_port: int) -> None:
    """Best-effort kill of whatever still listens on the app port."""
    try:
        pids = subprocess.run(["lsof", "-ti", f":{web_port}"], capture_output=True, text=True, timeout=15).stdout.split()
    except (OSError, subprocess.TimeoutExpired):
        pids = []
    if not pids:
        log(f"[postflight] port {web_port} already free")
        return
    for cmd in (["fuser", "-k", f"{web_port}/tcp"], ["sh", "-c", f"lsof -ti :{web_port} | xargs -r kill"]):
        try:
            if subprocess.run(cmd, capture_output=True, text=True, timeout=15).returncode == 0:
                log(f"[postflight] killed {len(pids)} listener(s) on port {web_port} via {cmd[0]}: {pids}")
                return
        except (OSError, subprocess.TimeoutExpired):
            continue
    log(f"[postflight] port {web_port} cleanup attempted (no tool matched)")


def _port_watchdog(web_port: int, output_dir: Path, stop: threading.Event) -> None:
    """Kill OUR processes that bind the grading port during generation (the
    runner terminates a run that serves the grading port early). Foreign
    listeners are left alone: the runner host is shared."""
    root = str(output_dir).rstrip("/")
    while not stop.is_set():
        try:
            pids = subprocess.run(["lsof", "-ti", f":{web_port}"], capture_output=True, text=True, timeout=10).stdout.split()
        except (OSError, subprocess.TimeoutExpired):
            pids = []
        for pid in pids:
            try:
                cwd = os.readlink(f"/proc/{pid}/cwd")
            except OSError:
                cwd = ""
            if cwd.startswith(root):
                log(f"[watchdog] port {web_port} bound by our process {pid} (cwd={cwd}); killing")
                try:
                    os.kill(int(pid), signal.SIGKILL)
                except (ProcessLookupError, PermissionError, ValueError):
                    pass
            else:
                log(f"[watchdog] port {web_port} held by foreign process {pid} (cwd={cwd or '?'}); leaving it")
        stop.wait(5)


# ---------------------------------------------------------------- requirements

def load_requirement_tree(req_dir: Path) -> dict:
    req_file = req_dir / "requirements.yaml"
    if not req_file.exists():
        req_file = req_dir / "requirements.yml"
    data = yaml.safe_load(req_file.read_text(encoding="utf-8"))
    if isinstance(data, dict) and "id" not in data:
        for wrapper in ("root", "requirement"):
            if isinstance(data.get(wrapper), dict):
                data = data[wrapper]
                break
    if not isinstance(data, dict) or "id" not in data:
        raise ValueError(f"invalid requirements.yaml in {req_dir}")
    return data


def describe_node(node: dict) -> str:
    lines = [f"ID: {node.get('id')}", f"Name: {node.get('name', '')}"]
    if node.get("description"):
        lines.append(f"Description: {node['description']}")
    scenarios = node.get("scenarios") or []
    if scenarios:
        lines.append("Scenarios:")
        for sc in scenarios:
            lines.append(f"  - {sc.get('name', 'scenario')}")
            for step in sc.get("steps") or []:
                if isinstance(step, dict):
                    lines.append(f"      {step.get('keyword', '')} {str(step.get('content', '')).strip()}")
    deps = node.get("dependencies") or []
    if deps:
        lines.append(f"Depends on: {', '.join(map(str, deps))}")
    return "\n".join(lines)


def folder_descendants(tree: dict) -> dict[str, list[str]]:
    """Non-atomic node id -> ids of its ATOMIC descendants (document order)."""
    out: dict[str, list[str]] = {}

    def walk(node: dict) -> list[str]:
        children = [c for c in (node.get("children") or []) if isinstance(c, dict)]
        node_type = str(node.get("type") or "").upper()
        node_id = str(node.get("id") or "")
        if node_type == "ATOMIC" or (not children and node_type != "FOLDER"):
            return [node_id]
        ids: list[str] = []
        for child in children:
            ids.extend(walk(child))
        if node_id:
            out[node_id] = ids
        return ids

    walk(tree)
    return out


def previous_requirement_records(output_dir: Path) -> dict[str, dict]:
    """The previous run's requirement table (committed with the template)."""
    path = output_dir / ".arc" / "traceability" / "requirements.json"
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}
    return {k: v for k, v in data.items() if isinstance(v, dict)} if isinstance(data, dict) else {}


def unchanged_node_ids(nodes: list[dict], previous: dict[str, dict]) -> set[str]:
    out = set()
    for node in nodes:
        prev = previous.get(str(node.get("id")))
        if prev and node_fingerprint(prev) == node_fingerprint(node):
            out.add(str(node.get("id")))
    return out


CODEGEN_MANIFESTS = {
    # Pages are copied twice: `register.html` and extensionless `register`, so a naive static
    # server that maps /register -> dist/register still finds the page (local s5/s8 first pass: 404 -> 0/6).
    "frontend/package.json": {"name": "f", "private": True, "scripts": {"build": "node -e \"const f=require('fs');f.mkdirSync('dist',{recursive:true});for(const n of f.readdirSync('src')){f.copyFileSync('src/'+n,'dist/'+n);if(n.endsWith('.html')&&n!=='index.html')f.copyFileSync('src/'+n,'dist/'+n.slice(0,-5))}\""}},
    # "type": "commonjs" pins the loader: Node 20.19 module detection treated a server.js mixing
    # import and require as ESM (cloud 3e425ce2ebf6: "require is not defined in ES module scope").
    "backend/package.json": {"name": "b", "private": True, "type": "commonjs", "scripts": {"start": "node server.js"}},
}


def write_codegen_manifests(output_dir: Path) -> list[str]:
    """Codegen turns never emit package.json: the harness writes the two fixed
    manifests (idempotent build copying src/* to dist, start running server.js)."""
    written = []
    for rel, data in CODEGEN_MANIFESTS.items():
        path = output_dir / rel
        if path.exists():
            continue
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
        written.append(rel)
    return written


def inline_sources(output_dir: Path, max_chars: int = 40000, exts: tuple = (".js", ".mjs", ".cjs", ".html", ".css", ".json")) -> str:
    """Quote the app's source files (frontend sources, backend JS) so a repair
    turn edits immediately instead of spending its request budget on reads.
    Bounded; largest files first are skipped when they would not fit."""
    files: list[Path] = []
    for part in ("frontend", "backend"):
        base = output_dir / part
        if base.is_dir():
            for path in sorted(base.rglob("*")):
                rel = path.relative_to(output_dir)
                if any(seg in ("node_modules", "dist", ".git", "data") for seg in rel.parts):
                    continue
                if path.is_file() and path.suffix in exts:
                    files.append(path)
    parts, total = [], 0
    for path in sorted(files, key=lambda p: p.stat().st_size):
        try:
            text = path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        if total + len(text) > max_chars:
            parts.append(f"--- {path.relative_to(output_dir)} --- (omitted, {len(text)} chars; read it if you must change it)\n")
            continue
        total += len(text)
        parts.append(f"--- {path.relative_to(output_dir)} ---\n{text.rstrip()}\n")
    return ("Current source files (quoted; edit them directly, no need to read):\n" + "".join(parts)) if parts else ""


def source_listing(output_dir: Path, limit: int = 60) -> str:
    """Short, stable listing of the app sources for evolution prompts."""
    lines = []
    for part in ("frontend", "backend"):
        base = output_dir / part
        if not base.is_dir():
            continue
        for path in sorted(base.rglob("*")):
            rel = path.relative_to(output_dir)
            if any(seg in ("node_modules", "dist", ".git") for seg in rel.parts):
                continue
            if path.is_file():
                lines.append(f"{rel} ({path.stat().st_size} B)")
            if len(lines) >= limit:
                lines.append("...")
                return "\n".join(lines)
    return "\n".join(lines)


# ---------------------------------------------------------------- octos driver

OCTOS_RELEASE_URL = (
    "https://github.com/octos-org/octos-arc/releases/download/v2.0.3-rc.11-arc.11/"
    "octos-bundle-x86_64-unknown-linux-gnu.tar.gz"
)


def _download_octos(dest_dir: Path) -> str:
    """Fetch the Linux octos binary at runtime via gh-proxy mirrors first (the
    runner's path to GitHub stalls / kills HTTP/2 streams)."""
    import tarfile
    import urllib.request

    dest_dir.mkdir(parents=True, exist_ok=True)
    tarball = dest_dir / "octos-bundle.tar.gz"
    url = os.environ.get("OCTOS_RELEASE_URL", OCTOS_RELEASE_URL)

    def tarball_ok() -> bool:
        try:
            with tarfile.open(tarball) as tf:
                return tf.getmember("octos") is not None
        except Exception:  # noqa: BLE001
            return False

    ok = tarball_ok()
    mirrors = [f"{prefix}/{url}" for prefix in ("https://ghfast.top", "https://gh-proxy.com")] + [url]
    for attempt in range(1, 13):
        if ok:
            break
        mirror = mirrors[(attempt - 1) % len(mirrors)]
        log(f"[octos] download attempt {attempt} ({mirror}) ...")
        if shutil.which("curl"):
            try:
                subprocess.run(["curl", "-fsSL", "--http1.1", "-C", "-", "--connect-timeout", "30",
                                "--speed-limit", "10240", "--speed-time", "60", "--retry", "2",
                                "-o", str(tarball), mirror], check=False, timeout=600)
            except subprocess.TimeoutExpired:
                log(f"[octos] attempt {attempt} killed after 600s stall; rotating mirror")
        else:
            try:
                urllib.request.urlretrieve(mirror, tarball)
            except Exception as exc:  # noqa: BLE001
                log(f"[octos] download error: {exc}")
        ok = tarball_ok()
    if not ok:
        raise RuntimeError("failed to download octos binary after 12 attempts")
    with tarfile.open(tarball) as tf:
        for member in ("octos", "octos-sandbox"):
            try:
                tf.extract(member, dest_dir, filter="data")
            except KeyError:
                pass
    binary = dest_dir / "octos"
    binary.chmod(0o755)
    if (dest_dir / "octos-sandbox").exists():
        (dest_dir / "octos-sandbox").chmod(0o755)
    return str(binary)


def find_octos() -> str:
    env_bin = os.environ.get("OCTOS_BIN")
    if env_bin and Path(env_bin).exists():
        return env_bin
    bundled = BUNDLE_DIR / "bin" / "octos"
    if bundled.exists():
        return str(bundled)
    found = shutil.which("octos")
    if found:
        return found
    cache_dir = Path(os.environ.get("OCTOS_CACHE_DIR", "/tmp/octos-bin"))
    if (cache_dir / "octos").exists():
        return str(cache_dir / "octos")
    return _download_octos(cache_dir)


def protected_hooks(protected_dirs: list[Path] | None) -> list[dict]:
    """before_tool_call hook denying file writes into the official tests /
    requirements directories (exit 1 = deny). Shell commands are redacted by
    the kernel and cannot be checked here; the harness restores the trees
    after every turn as the second layer."""
    hook_script = BUNDLE_DIR / "hooks" / "deny_protected.py"
    if not protected_dirs or not hook_script.is_file():
        return []
    return [{
        "event": "before_tool_call",
        "command": [sys.executable, str(hook_script), *[str(p) for p in protected_dirs]],
        "timeout_ms": 4000,
        "tool_filter": ["write_file", "edit_file", "diff_edit", "apply_patch", "create_file", "append_file"],
    }]


def write_profile_defaults(data_dir: Path, config_dir: Path, hooks: list[dict]) -> None:
    """Belt and braces: the solo ProfileRuntime builds its HookExecutor from
    the profile's own config (the stdio driver patches `hooks` into the
    profile registry file — the mechanism verified to deny with a real turn);
    a `profile-defaults.json` covers code paths that merge store defaults."""
    if not hooks:
        return
    for root in (data_dir, config_dir):
        try:
            root.mkdir(parents=True, exist_ok=True)
            (root / "profile-defaults.json").write_text(json.dumps({"hooks": hooks}, indent=2), encoding="utf-8")
        except OSError:
            pass


def build_octos_env(config_dir: Path, protected_dirs: list[Path] | None = None) -> dict:
    """Prepare env + minimal config.json for non-interactive octos.

    `protected_dirs` (official tests, requirements) get a before_tool_call
    hook that denies write_file/edit_file into them (exit 1 = deny)."""
    env = os.environ.copy()
    api_key = env.get("OPENAI_API_KEY", "")
    base_url = env.get("OPENAI_BASE_URL", "")
    model = os.environ.get("OCTOS_MODEL") or env.get("MODEL", "")
    provider = os.environ.get("OCTOS_PROVIDER")
    if not provider:
        provider = "deepseek" if "deepseek" in base_url else "anthropic" if "anthropic" in base_url else "openai"
    key_env = "OPENAI_API_KEY"
    if provider == "deepseek" and api_key:
        env.setdefault("DEEPSEEK_API_KEY", api_key)
        key_env = "DEEPSEEK_API_KEY"
    elif provider == "anthropic" and api_key:
        env.setdefault("ANTHROPIC_API_KEY", api_key)
        key_env = "ANTHROPIC_API_KEY"
    elif provider not in ("openai", "deepseek", "anthropic") and api_key:
        env.setdefault(f"{provider.upper()}_API_KEY", api_key)
        key_env = f"{provider.upper()}_API_KEY"
    config = {
        "provider": provider,
        "model": model,
        "sandbox": {"allow_network": True},
        "memory": {"refresh": {"enabled": False}},
        # deepseek-v4 spends its default 4096 output budget on reasoning and
        # returns empty content; give it real headroom.
        "gateway": {"max_output_tokens": 65536},
    }
    if provider not in ("openai", "deepseek", "anthropic") and base_url:
        config["base_url"] = base_url
    if provider == "deepseek":
        config["gateway"]["reasoning_effort"] = "low"
    hooks = protected_hooks(protected_dirs)
    if hooks:
        config["hooks"] = hooks
    config_dir.mkdir(parents=True, exist_ok=True)
    (config_dir / "config.json").write_text(json.dumps(config, indent=2), encoding="utf-8")
    env["OCTOS_CONFIG_DIR"] = str(config_dir)
    env.setdefault("OCTOS_DISABLE_STREAMING", "1")   # platform proxies reject SSE
    env.setdefault("OCTOS_DANGER_FULL_ACCESS", "1")  # the container is the sandbox
    env.setdefault("npm_config_registry", "https://registry.npmmirror.com")
    env.setdefault("NPM_CONFIG_REGISTRY", "https://registry.npmmirror.com")
    rules_file = BUNDLE_DIR / "EXTRA_RULES.md"
    if rules_file.is_file() and "OCTOS_ARC_EXTRA_RULES" not in env:
        env["OCTOS_ARC_EXTRA_RULES"] = rules_file.read_text(encoding="utf-8")[:8000]
    env["_ARC_PROVIDER"] = provider
    env["_ARC_MODEL"] = model
    env["_ARC_BASE_URL"] = base_url
    env["_ARC_KEY_ENV"] = key_env
    return env


_CHAT_FLAGS_CACHE: dict[str, set[str]] = {}


def _chat_supported_flags(octos_bin: str) -> set[str]:
    if octos_bin not in _CHAT_FLAGS_CACHE:
        try:
            proc = subprocess.run([octos_bin, "chat", "--help"], capture_output=True, text=True, timeout=30)
            help_text = (proc.stdout or "") + (proc.stderr or "")
        except Exception:  # noqa: BLE001
            help_text = ""
        _CHAT_FLAGS_CACHE[octos_bin] = {
            flag for flag in ("--json", "--cwd", "--data-dir", "--sandbox", "--profile",
                              "--max-iterations", "--no-session-persistence") if flag in help_text}
    return _CHAT_FLAGS_CACHE[octos_bin]


def run_octos(octos_bin: str, cwd: Path, prompt: str, env: dict, data_dir: Path,
              timeout: int, max_iterations: int) -> tuple[bool, str]:
    """One non-interactive `octos chat` turn (fallback driver)."""
    flags = _chat_supported_flags(octos_bin)
    cmd = [octos_bin, "chat", "-m", prompt]
    if "--json" in flags:
        cmd.append("--json")
    if "--cwd" in flags:
        cmd += ["--cwd", str(cwd)]
    if "--data-dir" in flags:
        cmd += ["--data-dir", str(data_dir)]
    if "--max-iterations" in flags:
        cmd += ["--max-iterations", str(max_iterations)]
    if "--no-session-persistence" in flags:
        cmd.append("--no-session-persistence")
    if "--sandbox" in flags and env.get("OCTOS_DANGER_FULL_ACCESS") == "1":
        cmd += ["--sandbox", "danger-full-access"]
    if "--profile" in flags:
        cmd += ["--profile", os.environ.get("OCTOS_CHAT_PROFILE", "coding")]
    try:
        proc = subprocess.run(cmd, cwd=str(cwd), env=env, capture_output=True, text=True,
                              timeout=timeout, errors="replace")
    except subprocess.TimeoutExpired:
        return False, f"octos timed out after {timeout}s"
    out = (proc.stdout or "").strip()
    if proc.returncode != 0:
        return False, f"octos exited {proc.returncode}: {out or (proc.stderr or '').strip()[-2000:]}"
    try:
        payload = json.loads(out)
        if isinstance(payload, dict) and payload.get("error"):
            return False, str(payload["error"])
        return True, str(payload.get("text", "")) if isinstance(payload, dict) else out
    except json.JSONDecodeError:
        return True, out[-4000:]


DRYRUN_FILES = """\
<<<FILE frontend/src/index.html>>>
<!DOCTYPE html><html><head><meta charset="utf-8"><title>dry run</title></head>
<body><!--NAV--><main data-testid="dryrun">dry run placeholder</main></body></html>
<<<END FILE>>>
<<<FILE backend/server.js>>>
const http = require('http'); const fs = require('fs'); const path = require('path');
const dist = path.join(__dirname, '..', 'frontend', 'dist');
const handler = (req, res) => { try {
  const file = path.join(dist, req.url === '/' ? 'index.html' : req.url.split('?')[0]);
  if (!file.startsWith(dist) || !fs.existsSync(file) || fs.statSync(file).isDirectory()) { res.writeHead(404); return res.end('not found'); }
  res.writeHead(200, {'Content-Type': 'text/html; charset=utf-8'}); res.end(fs.readFileSync(file));
} catch (e) { res.writeHead(500); res.end('error'); } };
http.createServer(handler).listen(process.env.PORT || 3000);
process.on('uncaughtException', () => {});
<<<END FILE>>>
"""


class DryRunDriver:
    """OCTOS_ARC_DRYRUN=1: no kernel, no model. Every turn returns a fixed reply
    (file blocks for codegen prompts, a sentence otherwise) so the whole flow —
    tree order, mode selection, probes, acceptance, repair/budget logic, events —
    runs end to end for structural parity checks. Real-path behaviour is untouched."""

    def __init__(self) -> None:
        self.hooks: list = []
        self.turns = 0

    def run(self, prompt: str, timeout: int, monitor: TurnMonitor | None = None) -> tuple[bool, str]:
        self.turns += 1
        time.sleep(0.05)
        if "<<<FILE" in prompt:
            return True, DRYRUN_FILES
        if "page markup only" in prompt:
            return True, "<!DOCTYPE html><html><head><meta charset=\"utf-8\"></head><body><main>dry run</main></body></html>"
        return True, "dry run: no model call; nothing written."

    def end_scope(self, *args, **kwargs) -> None:
        pass

    def close(self, *args, **kwargs) -> None:
        pass


class OctosDriver:
    """stdio UI-protocol session (default) or one-shot chat turns.

    By default every turn gets a fresh session: the per-turn prompt already
    carries all the state the model needs, and a short, byte-stable prefix
    (system prompt + tool schemas) is what the provider's prefix cache keys on.
    """

    def __init__(self, octos_bin: str, cwd: Path, env: dict, data_dir: Path,
                 max_iterations: int, events_log: Path) -> None:
        self.mode = os.environ.get("OCTOS_DRIVER", "stdio")
        # "turn": new session every turn; "node": one session per requirement
        # node (design -> implement -> repairs share context); "run": one session.
        # Default "turn" since specs are quoted into every prompt: a repair turn
        # is self-contained, while a shared node session made each repair
        # request carry the whole implement history (v8-tb: 49 requests, 1.1M
        # prompt tokens for 7 repairs).
        self.session_scope = os.environ.get("OCTOS_SESSION_SCOPE", "turn")
        if os.environ.get("OCTOS_SESSION_PER_TURN") == "0" and "OCTOS_SESSION_SCOPE" not in os.environ:
            self.session_scope = "run"
        self.octos_bin = octos_bin
        self.cwd = cwd
        self.env = env
        self.data_dir = data_dir
        self.max_iterations = max_iterations
        self.events_log = events_log
        self._session = None
        self.monitor: TurnMonitor | None = None
        self.hooks: list = []  # profile hooks (protected-directory deny), set by the flow

    def _log_event(self, method: str, params: dict) -> None:
        if method == "core/marker":
            log(f"[core-mod] {params.get('line', '')}")
        if self.monitor is not None and method in ("tool/started", "tool/completed"):
            try:
                self.monitor.observe(method, params)
            except Exception:  # noqa: BLE001 - guard must never break a turn
                pass
        try:
            with self.events_log.open("a", encoding="utf-8") as fh:
                fh.write(json.dumps({"method": method, "params": params}, ensure_ascii=False) + "\n")
        except OSError:
            pass

    def _get_session(self):
        if self._session is None:
            from octos_stdio import OctosStdioSession
            self._session = OctosStdioSession(self.octos_bin, self.cwd, self.env, self.data_dir,
                                              on_event=self._log_event)
            self._session.bootstrap_profile(
                provider=self.env.get("_ARC_PROVIDER", "openai"),
                model=self.env.get("_ARC_MODEL", ""),
                base_url=self.env.get("_ARC_BASE_URL") or None,
                api_key_env=self.env.get("_ARC_KEY_ENV") or None,
                hooks=self.hooks,
            )
            self._session.open()
        return self._session

    def run(self, prompt: str, timeout: int, monitor: TurnMonitor | None = None) -> tuple[bool, str]:
        self.monitor = monitor
        if self.mode == "chat":
            fn = lambda: run_octos(self.octos_bin, self.cwd, prompt, self.env, self.data_dir,  # noqa: E731
                                   timeout, self.max_iterations)
        else:
            fn = lambda: self._run_stdio(prompt, timeout)  # noqa: E731
        try:
            ok, text = self._run_with_heartbeat(lambda: self._run_with_retries(fn))
        finally:
            self.monitor = None
            if self.session_scope == "turn":
                self.close()
        if monitor is not None:
            monitor.finish(text)
        return ok, text

    @staticmethod
    def _run_with_heartbeat(fn) -> tuple[bool, str]:
        """Run a turn in a thread, logging a keepalive every 30s (the runner
        kills silent processes)."""
        box: dict = {}

        def target() -> None:
            try:
                box["r"] = fn()
            except Exception as exc:  # pragma: no cover - defensive
                box["r"] = (False, f"turn raised: {exc}"[:500])

        th = threading.Thread(target=target, daemon=True)
        th.start()
        t0 = time.time()
        while True:
            th.join(30)
            if not th.is_alive():
                break
            log(f"[flow] turn still running ({int(time.time() - t0)}s elapsed)")
        return box.get("r", (False, "turn thread ended without result"))

    @staticmethod
    def _transient(text: str) -> bool:
        lowered = text.lower()
        if "octos turn timed out" in lowered or "octos timed out after" in lowered:
            return False  # our own wall-clock cap, not a provider hiccup: never replay the turn
        return any(k in lowered for k in (
            "temporarily unavailable", "503", "502", "429", "rate limit", "timeout", "timed out",
            "connection reset", "overloaded", "failed to send", "streaming request",
            "403", "authentication failed", "401", "unauthorized"))

    def _run_with_retries(self, fn, attempts: int = 3) -> tuple[bool, str]:
        ok, text = fn()
        for attempt in range(2, attempts + 1):
            if ok or not self._transient(text):
                break
            wait = 30 * (attempt - 1)
            log(f"[driver] transient error, retry {attempt}/{attempts} after {wait}s: {text[:200]}")
            time.sleep(wait)
            self.close()
            ok, text = fn()
        return ok, text

    def _run_stdio(self, prompt: str, timeout: int) -> tuple[bool, str]:
        try:
            return self._get_session().run_turn(prompt, timeout=float(timeout))
        except Exception as exc:  # noqa: BLE001
            self.close()
            chat_ok, chat_text = run_octos(self.octos_bin, self.cwd, prompt, self.env, self.data_dir,
                                           timeout, self.max_iterations)
            if chat_ok:
                return True, chat_text
            return False, f"stdio driver error: {exc}; chat fallback: {chat_text}"[:1000]

    def end_scope(self, scope: str) -> None:
        """Called by the flow at node boundaries; closes the session when the
        configured scope ends."""
        if scope == self.session_scope or self.session_scope == "turn":
            self.close()

    def close(self) -> None:
        if self._session is not None:
            self._session.close()
            self._session = None


# ---------------------------------------------------------------- prompts
#
# Prompt text is deliberately static (no timestamps, fixed section order) so
# that identical turns share the provider's prefix cache.

UI_CONTRACT_CORE = """\
UI contract (the hidden Playwright tests depend on these; a violation scores 0):
- Buttons are real <button> elements, links are <a href>, every form control has a visible <label for=id>; their texts are copied VERBATIM from the requirement/spec (anchored regexes like /^name$/i reject "Full Name"). Use plain text/password/email inputs, native <select>/checkbox/radio; NEVER type="date"/"number". All controls exist in the served HTML itself and stay visible, enabled and editable at all times; no CSS transitions/animations and no JavaScript that re-renders or re-creates form controls after load (Playwright waits for elements to be "stable" — cloud run 954a231a3d23 timed out on a checkbox that kept changing).
- No native HTML5 validation attributes; validate in JavaScript and show ONE inline error element (role="alert") naming the problem (required / invalid / match / terms / duplicate). On error stay on the page and create no record.
- Strict mode: every echoed value (an entered name, a chosen option, a date) appears in EXACTLY ONE element per page; every link target appears in EXACTLY ONE <a> per page (never a nav link plus a call-to-action to the same href: a spec that clicks `a[href=...]` fails on two matches); never both a short and a long form of one entity, never a per-field error plus a summary. Serve a SEPARATE HTML document per route (`/`, `/register`, `/login`, ...) — never several forms in one document with hidden views: hidden inputs and labels still collide in getByLabel/getByRole.
- State: persist ONLY what the requirement says is persisted and reproduce that seed on EVERY fresh start; a page's initial state (e.g. "the count is initially 0") is per-page-load client state, never a shared server value — the grader runs several test files in parallel against ONE server. The initial state must already be in the served HTML (e.g. the element contains `0` in the markup); never leave it empty until a fetch completes — the tests assert immediately after load.
- Zero external requests (no CDN, fonts, analytics); assets small and same-origin.
- Live indicators (any element the spec reads back after typing) update their OWN element's text/attributes synchronously in the `input` event handler — never on change/blur, never debounced, never only a wrapper's class (specs compare the element's outerHTML before and after typing).
- Text only: never OCR reference images. Write files in your first actions.
"""

CODEGEN_SYSTEM = "You write complete, minimal web apps. Reply only with file blocks in the requested format."

CODEGEN_PROMPT = """\
Requirement {node_id}: {description}

Acceptance test (ground truth):
{spec}
Files: frontend/src/index.html (+ one html per further route); backend/server.js = CommonJS (require) Node http server on process.env.PORT||{port} serving ../frontend/dist files (index.html for /, <name>.html for /<name>) plus any API routes the requirement needs (in-memory state), 404 for anything else, wrapped in try/catch and process.on('uncaughtException').{ports} Both package.json files already exist (build copies src/* to dist; start runs server.js): do not output them.
Rules: texts, button names, labels and test ids exactly as in the test; the initial state is literally in the HTML; state lives in the page script unless the requirement says it is persisted; no external resources, no CSS, no comments, no notes; Playwright strict mode: every locator in the test must match exactly one element on the served page (no duplicate links, labels, texts or ids; each label's for= resolves to its own control). {size_rule}
"""

CODEGEN_SIZE_SMALL = "index.html <= 20 lines, server.js <= 20 lines."
CODEGEN_SIZE_FULL = ("As short as the tests allow; one page file per route. Mechanisms (follow exactly): "
                     "(1) every page contains the literal `<!--NAV-->` and no other navigation links; before sending, the server "
                     "replaces it with the signed-out links or the signed-in header (the exact link texts and labels the tests "
                     "expect, each exactly once), decided from the session cookie. "
                     "(2) Session cookie exactly `session=TOKEN; Path=/; HttpOnly; SameSite=Lax`; sign-out clears it and redirects to /. "
                     "(3) Validation: every value the test helpers generate is valid input and MUST be accepted; do not invent "
                     "stricter rules than the requirement states; reject only the cases the tests assert are rejected; each message is the FIRST alternative of the test's regex copied verbatim, shown in one persistent `role=alert` element. "
                     "(4) Elements the test expects visible have a non-empty box (never an empty div/span). "
                     "(5) No HTML5 validation attributes (required/pattern/type=email): the server validates.")

# Tiny-spec tier (OCTOS_ARC_TINY_SPEC_CHARS, default 1500; OCTOS_ARC_TINY=0 disables): the prompt is the
# spec's own statements only, the reply is one HTML file, the server is a fixed harness scaffold (no task
# logic), thinking is off. First-pass failure falls back to the compact codegen tier for the same node.
TINY_SYSTEM = "Reply with HTML only."

TINY_PROMPT = """\
Playwright test the page at / must pass:
{spec}
Reply with the page markup only: minimal elements + one inline <script>; no doctype, head, CSS, comments or blank lines.
"""

TINY_PROMPT_EVOLUTION = """\
Current index.html:
{page}
Additional Playwright test it must also pass (keep existing behaviour):
{spec}
Reply with the complete updated page markup only: minimal elements + one inline <script>; no doctype, head, CSS, comments or blank lines.
"""

TINY_SERVER_JS = """\
const http = require('http'); const fs = require('fs'); const path = require('path');
const dist = path.join(__dirname, '..', 'frontend', 'dist');
const handler = (req, res) => {{ try {{
  const url = req.url.split('?')[0];
  const name = url === '/' ? 'index.html' : url.replace(/^\\//, '');
  const candidates = [name, name + '.html'].map(n => path.join(dist, n));
  const file = candidates.find(f => f.startsWith(dist) && fs.existsSync(f) && fs.statSync(f).isFile());
  if (!file) {{ res.writeHead(404); return res.end('not found'); }}
  const type = file.endsWith('.js') ? 'application/javascript' : file.endsWith('.css') ? 'text/css' : 'text/html; charset=utf-8';
  res.writeHead(200, {{ 'Content-Type': type }}); res.end(fs.readFileSync(file));
}} catch (e) {{ res.writeHead(500); res.end('error'); }} }};
http.createServer(handler).listen(process.env.PORT || {port});
if (process.env.ARC_EXTRA_PORTS !== '0') for (const p of {extra_ports}) if (String(p) !== String(process.env.PORT || {port})) http.createServer(handler).listen(p);
process.on('uncaughtException', () => {{}}); process.on('unhandledRejection', () => {{}});
"""


def looks_like_markup(text: str) -> bool:
    """A bare page or page fragment (the tiny tier asks for markup without doctype/head)."""
    return bool(re.search(r"<(html|body|main|div|section|form|button|script|span|p|h[1-6]|input|label|ul|table)\b", text, re.IGNORECASE))


def strip_code_fences(text: str) -> str:
    text = text.strip()
    m = re.search(r"```[a-zA-Z]*\n(.*?)```", text, re.DOTALL)
    return m.group(1).strip() if m else text


def compact_spec_lines(text: str) -> str:
    """The spec's statements without imports, blank lines, `await` and closing
    braces — what a page must satisfy, in the spec's own words."""
    out = []
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith(("import ", "//", "/*", "*")) or line in ("});", "})", "}"):
            continue
        line = re.sub(r"^await\s+", "", line)
        line = re.sub(r"^test\((['\"])(.*?)\1,\s*async\s*\(\{[^}]*\}\)\s*=>\s*\{$", r"test: \2", line)
        out.append(line)
    return "\n".join(out)


UI_CONTRACT_DATA = """\
- Concrete example values in the requirement (seed records, option labels, sample accounts, nationalities, seat classes) are FIXTURE DATA: they must exist verbatim as <option>s / seed rows. When a control's values are described but not listed, offer a broad standard set.
"""

UI_CONTRACT_SESSION = """\
- Sessions: after register/login navigate to `/`, show the exact username in one element and a "Sign out" link; the session survives reload. Failed login/registration shows one generic error, keeps the anonymous header, creates nothing.
"""

UI_CONTRACT = UI_CONTRACT_CORE + UI_CONTRACT_DATA + UI_CONTRACT_SESSION  # full set (multi-node tasks)

PERFORMANCE_CONTRACT = """\
Performance & robustness (the grader is a slow container, tests run in parallel, EACH TEST HAS A 10 s BUDGET including reloads):
- The grader CPU is 5–10x slower than a laptop and runs 4 browsers at once, so budget CPU per request at 30 ms: any password hashing must cost a few milliseconds per call (a low-cost KDF parameter or a single digest), never a default-cost KDF or a native hashing module; keep the JSON store small and rewrite it only on mutation.
- Session cookie: HttpOnly; Path=/; SameSite=Lax; Max-Age at least 7 days; NO `Secure`, NO `Domain` attribute (tests run on http://127.0.0.1). Render every page server-side from the cookie (signed-in header, username) so a page needs NO XHR after load; keep pages tiny (one small inline script, no separate JS bundles) — the grader's browsers are slow and memory-starved.
- Persistence: the in-memory store is the single source of truth; never re-read the JSON file per request. Mutations update memory first and then write the whole file synchronously (writeFileSync to a temp file, then rename) — never an async read-modify-write, because the grader runs 2–4 test files in parallel against ONE backend and a concurrent register/login pair must never lose a user. No setTimeout delays, polling, service workers, beforeunload handlers, or debounced writes.
"""

ARCHITECTURE_CONTRACT = """\
Architecture (the runner depends on this EXACT layout; violation = 0 score):
- frontend/ — package.json with a working `npm run build` that produces frontend/dist/ (a plain HTML/CSS/JS app plus a tiny Node copy script is ideal; no TypeScript, no framework needed).
- backend/  — Node.js, package.json with `npm run start`, ZERO npm dependencies: `http.createServer` + a hand-written router, `fs`, `path`, `url`, `crypto` only. It reads PORT (default {port}), serves frontend/dist/ at `/` and JSON APIs under /api/. Persistence is a JSON file (backend/data/db.json) loaded at startup and rewritten on every mutation. Never better-sqlite3/sqlite3/bcrypt or any native module.
- Crash safety: the process must never exit on a request. Wrap every request handler in try/catch (respond 500 JSON), return 404 for unknown paths and missing static files (browsers request /favicon.ico — an unhandled ENOENT there kills the server and fails every test), and register process.on('uncaughtException') / process.on('unhandledRejection') handlers that log and keep serving.
- If a package is truly unavoidable, install it only with `npm install --registry=https://registry.npmmirror.com <pkg>` and write `registry=https://registry.npmmirror.com` into that folder's .npmrc.
"""

VERIFY_FULL = """\
Verify briefly before you finish — the harness runs the official acceptance tests for this node right after your turn and hands you the failures, so do not build your own test suite: `npm run build` in frontend/, start the backend with `ARC_EXTRA_PORTS=0 PORT={smoke} npm start`, one curl per new endpoint (one success, one error case), stop the server.
"""

VERIFY_MINIMAL = """\
You have no shell in this turn — the harness runs `npm run build`, starts the backend and runs the official Playwright spec right after your turn and hands you any failure. Tool budget for this turn: at most 8 write_file/edit_file calls (one backend file backend/server.js plus at most 4 frontend files; write each file once, complete) and at most 2 read_file calls. Group the write_file calls into as few responses as possible — small files together, but a large file (more than ~150 lines) alone in its own response — then finish with a one-line summary; every extra round trip resends the whole context and is billed, and an oversized response gets truncated and loses everything in it. Do not list directories or re-read files you just wrote; the file listing above is authoritative. Double-check syntax mentally before writing: a build or start failure costs a repair round.
"""

PORT_RULES = """\
Ports: run your own smoke servers ONLY with `ARC_EXTRA_PORTS=0 PORT={smoke} npm start` (port {smoke}). NEVER bind port {port} — the runner watches it and terminates the run. Stop every server you started before you finish. Do not run git; the harness commits.
"""

SKELETON_PROMPT = """\
Build the skeleton of a full-stack web application in the current working directory. The requirement tree is at {req_dir} (skim it; individual features come in later turns).

""" + ARCHITECTURE_CONTRACT + """
{tests}
Steps: create frontend/ and backend/ as specified with a home page and a health endpoint, seed the JSON store, run `npm run build` in frontend/, start the backend with `ARC_EXTRA_PORTS=0 PORT={smoke} npm start`, `curl http://127.0.0.1:{smoke}/` to confirm the page is served, then stop it.
""" + PORT_RULES

NUDGE_PROMPT = """\
You ended your last turn before creating any files. Stop analysing. In your very next actions CREATE the project files with your file-writing tools: frontend/package.json (build script), the frontend page sources, backend/package.json (start script) and the backend server with the JSON store and seed data. Do not describe the plan — write the files now.\
"""

DESIGN_PROMPT = """\
Design — do NOT implement yet — requirement node {node_id} of the web application in the current directory.

{node_spec}
{ancestors}
{tests}
Read the acceptance spec files for this node in full and the existing code they will exercise. Then write ONE JSON object (at most 80 lines) to the file .arc/design/{node_id}.json AND repeat it in your reply inside a ```json fence. Shape:
{{"routes": [{{"method": "POST", "path": "/api/...", "request": {{}}, "response": {{}}, "errors": []}}],
 "pages": [{{"path": "/...", "elements": [{{"role": "textbox|button|link|combobox|checkbox|radio|alert", "name": "exact accessible name", "notes": ""}}]}}],
 "data_model": {{"collection": {{"field": "type"}}}},
 "files": ["backend/server.js", "frontend/src/..."],
 "notes": "validation rules, session handling, seed data, performance decisions"}}
Copy every accessible name verbatim from the specs. This is a reading turn: use only file reading, listing and grep — no builds, servers, curl or other shell commands — and do not create or modify any other file.\
"""

NODE_PROMPT = """\
{preamble}
{node_spec}
{design}{ancestors}
{tests}
{ui}{performance}
{verify}
""" + PORT_RULES

NODE_PREAMBLE_EXTEND = """\
Implement requirement node {node_id} in the existing application (frontend/ built by `npm run build` into frontend/dist/; zero-dependency Node backend in backend/, `npm start`, PORT env var). Extend the app; do not rewrite or break existing features.
"""

NODE_PREAMBLE_CREATE = """\
Build a full-stack web application in the current working directory that implements requirement node {node_id} (the whole requirement tree is at {req_dir}; further nodes, if any, come in later turns — leave room for them but implement only this one).

""" + ARCHITECTURE_CONTRACT + """
Mandatory files (all in this turn): frontend/package.json (with the `build` script), the frontend page sources plus the tiny build script that fills frontend/dist/, backend/package.json (with the `start` script, empty dependencies) and backend/server.js.
"""


INLINE_DESIGN_NOTE = """\
Before writing code, write your design for this node as ONE JSON object to .arc/design/{node_id}.json ({{"routes": [...], "pages": [{{"path", "elements": [{{"role", "name"}}]}}], "data_model": {{}}, "files": [...], "notes": ""}}; accessible names copied verbatim from the specs), then implement it.
"""

EVOLUTION_NOTE = """\
This is an EXISTING application that already passed its previous acceptance tests. Current sources:
{listing}
Read the files you need before changing them, keep every existing route, label and behaviour intact, and change only what this node requires.
"""

REPAIR_PROMPT = """\
The official acceptance tests for requirement node {node_id} just ran against your app: {passed}/{total} passed. Failing tests (Feature / where it failed / what was observed / the last steps before failure):
{failures}
{corrections}{slow}{sources}
Fix frontend/ and/or backend/ so these tests pass without breaking the passing ones. You have about 10 requests: in the FIRST response read at most two files (only the ones you will change), in the SECOND response emit every edit_file/write_file call together, then finish — do not read more files afterwards. No shell commands. The harness rebuilds and re-runs the official tests right after your turn. The spec files are read-only ground truth.
""" + PORT_RULES

FINAL_CHECK_PROMPT = """\
Final end-to-end check of the web application in the current directory:
1. `npm run build` in frontend/ — fix any error.
2. Kill leftover servers, start the backend with `ARC_EXTRA_PORTS=0 PORT={smoke} npm start`, confirm `curl http://127.0.0.1:{smoke}/` serves the app and every API endpoint answers (success and error cases).
3. Audit every page against the contracts below and fix violations; run a mechanical strict-mode check: for each value the pages echo, count the elements containing it (`curl -s <page> | grep -o '<value>' | wc -l` for server-rendered pages, or read the render code) — the count must be 1.
{tests}
{ui}{performance}
""" + PORT_RULES

REHEARSAL_REPAIR_PROMPT = """\
The app failed the pre-grading startup rehearsal. The runner executes exactly:
1. cd frontend && npm install && npm run build   (must exit 0)
2. cd backend && npm install && npm start        (must bind PORT and stay up)
Rehearsal error:
{error}
Fix the project so this sequence works (typical causes: a require() path that does not match a real file, a file referenced but never written, a startup syntax error, a dependency missing from package.json). Verify: build the frontend, start the backend with `ARC_EXTRA_PORTS=0 PORT={smoke} npm start`, confirm it binds, stop it. Never bind {port}. Write the fix now.\
"""

ACCEPTANCE_TESTS_PROMPT = """\
OFFICIAL ACCEPTANCE TESTS (ground truth; when prose and spec disagree, the spec wins) live under {tests_dir}. Files: {files}. They define routes, hrefs, accessible names, option labels, exact texts, error wording and action order. Never modify, copy or delete them.
"""

INLINE_SPEC_HEADER = """\
The spec files are quoted below in full — do NOT spend tool calls reading them or the requirement again:
"""


def inline_spec_text(tests_dir: Path, files: list[str], max_chars: int) -> str:
    """Quote spec + helper files into the prompt (bounded). Each read_file the
    model would otherwise issue is a full-context round trip (~11k tokens)."""
    parts = []
    total = 0
    for rel in files:
        path = tests_dir / rel
        try:
            text = path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        if total + len(text) > max_chars:
            return ""  # too big to inline; let the model read selectively
        total += len(text)
        parts.append(f"--- {rel} ---\n{text.rstrip()}\n")
    return INLINE_SPEC_HEADER + "".join(parts) if parts else ""


def locate_acceptance_tests(tree: dict, bundle_dir: Path) -> Path | None:
    """ARCBENCH_TESTS_DIR, then the runner's /workspace/tests, then the public
    specs shipped in the bundle (matched by requirement root name)."""
    candidates: list[Path] = []
    env_dir = os.environ.get("ARCBENCH_TESTS_DIR")
    if env_dir:
        candidates.append(Path(env_dir))
    candidates.append(Path("/workspace/tests"))
    bundled = bundle_dir / "public-tests"
    manifest = bundled / "manifest.json"
    if manifest.is_file():
        try:
            mapping = json.loads(manifest.read_text(encoding="utf-8"))
            root_name = str(tree.get("name", "")).strip()
            for req_id, title in mapping.items():
                if str(title).strip() == root_name and (bundled / req_id).is_dir():
                    candidates.append(bundled / req_id)
        except Exception as exc:  # noqa: BLE001
            log(f"[tests] manifest unreadable: {exc}")
    for cand in candidates:
        try:
            if cand.is_dir() and any(cand.rglob("*.spec.ts")):
                return cand.resolve()
            log(f"[tests] candidate {cand}: {'no *.spec.ts' if cand.is_dir() else 'absent'}")
        except Exception as exc:  # noqa: BLE001
            log(f"[tests] candidate {cand} unreadable: {exc}")
    return None


def spec_base_ports(tests_dir: Path | None) -> list[int]:
    """Ports the specs hard-code as their default base URL (e.g. 3301)."""
    if not tests_dir:
        return []
    ports: set[int] = set()
    for path in tests_dir.rglob("*.ts"):
        try:
            text = path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        for m in re.finditer(r"https?://(?:127\.0\.0\.1|localhost):(\d{2,5})", text):
            ports.add(int(m.group(1)))
    return sorted(ports)


def acceptance_tests_prompt(tests_dir: Path | None, web_port: int, smoke_port: int,
                            files: list[str] | None = None, inline: bool = False) -> str:
    if not tests_dir:
        return ""
    if files is None:
        files = sorted(str(p.relative_to(tests_dir)) for p in tests_dir.rglob("*.ts"))
    text = ACCEPTANCE_TESTS_PROMPT.format(tests_dir=tests_dir, files=", ".join(files[:40]) or "(none)")
    if inline:
        text += inline_spec_text(tests_dir, files, int(os.environ.get("OCTOS_ARC_INLINE_SPEC_CHARS", "24000")))
    extra = [p for p in spec_base_ports(tests_dir) if p != web_port]
    if extra:
        ports = ", ".join(map(str, extra))
        text += (f"PORT CONTRACT (mandatory): the specs default to port(s) {ports} while the grader starts the "
                 f"backend with PORT={web_port}. Serve the identical app on BOTH the PORT value and port(s) {ports}: "
                 f"create a SEPARATE http.createServer(handler) for each port (one Server can listen only once — "
                 f"calling listen() twice throws ERR_SERVER_ALREADY_LISTEN and the process dies), binding the extra "
                 f"port(s) only when process.env.ARC_EXTRA_PORTS is not '0'. The grader sets ONLY PORT, so the "
                 f"extra port(s) ARE bound during grading.\n")
    return text


# ---------------------------------------------------------------- flow

class Flow:
    def __init__(self, args, output_dir: Path, req_dir: Path) -> None:
        self.args = args
        self.output_dir = output_dir
        self.req_dir = req_dir
        self.web_port = args.web_port
        self.smoke_port = int(os.environ.get("OCTOS_SMOKE_PORT", "3100"))
        if self.smoke_port == self.web_port:
            self.smoke_port += 1
        self.node_timeout = int(os.environ.get("OCTOS_NODE_TIMEOUT", "1200"))
        self.design_timeout = int(os.environ.get("OCTOS_DESIGN_TIMEOUT", "420"))
        self.budget = int(os.environ["OCTOS_TIME_BUDGET"]) if os.environ.get("OCTOS_TIME_BUDGET") else 3600
        self.budget_explicit = bool(os.environ.get("OCTOS_TIME_BUDGET"))
        # keep-local-3 (workflow C): with 480 s/node, 16 of 17 implement/repair
        # turns were cut at 283 s; Web nodes need 10-20 min of implementation.
        self.seconds_per_node = int(os.environ.get("OCTOS_SECONDS_PER_NODE", "1500"))
        self.min_repair_seconds = int(os.environ.get("OCTOS_MIN_REPAIR_SECONDS", "300"))
        self.node_budget_cap = int(os.environ.get("OCTOS_NODE_TIME_BUDGET", "1500"))
        self.repair_rounds = int(os.environ.get("OCTOS_REPAIR_ROUNDS", "5"))
        self.repair_rounds_explicit = bool(os.environ.get("OCTOS_REPAIR_ROUNDS"))
        # Run-wide cost guard. Defaults scale with the tree and sit ~3x above a normal run
        # (calibration: cloud keep 2224a9013528, 32 nodes, PASSED 32/32, 9,038 s, ¥16.58 ≈ 26M
        # platform tokens ≈ 0.8M tokens and ~1.1 turns per node), so they never truncate a
        # healthy run; they only stop repair loops that have gone pathological. Explicit env
        # values override (0 = off). OCTOS_ARC_MAX_TOTAL_TOKENS_ABS is the optional absolute
        # ceiling for a per-run spend rule (e.g. ¥50 ≈ 75M tokens at the observed ¥0.63/M).
        self.max_total_tokens = int(os.environ.get("OCTOS_ARC_MAX_TOTAL_TOKENS", "-1"))
        self.max_turns = int(os.environ.get("OCTOS_ARC_MAX_TURNS", "-1"))
        self.max_total_tokens_abs = int(os.environ.get("OCTOS_ARC_MAX_TOTAL_TOKENS_ABS", "0"))
        self.turn_count = 0
        self._wound_down_logged = False
        self.design_enabled = os.environ.get("OCTOS_DESIGN_TURN", "1") != "0"
        self.design_min_nodes = int(os.environ.get("OCTOS_DESIGN_MIN_NODES", "3"))
        self.skeleton_min_nodes = int(os.environ.get("OCTOS_SKELETON_MIN_NODES", "3"))
        self.small_task_nodes = int(os.environ.get("OCTOS_SMALL_TASK_NODES", "2"))
        # "separate": own read-only turn before implementing; "inline": the
        # implement turn writes .arc/design/<node>.json first, then codes.
        self.design_mode = os.environ.get("OCTOS_DESIGN_MODE", "inline")
        self.implement_fraction = float(os.environ.get("OCTOS_IMPLEMENT_FRACTION", "0.6"))
        self.alias_states = os.environ.get("OCTOS_ARC_ALIAS_SPEC_IDS", "1") != "0"
        self.perf_contract = os.environ.get("OCTOS_PERF_CONTRACT", "1") != "0"
        self.guard_enabled = os.environ.get("OCTOS_GUARD", "1") != "0"
        self.t_start = time.time()
        self.runtime = None
        self.events = None
        self.driver: OctosDriver | None = None
        self.tests_dir: Path | None = None
        self.spec_map: dict = {None: []}
        self.probe_summaries: dict = {}
        self.probe_count = 0  # nodes actually probed against the existing app
        self.aliases: dict[str, str] = {}
        self.runner: AcceptanceRunner | None = None
        self.designs: dict[str, dict] = {}
        self.test_verdict: dict[str, bool | None] = {}
        self.impl_failed: list[str] = []
        self.pending_corrections: list[str] = []
        self.evolution = False
        self.folder_children: dict[str, list[str]] = {}

    # -- helpers ----------------------------------------------------------
    def wound_down(self) -> bool:
        """True once the run has spent its token or turn allowance: no more repair
        turns, remaining nodes get one implement turn each, one final suite, done."""
        proxy = getattr(self, "llm_proxy", None)
        tokens = proxy.total_tokens if proxy is not None else 0
        over = (self.max_total_tokens > 0 and tokens >= self.max_total_tokens) or \
               (self.max_turns > 0 and self.turn_count >= self.max_turns) or \
               (self.max_total_tokens_abs > 0 and tokens >= self.max_total_tokens_abs)
        if over and not self._wound_down_logged:
            self._wound_down_logged = True
            log(f"[guard] cost guard tripped: {tokens} billable tokens, {self.turn_count} turns "
                f"(limits {self.max_total_tokens} / {self.max_turns} / abs {self.max_total_tokens_abs}); no further repair turns")
        return bool(over)

    def remaining(self) -> float:
        return self.budget - (time.time() - self.t_start)

    def time_up(self) -> bool:
        return self.remaining() <= 0

    def mark(self, kind: str, node_id: str, message: str | None = None) -> None:
        fn = getattr(self.events, f"mark_{kind}")
        fn(node_id, message)
        if self.alias_states:
            for alias, target in self.aliases.items():
                if target == node_id:
                    fn(alias, message)

    def protected_prefixes(self) -> list[str]:
        prefixes = [".arc/", str(self.output_dir / ".arc"), "requirements/", str(self.req_dir)]
        if self.tests_dir:
            prefixes.append(str(self.tests_dir))
        return prefixes

    def sources_text(self) -> str:
        limit = int(os.environ.get("OCTOS_ARC_INLINE_SOURCE_CHARS", "40000"))
        return inline_sources(self.output_dir, limit) + "\n" if limit > 0 else ""

    def corrections_text(self) -> str:
        if not self.pending_corrections:
            return ""
        text = "Corrections from the harness:\n" + "\n".join(f"- {c}" for c in self.pending_corrections) + "\n"
        self.pending_corrections = []
        return text

    def turn(self, prompt: str, timeout: int, label: str, expect_verification: bool = True,
             request_budget: int | None = None) -> tuple[bool, str]:
        monitor = TurnMonitor(self.protected_prefixes(), expect_verification=expect_verification,
                              allowed_prefixes=[".arc/design/", str(self.output_dir / ".arc" / "design")])
        proxy = getattr(self, "llm_proxy", None)
        if proxy is not None:
            # Per-turn reasoning: OCTOS_ARC_IMPLEMENT_REASONING (e.g. "none") applies
            # to first implement turns of small tasks; rewrite/repair keep the base mode.
            base_mode = getattr(self, "base_reasoning_mode", proxy.mode)
            impl_mode = os.environ.get("OCTOS_ARC_IMPLEMENT_REASONING", "")  # auto already gives "none" to 1-node tasks
            is_implement = label.endswith(" implement") or label.startswith("skeleton")
            proxy.mode = impl_mode if (impl_mode and is_implement and self.minimal_mode(getattr(self, "n_nodes", 99))) else base_mode
            if request_budget is None:
                request_budget = int(os.environ.get("OCTOS_ARC_REPAIR_REQUESTS", "10")) if "repair" in label else \
                    int(os.environ.get("OCTOS_ARC_IMPLEMENT_REQUESTS", "20" if self.minimal_mode(getattr(self, "n_nodes", 99)) else "0"))
            proxy.begin_turn(request_budget)
        t0 = time.time()
        self.turn_count += 1
        ok, text = self.driver.run(prompt, max(60, int(timeout)), monitor)
        log(f"[flow] {label} {'ok' if ok else 'FAILED'} in {time.time()-t0:.0f}s "
            f"(tools={monitor.tool_calls} wrote={monitor.wrote_files} verified={monitor.verified}): {text[-240:]!r}")
        if proxy is not None and proxy.turn_budget and proxy.turn_requests > proxy.turn_budget:
            log(f"[guard] {label}: request budget {proxy.turn_budget} hit; turn forced to finish")
        for c in monitor.corrections():
            log(f"[guard] {label}: {c[:160]}")
            if self.guard_enabled:
                self.pending_corrections.append(c)
        restored = self.restore_protected()
        if restored:
            self.pending_corrections.append(
                "You changed official test/requirement files; the harness restored them: "
                + ", ".join(restored[:5]) + ". They are read-only ground truth — fix the app instead.")
        return ok, text

    def perf_text(self) -> str:
        return PERFORMANCE_CONTRACT if self.perf_contract and self.needs_session else ""

    def classify_tree(self, tree: dict) -> None:
        """Keyword-gate the optional contract blocks so a counter never reads
        session/hashing rules; the hidden specs only test what the tree says."""
        text = json.dumps(tree, ensure_ascii=False).lower()
        self.needs_session = bool(re.search(r"login|log in|sign in|password|session|register|注册|登录|密码|会话", text))
        self.needs_data = bool(re.search(r"seed|published|fixture|option|select|dropdown|nationalit|车次|train|选项|下拉|预置", text))

    def ui_contract(self) -> str:
        blocks = [UI_CONTRACT_CORE]
        if getattr(self, "needs_data", True):
            blocks.append(UI_CONTRACT_DATA)
        if getattr(self, "needs_session", True):
            blocks.append(UI_CONTRACT_SESSION)
        return "".join(blocks)

    SHELL_TOOLS = {"bash", "shell", "exec_command"}

    def minimal_mode(self, total_nodes: int) -> bool:
        mode = os.environ.get("OCTOS_VERIFY_MODE", "auto")
        return mode == "minimal" or (mode != "full" and total_nodes <= self.small_task_nodes)

    def verify_text(self, total_nodes: int) -> str:
        minimal = self.minimal_mode(total_nodes)
        # Prompt budgets alone are ignored often enough (v9-tb-a: 41 tool calls
        # incl. servers in a "no shell" repair turn); in minimal mode the proxy
        # removes the shell tools so commands are impossible, the harness builds.
        proxy = getattr(self, "llm_proxy", None)
        if proxy is not None and os.environ.get("OCTOS_ARC_DROP_SHELL", "1") != "0":
            proxy.extra_drop_tools = set(self.SHELL_TOOLS) if minimal else set()
        return VERIFY_MINIMAL if minimal else VERIFY_FULL.format(smoke=self.smoke_port)

    def codegen_mode(self) -> bool:
        """One-request generation for one-node tasks (OCTOS_ARC_CODEGEN=0 disables)."""
        return (os.environ.get("OCTOS_ARC_CODEGEN", "1") != "0" and getattr(self, "llm_proxy", None) is not None
                and not getattr(self, "codegen_blocked", False)
                and getattr(self, "n_nodes", 99) <= int(os.environ.get("OCTOS_ARC_CODEGEN_MAX_NODES", "2")))

    def all_specs_tiny(self, node_ids: list[str]) -> bool:
        """True when every node that has specs falls in the tiny tier (and at least one does)."""
        sizes = [len(self.spec_bodies(n)) for n in node_ids if self.spec_map.get(n)]
        return bool(sizes) and all(self.tiny_mode(n) for n in sizes)

    def maybe_probe(self, node_ids: list[str]) -> None:
        """Endpoint probe policy: none in dry runs; none when the whole task is tiny-tier
        (the first real request is the probe — a failure there is diagnosed by the normal
        turn error path); otherwise the token-free GET /models probe with a minimal fallback."""
        if os.environ.get("OCTOS_ARC_DRYRUN") == "1":
            log("[probe] skipped (OCTOS_ARC_DRYRUN=1)")
        elif self.all_specs_tiny(node_ids):
            log("[probe] skipped (tiny-tier task: the first real request doubles as the probe)")
        else:
            probe_endpoint()

    def tiny_mode(self, spec_chars: int) -> bool:
        threshold = int(os.environ.get("OCTOS_ARC_TINY_SPEC_CHARS", "1500"))
        return os.environ.get("OCTOS_ARC_TINY", "1") != "0" and 0 < spec_chars < threshold

    def tiny_turn(self, node_id: str, specs: list[str], timeout: int) -> bool:
        """Tiny-spec tier: harness writes the manifests and a fixed static server, the
        model returns one index.html for the spec's statements. Returns True only when
        the node's specs pass right away; otherwise the caller falls back to the compact tier."""
        write_codegen_manifests(self.output_dir)
        server = self.output_dir / "backend" / "server.js"
        if not server.exists():
            server.parent.mkdir(parents=True, exist_ok=True)
            extra = [p for p in spec_base_ports(self.tests_dir) if p != self.web_port]
            server.write_text(TINY_SERVER_JS.format(port=self.web_port, extra_ports=json.dumps(extra)), encoding="utf-8")
        spec = compact_spec_lines(self.spec_bodies(node_id))
        page = self.output_dir / "frontend" / "src" / "index.html"
        if page.is_file():
            prompt = TINY_PROMPT_EVOLUTION.format(page=page.read_text(encoding="utf-8", errors="replace").strip(), spec=spec)
        else:
            prompt = TINY_PROMPT.format(spec=spec)
        ok, _ = self.codegen_turn(prompt, timeout, f"{node_id} implement (tiny)", spec_chars=len(spec),
                                  system=TINY_SYSTEM, format_instructions="", raw_target="frontend/src/index.html")
        if not ok or not page.is_file() or self.runner is None or not specs:
            log(f"[flow] {node_id}: tiny tier produced no page; compact tier next")
            return False
        summary = self.run_specs(specs)
        passed = (not summary.error) and summary.total and summary.passed == summary.total
        log(f"[flow] {node_id}: tiny tier {'passed' if passed else 'failed'} its specs"
            f" ({summary.passed}/{summary.total})" if not summary.error else f"[flow] {node_id}: tiny tier could not run specs")
        return bool(passed)

    def codegen_reasoning(self, spec_chars: int) -> str | None:
        """Reasoning effort for a codegen turn, derived from the size of the spec it
        must satisfy (OCTOS_ARC_CODEGEN_REASONING_CHARS, default 5000): small specs are
        generated correctly without reasoning; large ones keep the base mode."""
        if os.environ.get("OCTOS_ARC_REASONING", "auto") != "auto":
            return None
        threshold = int(os.environ.get("OCTOS_ARC_CODEGEN_REASONING_CHARS", "5000"))
        return "none" if spec_chars and spec_chars < threshold else None

    def codegen_turn(self, prompt: str, timeout: int, label: str, spec_chars: int = 0,
                     system: str = CODEGEN_SYSTEM, format_instructions: str = FORMAT_INSTRUCTIONS,
                     raw_target: str | None = None) -> tuple[bool, str]:
        """Run a tool-less turn; parse and write the file blocks from the reply.
        `raw_target`: when the reply is a bare HTML document (tiny tier), write it there."""
        proxy = self.llm_proxy
        proxy.no_tools = True
        proxy.system_override = system
        mode_override = self.codegen_reasoning(spec_chars)
        saved_base = getattr(self, "base_reasoning_mode", proxy.mode)
        if mode_override:
            self.base_reasoning_mode = mode_override
        try:
            ok, text = self.turn((prompt + "\n" + format_instructions) if format_instructions else prompt, timeout, label,
                                 expect_verification=False,
                                 request_budget=int(os.environ.get("OCTOS_ARC_CODEGEN_REQUESTS", "3")))
        finally:
            proxy.no_tools = False
            proxy.system_override = None
            self.base_reasoning_mode = saved_base
        files = parse_file_blocks(text) if ok else {}
        if ok and not files and raw_target:
            html = strip_code_fences(text)
            if looks_like_markup(html):
                files = {raw_target: html}
        if files:
            written = write_files(self.output_dir, files)
            log(f"[codegen] {label}: wrote {len(written)} file(s): {written[:8]}")
            deduped = dedupe_nav_links(self.output_dir)
            if deduped:
                log(f"[codegen] {label}: removed static nav links duplicating the NAV placeholder in {deduped}")
            return True, text
        if ok:
            log(f"[codegen] {label}: reply contained no file blocks")
            return False, "codegen reply contained no <<<FILE>>> blocks"
        return ok, text

    def codegen_ports_clause(self) -> str:
        extra = [p for p in spec_base_ports(self.tests_dir) if p != self.web_port]
        if not extra:
            return ""
        ports = ", ".join(map(str, extra))
        return (f" The tests default to port(s) {ports} while the grader sets only PORT: ALSO listen on {ports} with a "
                f"separate http.createServer(handler) (same handler) unless process.env.ARC_EXTRA_PORTS === '0'.")

    def spec_bodies(self, node_id: str | None) -> str:
        """Just the spec file contents for a node (codegen prompts)."""
        if not self.tests_dir:
            return "(none)"
        files = list(self.spec_map.get(node_id) or [])
        files += sorted(str(p.relative_to(self.tests_dir)) for p in self.tests_dir.rglob("*.ts")
                        if not p.name.endswith(".spec.ts") and str(p.relative_to(self.tests_dir)) not in files)
        parts = []
        for rel in files:
            try:
                text = (self.tests_dir / rel).read_text(encoding="utf-8", errors="replace").strip()
            except OSError:
                continue
            parts.append(text if len(files) == 1 else f"--- {rel} ---\n{text}")
        return "\n".join(parts) or "(none)"

    def tests_prompt_for(self, node_id: str | None, skeleton: bool = False) -> str:
        if not self.tests_dir:
            return ""
        if skeleton:
            support = sorted(str(p.relative_to(self.tests_dir)) for p in self.tests_dir.rglob("*.ts")
                             if not p.name.endswith(".spec.ts"))
            n_specs = len(list(self.tests_dir.rglob("*.spec.ts")))
            return (f"The official Playwright specs ({n_specs} files) live under {self.tests_dir}; each later turn "
                    f"receives the spec files for its own node. In THIS turn read only the shared helpers "
                    f"({', '.join(support[:10]) or 'none'}) and at most two spec files to learn the base URL, "
                    f"navigation and header conventions; do not implement the features yet.\n"
                    + acceptance_tests_prompt(self.tests_dir, self.web_port, self.smoke_port, []).split("\n", 1)[-1])
        files = list(self.spec_map.get(node_id) or [])
        support = sorted(str(p.relative_to(self.tests_dir)) for p in self.tests_dir.rglob("*.ts")
                         if not p.name.endswith(".spec.ts"))
        if not files:  # node without its own spec: show everything
            files = sorted(str(p.relative_to(self.tests_dir)) for p in self.tests_dir.rglob("*.spec.ts"))
        return acceptance_tests_prompt(self.tests_dir, self.web_port, self.smoke_port, files + support,
                                       inline=os.environ.get("OCTOS_ARC_INLINE_SPECS", "1") != "0")

    def ancestors_text(self, node_id: str, ordered: list[dict]) -> str:
        anc = ancestors_of(node_id, ordered)
        if not anc:
            return ""
        parts = []
        for dep in anc:
            design = self.designs.get(dep)
            if design:
                slim = {k: design.get(k) for k in ("routes", "pages", "data_model") if design.get(k)}
                parts.append(f"{dep}: {json.dumps(slim, ensure_ascii=False)[:1500]}")
            else:
                parts.append(f"{dep}: implemented (see code)")
        return "Already implemented dependencies — reuse their routes/data, never break them:\n" + "\n".join(parts) + "\n"

    # -- git --------------------------------------------------------------
    def head(self) -> str | None:
        return self.runtime.git.current_head()

    def commit(self, message: str) -> bool:
        try:
            return self.runtime.git.commit(message)
        except Exception as exc:  # noqa: BLE001
            log(f"[git] commit failed: {exc}")
            return False

    def restore_app(self, sha: str) -> None:
        git = self.runtime.git
        for part in ("frontend", "backend"):
            if (self.output_dir / part).exists():
                git.run(["checkout", sha, "--", part], check=False)
        git.run(["clean", "-fd", "-e", "node_modules", "-e", "dist", "--", "frontend", "backend"], check=False)
        log(f"[flow] restored frontend/ and backend/ to best commit {sha[:8]}")

    # -- acceptance -------------------------------------------------------
    def setup_playwright(self) -> None:
        """Prefer the Playwright already on the machine (the runner image ships
        one). A private install is the last resort and never touches shared
        state: own npm cache, own browser dir, pinned version, removed at exit.
        Run da9a64b32c09: an unisolated install made the platform's own
        `npx playwright test` resolve a different version whose chromium build
        was missing, and every graded test failed."""
        if not self.tests_dir:
            return
        env_extra: dict = {}
        root = find_playwright_root(playwright_candidates(BUNDLE_DIR, self.tests_dir, self.output_dir))
        if root is None:
            root = find_playwright_by_search(log)
        if root is None and os.environ.get("OCTOS_ARC_INSTALL_PLAYWRIGHT", "1") != "0":
            version = playwright_version_hint(self.tests_dir)
            log(f"[acceptance] no preinstalled Playwright found; private install of @playwright/test@{version}")
            self.private_playwright = Path(tempfile.mkdtemp(prefix="octos-arc-playwright-"))
            installed = ensure_playwright(self.private_playwright, log, version=version)
            if installed:
                root, env_extra = installed
        if root is None:
            log("[acceptance] Playwright unavailable; nodes will be judged by the final check only")
            return
        limit = container_memory_limit()
        self.mem_limit = limit
        workers = workers_for_memory(limit, int(os.environ.get("OCTOS_ARC_TEST_WORKERS", "2")))
        self.runner = AcceptanceRunner(root, self.tests_dir, acceptance_work_dir(root), log,
                                       timeout_ms=int(os.environ.get("OCTOS_ARC_TEST_TIMEOUT_MS", "10000")),
                                       workers=workers, env_extra=env_extra)
        log(f"[acceptance] using Playwright at {root}; workers={workers}"
            + (f" (container memory limit {limit // (1024 * 1024)} MiB)" if limit else ""))

    def snapshot_protected(self) -> None:
        """Copy the official tests dir (and requirements) so any edit the model
        sneaks past the hook (e.g. via a shell redirect) is undone after the
        turn — the platform grades with THESE files."""
        self.protected_snapshots = []
        for live in (self.tests_dir, self.req_dir):
            if not live or not live.is_dir():
                continue
            snap = Path(tempfile.mkdtemp(prefix="octos-protected-"))
            shutil.copytree(live, snap / "tree", ignore=shutil.ignore_patterns("node_modules"))
            self.protected_snapshots.append((live, snap / "tree", tree_digest(live)))

    def restore_protected(self) -> list[str]:
        fixed_all: list[str] = []
        for live, snap, digest in getattr(self, "protected_snapshots", []):
            try:
                fixed = restore_tree(live, snap, digest)
            except OSError as exc:
                log(f"[guard] could not restore {live}: {exc}")
                continue
            if fixed:
                log(f"[guard] restored {len(fixed)} protected file(s) under {live}: {fixed[:5]}")
                fixed_all.extend(f"{live}/{rel}" for rel in fixed)
        return fixed_all

    def start_llm_proxy(self) -> None:
        """Front the model endpoint with llm_proxy so DeepSeek reasoning is
        capped (`OCTOS_ARC_REASONING`: low (default) | medium | high | none |
        passthrough) and exact per-request usage lands in .arc/llm-usage.jsonl."""
        mode = os.environ.get("OCTOS_ARC_REASONING", "auto")
        if mode == "auto":
            # Thinking off is safe for one-node builds and one-node evolutions
            # (v10: Counter/Dice/Evolution all pass, completion 0.5-1.9k tokens)
            # but TB repairs without thinking looped 22 calls with no write.
            mode = "none" if getattr(self, "nodes_to_implement", 2) <= 1 else "low"
        upstream = os.environ.get("OPENAI_BASE_URL", "")
        if mode == "passthrough" or not upstream.startswith("http"):
            return
        try:
            dump = (self.output_dir / ".arc" / "llm-requests") if os.environ.get("OCTOS_ARC_PROXY_DUMP") == "1" else None
            self.llm_proxy = LlmProxy(upstream, mode, self.output_dir / ".arc" / "llm-usage.jsonl", dump_dir=dump,
                                      destream=os.environ.get("OCTOS_ARC_DESTREAM", "1") != "0",
                                      trim=os.environ.get("OCTOS_ARC_TRIM_PROMPT", "1") != "0",
                                      min_max_tokens=int(os.environ.get("OCTOS_ARC_MAX_TOKENS", "32768"))).start()
        except OSError as exc:
            log(f"[proxy] could not start local LLM proxy ({exc}); using the endpoint directly")
            return
        self.base_reasoning_mode = mode
        os.environ["OPENAI_BASE_URL"] = self.llm_proxy.base_url
        log(f"[proxy] LLM requests via {self.llm_proxy.base_url} -> {upstream} (reasoning={mode}, "
            f"destream={'on' if self.llm_proxy.destream else 'off'}, trim={'on' if self.llm_proxy.trim else 'off'})")

    def stop_llm_proxy(self) -> None:
        proxy = getattr(self, "llm_proxy", None)
        if proxy:
            proxy.stop()
        self.log_usage_summary()

    def log_usage_summary(self) -> None:
        """Provider-reported usage totals (same numbers the platform bills on),
        printed so the runner log carries them even when .arc/ is not exported."""
        path = self.output_dir / ".arc" / "llm-usage.jsonl"
        if not path.is_file():
            return
        tot = {"requests": 0, "prompt_tokens": 0, "completion_tokens": 0, "reasoning_tokens": 0,
               "prompt_cache_hit_tokens": 0, "total_tokens": 0, "request_bytes": 0, "response_bytes": 0,
               "sse_chunks": 0}
        for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
            try:
                rec = json.loads(line)
            except json.JSONDecodeError:
                continue
            tot["requests"] += 1
            for k in list(tot)[1:]:
                tot[k] += int(rec.get(k) or 0)
        log(f"[usage] provider totals: {json.dumps(tot)}")

    def cleanup_playwright(self) -> None:
        private = getattr(self, "private_playwright", None)
        if private and Path(private).exists():
            shutil.rmtree(private, ignore_errors=True)
            log(f"[acceptance] removed private Playwright install {private}")

    def app_server(self, grader_like: bool) -> AppServer:
        return AppServer(self.output_dir, self.smoke_port, log, grader_like=grader_like,
                         extra_ports=[p for p in spec_base_ports(self.tests_dir) if p != self.web_port])

    def run_specs(self, specs: list[str], workers: int | None = None, grader_like: bool = False) -> RunSummary:
        """Build, start, run the specs, then undo whatever the test run mutated
        (a persisted counter at -1 would otherwise be committed as the seed).
        `grader_like` starts the backend with only PORT set, as the platform does."""
        git_run = lambda args: self.runtime.git.run(args, check=False)  # noqa: E731
        snapshot_worktree(git_run)
        server = self.app_server(grader_like)
        try:
            err = server.build()
            if err is None:
                err = server.start()
            if err is not None:
                return RunSummary(error=err)
            return self.runner.run(specs, f"http://127.0.0.1:{self.smoke_port}", workers=workers)
        finally:
            server.stop()
            restore_worktree(git_run)

    def record_tests(self, node_id: str, specs: list[str], summary: RunSummary) -> None:
        try:
            for r in summary.results:
                test_id = re.sub(r"[^A-Za-z0-9._-]+", "-", r.title)[:120]
                self.runtime.traceability.upsert_test(test_id=test_id, req_id=node_id, type="e2e",
                                                      file_path=r.file or None, passed=r.ok, emit_event=False)
        except Exception as exc:  # noqa: BLE001
            log(f"[trace] test rows not recorded: {exc}")

    def acceptance_loop(self, node_id: str, specs: list[str], deadline: float,
                        rebuild_prompt=None) -> bool | None:
        """Returns True/False for a real verdict, None when no local run happened.
        `rebuild_prompt(failures)` (optional) yields a full re-implementation
        prompt; it is used once when round 0 passes nothing — rewriting beats
        patching a structurally broken first attempt (v13-tb-a)."""
        if self.runner is None or not specs:
            return None
        best_passed, best_sha, regressions, stalls = -1, self.head(), 0, 0
        rewrite_used = False
        previous_failures = None
        self.codegen_blocked = False  # same failure twice in codegen mode -> tool mode for this node
        for attempt in range(self.repair_rounds + 1):
            summary = self.run_specs(specs)
            if summary.error and summary.killed:
                log(f"[acceptance] {node_id}: test runner killed ({summary.error[:120]}); no verdict from this round")
                return None
            if summary.error:
                log(f"[acceptance] {node_id} infrastructure error: {summary.error[:300]}")
                failures = f"- Feature: app startup\n  Failed at: build/start\n  Observation: {summary.error[:600]}\n  Steps: npm run build -> npm start"
                summary = RunSummary(passed=0, total=max(1, len(specs)))
                passed = 0
            else:
                passed = summary.passed
                failures = failure_summaries(summary)
                self.record_tests(node_id, specs, summary)
            log(f"[acceptance] {node_id} round {attempt}: {passed}/{summary.total}")
            normalized = re.sub(r"\d+", "#", failures or "")
            if normalized and normalized == previous_failures:
                # Cloud 91aaecaf31af: three codegen rounds, identical observation.
                self.codegen_blocked = True
                self.pending_corrections.append(
                    "Your last two attempts produced EXACTLY the same failure. The same logic will fail again: read the "
                    "Expected/Received values in the observation, change the approach (e.g. render the initial state in "
                    "the served HTML instead of after a fetch), and check the spec's locator against your markup.")
                log(f"[flow] {node_id}: identical failure twice; switching repairs to tool mode")
            previous_failures = normalized
            if attempt >= int(os.environ.get("OCTOS_ARC_CODEGEN_REPAIRS", "2")) and passed < summary.total \
                    and self.codegen_mode():
                # Cloud 91aaecaf31af / 5747e6bcf530: repeated codegen repairs re-emit the same files.
                # One cheap codegen repair (failure digest + quoted sources) is allowed; then tools.
                self.codegen_blocked = True
                log(f"[flow] {node_id}: codegen attempt {attempt} still failing; repairs use tool mode")
            for line in (failures or "").splitlines():
                if line.strip().startswith(("Failed at:", "Observation:")):
                    log(f"[acceptance]   {' '.join(line.strip().split())[:360]}")
            if summary.total and passed == summary.total:
                self.commit(f"{node_id} (accepted): {passed}/{summary.total} acceptance tests pass")
                return True
            if passed > best_passed:
                if best_passed >= 0:
                    self.commit(f"{node_id} (repair {attempt}): {passed}/{summary.total} pass")
                best_passed, best_sha, regressions, stalls = passed, self.head(), 0, 0
            elif passed == best_passed and attempt > 0:
                stalls += 1
                if stalls >= 2:
                    # Cloud f9f0026819f1: six rounds oscillating 4/6 <-> 3/6.
                    log(f"[flow] {node_id}: no improvement for two repairs; keeping the best state")
                    break
            elif passed < best_passed:
                regressions += 1
                if regressions >= 2 and best_sha:
                    self.restore_app(best_sha)
                    self.pending_corrections.append(
                        f"Your last two repairs made the tests worse; the harness restored frontend/ and backend/ "
                        f"to the best state ({best_passed}/{summary.total}). Start from that code.")
                    regressions = 0
            if attempt == self.repair_rounds or self.wound_down():
                break
            left = deadline - time.time()
            if left < self.min_repair_seconds or self.time_up():
                # A repair turn that starts with only a couple of minutes left
                # times out too (keep-local-3); keep the best state instead.
                log(f"[flow] {node_id}: {left:.0f}s left, below the {self.min_repair_seconds}s a repair needs; "
                    f"keeping the best state")
                break
            self.snapshot_sources(node_id, attempt)
            slow = summary.slow(int(os.environ.get("OCTOS_ARC_SLOW_MS", "3000")))
            slow_text = ("Also, these tests took over 3 s on this fast machine and will exceed the grader's "
                         "10 s budget: " + "; ".join(slow) + ". Remove the latency.\n" + self.perf_text()) if slow else ""
            if passed == 0 and rebuild_prompt is not None and not rewrite_used \
                    and os.environ.get("OCTOS_ARC_REWRITE_ON_ZERO", "1") != "0":
                rewrite_used = True
                log(f"[flow] {node_id}: nothing passed; one full rewrite turn instead of a patch")
                prompt = rebuild_prompt(failures or "(no detail)")
                if self.codegen_mode():
                    self.codegen_turn(prompt, min(self.node_timeout, left), f"{node_id} rewrite (repair {attempt + 1})",
                                      spec_chars=getattr(self, "current_spec_chars", 0))
                else:
                    self.turn(prompt, min(self.node_timeout, left), f"{node_id} rewrite (repair {attempt + 1})",
                              request_budget=int(os.environ.get("OCTOS_ARC_IMPLEMENT_REQUESTS", "20")))
                continue
            prompt = REPAIR_PROMPT.format(node_id=node_id, passed=passed, total=summary.total,
                                          failures=failures or "(no detail)", corrections=self.corrections_text(),
                                          slow=slow_text, smoke=self.smoke_port, port=self.web_port,
                                          sources=self.sources_text())
            if self.codegen_mode():
                self.codegen_turn(prompt + "\nReturn every file you change as a complete file block.",
                                  min(self.node_timeout, left), f"{node_id} repair {attempt + 1}/{self.repair_rounds}",
                                  spec_chars=getattr(self, "current_spec_chars", 0))
            else:
                self.turn(prompt, min(self.node_timeout, left), f"{node_id} repair {attempt + 1}/{self.repair_rounds}")
        if best_passed > 0 and best_sha and self.head() != best_sha:
            self.restore_app(best_sha)
            self.commit(f"{node_id}: keep best acceptance state {best_passed}")
        return False

    # -- per node ---------------------------------------------------------
    def design(self, node: dict, ordered: list[dict], deadline: float) -> dict | None:
        node_id = str(node.get("id"))
        prompt = DESIGN_PROMPT.format(node_id=node_id, node_spec=describe_node(node),
                                      ancestors=self.ancestors_text(node_id, ordered),
                                      tests=self.tests_prompt_for(node_id))
        ok, text = self.turn(prompt, min(self.design_timeout, deadline - time.time()), f"{node_id} design",
                             expect_verification=False)
        design = None
        m = re.search(r"```json\s*(\{.*?\})\s*```", text, re.S) or re.search(r"(\{.*\})", text, re.S)
        if ok and m:
            try:
                design = json.loads(m.group(1))
            except json.JSONDecodeError:
                design = None
        if not isinstance(design, dict):
            written = self.output_dir / ".arc" / "design" / f"{node_id}.json"
            if written.is_file():
                try:
                    design = json.loads(written.read_text(encoding="utf-8"))
                    log(f"[flow] {node_id}: design read from {written.relative_to(self.output_dir)}")
                except (OSError, json.JSONDecodeError):
                    design = None
        if not isinstance(design, dict):
            log(f"[flow] {node_id}: design turn produced no JSON; continuing with prose design")
            return {"notes": text.strip()[-1500:]} if text.strip() else None
        return design

    def save_design(self, node_id: str, design: dict) -> None:
        design_dir = self.output_dir / ".arc" / "design"
        design_dir.mkdir(parents=True, exist_ok=True)
        (design_dir / f"{node_id}.json").write_text(json.dumps(design, ensure_ascii=False, indent=2), encoding="utf-8")
        try:
            self.runtime.traceability.upsert_node_contract(node_id, design)
            for i, route in enumerate(design.get("routes") or []):
                if isinstance(route, dict):
                    self.runtime.traceability.upsert_interface(
                        interface_id=f"{node_id}:route:{i}", req_ids=[node_id], type="http",
                        content=f"{route.get('method', '')} {route.get('path', '')}".strip(), emit_event=False)
        except Exception as exc:  # noqa: BLE001
            log(f"[trace] design not recorded: {exc}")

    def node_cycle(self, node: dict, ordered: list[dict], index: int, total: int) -> None:
        node_id = str(node.get("id"))
        specs = list(self.spec_map.get(node_id) or [])
        self.codegen_blocked = False  # a previous node's fallback to tool mode must not leak into this one
        if index > 1:
            reap_workspace_processes(self.output_dir, log)
        nodes_left = total - index + 1
        node_budget = min(self.node_budget_cap, max(240, self.remaining() / nodes_left))
        deadline = time.time() + node_budget
        log(f"[flow] node {index}/{total} {node_id} starting (budget {node_budget:.0f}s, specs={specs})")

        self.mark("design_started", node_id)
        design = None
        design_wanted = self.design_enabled and total >= self.design_min_nodes
        inline_design = design_wanted and self.design_mode == "inline"
        if design_wanted and not inline_design:
            design = self.design(node, ordered, deadline)
        if design:
            self.designs[node_id] = design
            self.save_design(node_id, design)
            self.mark("design_done", node_id, "design JSON written to .arc/design/" + node_id + ".json")
        elif not inline_design:
            self.mark("design_done", node_id, "design folded into the implementation prompt")

        self.mark("implementation_started", node_id)
        design_text = ("Design contract for this node (follow it):\n"
                       + json.dumps(design, ensure_ascii=False)[:4000] + "\n") if design else ""
        if inline_design:
            design_text = INLINE_DESIGN_NOTE.format(node_id=node_id)
        if self.evolution:
            design_text = EVOLUTION_NOTE.format(listing=source_listing(self.output_dir)) + design_text
        elif self.has_app():
            design_text = ("Current application files (read only backend/server.js and the page you extend):\n"
                           + source_listing(self.output_dir) + "\n") + design_text
        if self.has_app():
            preamble = NODE_PREAMBLE_EXTEND.format(node_id=node_id)
        else:  # single-node tree without a skeleton turn: create the app in this turn
            preamble = NODE_PREAMBLE_CREATE.format(node_id=node_id, req_dir=self.req_dir, port=self.web_port)
        prompt = NODE_PROMPT.format(node_id=node_id, node_spec=describe_node(node), design=design_text,
                                    preamble=preamble, ancestors=self.ancestors_text(node_id, ordered),
                                    tests=self.tests_prompt_for(node_id), smoke=self.smoke_port, port=self.web_port,
                                    performance=self.perf_text(), ui=self.ui_contract(), verify=self.verify_text(total))
        prompt = self.corrections_text() + prompt
        codegen_prompt = None
        implement_timeout = min(self.node_timeout, self.implement_fraction * node_budget, deadline - time.time())
        tiny_ok = False
        if self.codegen_mode() and self.tiny_mode(len(self.spec_bodies(node_id))):
            tiny_ok = self.tiny_turn(node_id, specs, implement_timeout)
            self.current_spec_chars = len(self.spec_bodies(node_id))
        if tiny_ok:
            ok, text = True, "tiny tier: specs pass"
        elif self.codegen_mode():
            spec_text = self.spec_bodies(node_id)
            self.current_spec_chars = len(spec_text)
            # Small specs (by size, an input-derived measure) get the compact rule; the multi-page
            # mechanisms only apply when the spec is large enough to need sessions/navigation.
            small = self.codegen_reasoning(self.current_spec_chars) == "none"
            compact = CODEGEN_PROMPT.format(node_id=node_id, description=str(node.get("description") or "").strip(),
                                            spec=spec_text, port=self.web_port, ports=self.codegen_ports_clause(),
                                            size_rule=CODEGEN_SIZE_SMALL if small else CODEGEN_SIZE_FULL)
            if self.has_app():  # evolution: keep the existing app, return every changed file complete
                compact = (compact.replace("Files:", "Existing app below; keep everything that works and output "
                                           "every changed file complete. Files:", 1)
                           + inline_sources(self.output_dir, 30000, exts=(".html", ".js")))
            codegen_prompt = compact
            write_codegen_manifests(self.output_dir)
            ok, text = self.codegen_turn(compact, implement_timeout, f"{node_id} implement", spec_chars=self.current_spec_chars)
        else:
            ok, text = self.turn(prompt, implement_timeout, f"{node_id} implement")
        if not ok and "truncated" in text.lower():
            # Cloud 76fb32a69d81: output cut by max_tokens, nothing written. Retry
            # once, one file per response (fresh session, same prompt).
            log(f"[flow] {node_id}: output truncated; retrying with one file per response")
            self.driver.close()
            retry = prompt + ("\nYOUR PREVIOUS RESPONSE WAS TRUNCATED BY THE OUTPUT LIMIT AND NOTHING WAS SAVED. "
                              "Write exactly ONE file per response (one write_file call, complete file), "
                              "starting with backend/server.js, then finish.\n")
            ok, text = self.turn(retry, min(self.node_timeout, deadline - time.time()), f"{node_id} implement (retry)")
        timed_out = (not ok) and "timed out" in text.lower()
        if ok and not self.has_app():
            # v6-counter: one package.json missing after the turn. Do not give
            # up — the acceptance loop's build error becomes the repair prompt.
            log(f"[flow] {node_id}: app layout incomplete after the turn; acceptance loop will drive the repair")
            self.pending_corrections.append(
                "Your turn ended without both frontend/package.json and backend/package.json (with `build` and "
                "`start` scripts) on disk; the harness could not even build the app. Create the missing files.")
        if not ok and not timed_out:
            self.mark("implementation_failed", node_id, text[-500:])
            self.impl_failed.append(node_id)
            return
        if timed_out:
            # The files written so far stay on disk; let the acceptance loop judge them.
            log(f"[flow] {node_id}: implement turn hit its {implement_timeout:.0f}s cap; testing what exists")
            self.driver.close()
            self.pending_corrections.append(
                "Your implementation turn ran out of time; work in smaller steps and verify with curl early.")
        if inline_design:
            written = self.output_dir / ".arc" / "design" / f"{node_id}.json"
            try:
                design = json.loads(written.read_text(encoding="utf-8")) if written.is_file() else None
            except (OSError, json.JSONDecodeError):
                design = None
            if isinstance(design, dict):
                self.designs[node_id] = design
                self.save_design(node_id, design)
                self.mark("design_done", node_id, "design JSON written inline to .arc/design/" + node_id + ".json")
            else:
                self.mark("design_done", node_id, "design folded into the implementation turn (no JSON file)")
        self.mark("implementation_done", node_id, (text[-500:] or None) if ok else "implement turn timed out; partial code")
        self.commit(f"{node_id} (implement): {node.get('name', '')}")

        def rebuild_prompt(failures: str) -> str:
            if self.codegen_mode() and codegen_prompt:
                return (codegen_prompt + "\nYour previous files (quoted below) failed every test. Failures:\n" + failures
                        + "\n" + inline_sources(self.output_dir, 30000, exts=(".html", ".js"))
                        + "Fix the root causes and return every file you change, complete.\n")
            return (prompt + "\nYOUR PREVIOUS ATTEMPT FAILED EVERY ACCEPTANCE TEST — the failures (Feature / where / "
                    "observation / steps):\n" + failures + "\n" + self.sources_text()
                    + "Rewrite the files for this node completely (full write_file for each file, not edits), "
                    "fixing the root causes above.\n")

        verdict = self.acceptance_loop(node_id, specs, deadline, rebuild_prompt=rebuild_prompt)
        self.test_verdict[node_id] = verdict
        if verdict is True:
            self.mark("test_passed", node_id, f"{len(specs)} acceptance spec file(s) pass locally")
            try:
                for iface in self.runtime.traceability.list_interfaces(req_id=node_id):
                    self.runtime.traceability.set_interface_implemented(iface["interface_id"], True, emit_event=False)
            except Exception:  # noqa: BLE001
                pass
        elif verdict is False:
            self.mark("test_failed", node_id, "acceptance specs still failing after repair rounds")

    def snapshot_sources(self, node_id: str, attempt: int) -> Path | None:
        """Copy the app sources that the next repair will overwrite into
        .arc/codegen/<node>-r<attempt>/ (the platform keeps the workspace but not
        our git history, so the first-pass code was unrecoverable: cloud 27de75de0cd0)."""
        dest = self.output_dir / ".arc" / "codegen" / f"{node_id}-r{attempt}"
        try:
            if dest.exists():
                shutil.rmtree(dest)
            count = 0
            for rel in ("frontend/src", "backend"):
                src = self.output_dir / rel
                if not src.is_dir():
                    continue
                for path in src.rglob("*"):
                    if not path.is_file() or "node_modules" in path.parts or path.suffix not in (".html", ".js", ".json", ".css"):
                        continue
                    target = dest / path.relative_to(self.output_dir)
                    target.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(path, target)
                    count += 1
            log(f"[flow] {node_id}: {count} source file(s) snapshotted to {dest.relative_to(self.output_dir)}")
            return dest
        except OSError as exc:
            log(f"[flow] {node_id}: source snapshot failed: {exc}")
            return None

    def discard_template(self) -> Path | None:
        """Move frontend/ and backend/ of a non-working existing app to
        .arc/template-discarded/ so the fresh build starts from our own layout."""
        dest = self.output_dir / ".arc" / "template-discarded"
        try:
            if dest.exists():
                shutil.rmtree(dest)
            dest.mkdir(parents=True, exist_ok=True)
            moved = []
            for name in ("frontend", "backend"):
                src = self.output_dir / name
                if src.exists():
                    shutil.move(str(src), str(dest / name))
                    moved.append(name)
            log(f"[flow] existing app passes no spec; moved {moved} to {dest.relative_to(self.output_dir)} and building fresh")
            return dest
        except OSError as exc:
            log(f"[flow] could not set the existing app aside: {exc}")
            return None

    def already_passing_nodes(self, node_ids: list[str]) -> set[str]:
        """Evolution probe: run each candidate node's specs against the existing app
        (no LLM); nodes that fully pass need no implementation turn."""
        out: set[str] = set()
        for node_id in node_ids:
            specs = list(self.spec_map.get(node_id) or [])
            if not specs:
                continue
            summary = self.run_specs(specs)
            self.probe_count += 1
            if summary.error:
                log(f"[acceptance] probe {node_id}: existing app does not build/start/serve ({summary.error[:160]})")
                continue
            if not summary.total:
                continue
            log(f"[acceptance] probe {node_id}: {summary.passed}/{summary.total} against the existing app")
            if summary.all_passed:
                out.add(node_id)
                self.probe_summaries[node_id] = summary  # regression_cycle reuses it
        return out

    def regression_cycle(self, node: dict) -> None:
        """Evolution: unchanged node — carry the design/impl over, re-run its specs."""
        node_id = str(node.get("id"))
        specs = list(self.spec_map.get(node_id) or [])
        self.mark("design_started", node_id)
        self.mark("design_done", node_id, "unchanged since the previous requirement version; carried over")
        self.mark("implementation_started", node_id)
        self.mark("implementation_done", node_id, "carried over from the template application")
        verdict = None
        if self.runner is not None and specs:
            summary = self.probe_summaries.pop(node_id, None) or self.run_specs(specs)
            if summary.error:
                log(f"[acceptance] regression {node_id} infrastructure error: {summary.error[:300]}")
            else:
                self.record_tests(node_id, specs, summary)
                verdict = summary.all_passed
                log(f"[acceptance] regression {node_id}: {summary.passed}/{summary.total}")
                if not verdict:
                    deadline = time.time() + min(self.node_budget_cap, max(240, self.remaining() / 2))
                    self.pending_corrections.append(
                        "This node passed before this evolution round; the regression below must be fixed "
                        "without removing the new behaviour.")
                    verdict = self.acceptance_loop(node_id, specs, deadline)
        self.test_verdict[node_id] = verdict
        if verdict is True:
            self.mark("test_passed", node_id, "regression specs pass locally")
        elif verdict is False:
            self.mark("test_failed", node_id, "regression specs fail after repair rounds")

    def final_acceptance(self) -> None:
        """Run EVERY spec file together, files in parallel, like the grader does.
        Per-node runs cannot see cross-node interference through shared server
        state; this pass can, and it repairs the nodes whose tests fail."""
        if self.runner is None or not self.tests_dir:
            return
        all_specs = sorted(str(p.relative_to(self.tests_dir)) for p in self.tests_dir.rglob("*.spec.ts"))
        unverified = [n for n, v in self.test_verdict.items() if v is not True] or \
            [n for n in self.spec_map if n and self.spec_map[n] and n not in self.test_verdict]
        if len(all_specs) < 2 and not unverified:
            return  # single spec already judged by the node run
        rounds = int(os.environ.get("OCTOS_FINAL_REPAIR_ROUNDS", "2"))
        workers = workers_for_final(getattr(self, "mem_limit", None), int(os.environ.get("OCTOS_ARC_FINAL_WORKERS", "4")))
        previous_failing: set[str] | None = None
        for attempt in range(rounds + 1):
            summary = self.run_specs(all_specs, workers=workers, grader_like=True)
            if summary.error and summary.killed:
                # Cloud 29c840566f36: the runner was OOM-killed under a 512 MiB
                # cgroup; two repair rounds were wasted on a non-failure.
                log(f"[acceptance] full suite could not run ({summary.error[:120]}); keeping per-node verdicts")
                return
            if summary.error:
                # The app does not even start the way the grader starts it: every node fails.
                log(f"[acceptance] full suite (grader-like start) failed: {summary.error[:300]}")
                for node_id in self.spec_map:
                    if node_id:
                        self.test_verdict[node_id] = False
                grouped = {None: []}
                failures = (f"- Feature: application startup exactly as the grader runs it (only PORT set)\n"
                            f"  Failed at: npm start\n  Observation: {summary.error[:700]}\n  Steps: npm run build -> npm start")
                summary = RunSummary(passed=0, total=len(all_specs))
            else:
                grouped = nodes_for_failures(summary.results, self.spec_map)
                failures = failure_summaries(RunSummary(results=[r for rs in grouped.values() for r in rs]))
            log(f"[acceptance] full suite round {attempt}: {summary.passed}/{summary.total}; failing nodes "
                f"{sorted(k for k in grouped if k) or ('all' if None in grouped and not summary.results else [])}")
            for node_id, specs in self.spec_map.items():
                if node_id and specs and summary.results:
                    self.record_tests(node_id, specs, RunSummary(results=[r for r in summary.results
                                      if Path(r.file or "").name in {Path(p).name for p in specs}]))
                    self.test_verdict[node_id] = node_id not in grouped
            if not grouped:
                self.commit(f"chore: full acceptance suite {summary.passed}/{summary.total} pass (parallel)")
                return
            failing_titles = {r.title for rs in grouped.values() for r in rs}
            if previous_failing is not None and failing_titles == previous_failing:
                log("[acceptance] full suite: same failures as the previous round; stopping repairs")
                break
            previous_failing = failing_titles
            if attempt == rounds or self.remaining() < 240 or self.wound_down():
                break
            failing = sorted(k for k in grouped if k) or ["all nodes"]
            prompt = REPAIR_PROMPT.format(
                node_id=", ".join(failing), passed=summary.passed, total=summary.total, failures=failures,
                sources=self.sources_text(),
                corrections=self.corrections_text() + "The grader runs all spec files IN PARALLEL against one "
                "server; tests from different files must not interfere through shared server state "
                "(e.g. a counter that every browser session shares). Keep persisted data only where the "
                "requirement demands persistence.\n",
                slow="", smoke=self.smoke_port, port=self.web_port)
            self.turn(prompt, min(self.node_timeout, max(120, self.remaining() - 200)),
                      f"full-suite repair {attempt + 1}/{rounds}")
            self.commit(f"fix: full-suite repair {attempt + 1}")

    # -- skeleton ---------------------------------------------------------
    def skeleton(self, tree: dict) -> None:
        log("[flow] skeleton turn starting")
        prompt = SKELETON_PROMPT.format(req_dir=self.req_dir, port=self.web_port, smoke=self.smoke_port,
                                        tests=self.tests_prompt_for(None, skeleton=True))
        for attempt in range(1, 5):
            if self.time_up():
                raise RuntimeError("time budget exhausted before the skeleton existed")
            ok, text = self.turn(prompt, self.node_timeout, f"skeleton attempt {attempt}")
            if ok and not self.has_app():
                log("[flow] skeleton turn wrote no frontend/backend; nudging")
                for nudge in range(1, 3):
                    self.turn(NUDGE_PROMPT, 600, f"nudge {nudge}/2")
                    if self.has_app():
                        break
            if self.has_app():
                self.commit("chore: scaffold web application skeleton")
                return
            time.sleep(30)
        raise RuntimeError("skeleton scaffolding failed: no frontend/ and backend/ after 4 attempts")

    def has_app(self) -> bool:
        return (self.output_dir / "frontend" / "package.json").is_file() and \
            (self.output_dir / "backend" / "package.json").is_file()

    # -- final ------------------------------------------------------------
    def rehearsal(self) -> bool:
        for attempt in range(1, 4):
            log(f"[rehearsal] startup rehearsal {attempt}/3 (smoke port {self.smoke_port}, grader-like env)")
            server = self.app_server(grader_like=True)
            err = server.build() or server.start()
            server.stop()
            if err is None:
                log("[rehearsal] app builds and starts cleanly")
                return True
            log(f"[rehearsal] FAILED: {err.splitlines()[0][:200]}")
            if attempt == 3 or self.remaining() < -600:
                log("[rehearsal] giving up; submitting as-is")
                return False
            self.turn(REHEARSAL_REPAIR_PROMPT.format(error=err[-1200:], port=self.web_port, smoke=self.smoke_port),
                      self.node_timeout, f"rehearsal repair {attempt}")
            self.commit("fix: startup rehearsal repair")
        return False

    # -- run --------------------------------------------------------------
    def run(self) -> int:
        self.runtime = AgentRuntime.from_env(project_dir=str(self.output_dir))
        self.events = self.runtime.events
        self.events.mark_run_started("octos bundle started")
        ordered: list[dict] = []
        watchdog_stop = threading.Event()
        try:
            previous = previous_requirement_records(self.output_dir)
            tree = load_requirement_tree(self.req_dir)
            self.runtime.traceability.store_requirement_tree(tree)
            ordered = topo_order(tree)
            if not ordered:
                raise ValueError("no ATOMIC requirement nodes found")
            self.classify_tree(tree)
            node_ids = [str(n.get("id")) for n in ordered]
            if not self.budget_explicit:
                # 32-node trees need hours, not the 1-hour smoke default.
                self.budget = max(self.budget, self.seconds_per_node * len(ordered))
            log(f"[flow] {len(ordered)} atomic nodes in dependency order: {node_ids}; time budget {self.budget}s")
            self.folder_children = folder_descendants(tree)

            self.evolution = self.has_app()
            unchanged: set[str] = set()
            if self.evolution:
                unchanged = unchanged_node_ids(ordered, previous)
                log(f"[flow] evolution mode: existing app detected; unchanged nodes {sorted(unchanged)}, "
                    f"to implement {[i for i in node_ids if i not in unchanged]}")
            self.nodes_to_implement = len([n for n in node_ids if n not in unchanged])
            self.n_nodes = len(ordered)
            if not self.repair_rounds_explicit and self.n_nodes > 2:
                self.repair_rounds = 3  # big trees: identical-failure/no-improvement stops make 5 rounds rare anyway
            if self.max_total_tokens < 0:
                self.max_total_tokens = max(6_000_000, 2_500_000 * self.n_nodes)   # ~3x the calibrated 0.8M/node
            if self.max_turns < 0:
                self.max_turns = max(24, 4 * self.n_nodes)                          # ~3.5x the calibrated 1.1/node
            log(f"[guard] cost guard: {self.max_total_tokens} tokens / {self.max_turns} turns"
                + (f" / absolute {self.max_total_tokens_abs}" if self.max_total_tokens_abs else ""))

            self.tests_dir = locate_acceptance_tests(tree, BUNDLE_DIR)
            if self.tests_dir:
                specs = sorted(str(p.relative_to(self.tests_dir)) for p in self.tests_dir.rglob("*.spec.ts"))
                self.spec_map, self.aliases = map_specs_to_nodes(specs, node_ids)
                log(f"[tests] {len(specs)} spec files at {self.tests_dir}; mapping "
                    f"{ {k: v for k, v in self.spec_map.items() if v} }; aliases {self.aliases}")
            else:
                log("[tests] no acceptance specs found; building from requirement text only")

            self.maybe_probe(node_ids)
            self.runtime.git.ensure_repo()
            self.setup_playwright()
            if self.evolution and self.runner is not None:
                # The platform's template app carries no traceability records, so
                # fingerprints cannot tell what is new. A node whose specs already
                # pass against the existing app is unchanged — no LLM turn for it.
                unchanged |= self.already_passing_nodes([n for n in node_ids if n not in unchanged])
                if self.probe_count and not unchanged:
                    # Nothing of the existing app satisfies any spec (a scaffold/placeholder
                    # template, or an app the new specs no longer accept): it is not a usable
                    # base. Set it aside and build the task fresh (cloud c30b29eab45b/10b04d36f704:
                    # implement-then-rewrite on a placeholder cost 40-80x the fresh build).
                    self.discard_template()
                    self.evolution = False
                self.nodes_to_implement = len([n for n in node_ids if n not in unchanged])
                log(f"[flow] {'evolution' if self.evolution else 'fresh build'} after probing the existing app: "
                    f"unchanged {sorted(unchanged)}, to implement {[i for i in node_ids if i not in unchanged]}")

            dry_run = os.environ.get("OCTOS_ARC_DRYRUN") == "1"
            if dry_run:
                octos_bin = "(dry run: no kernel)"
                log("[octos] OCTOS_ARC_DRYRUN=1: fixed placeholder replies, no model calls")
            else:
                octos_bin = find_octos()
            log(f"[octos] binary {octos_bin}")
            data_dir = Path(tempfile.mkdtemp(prefix="octos-data-"))
            protected = [p for p in (self.tests_dir, self.req_dir) if p and p.is_dir()]
            config_dir = Path(tempfile.mkdtemp(prefix="octos-config-"))
            self.start_llm_proxy()
            env = build_octos_env(config_dir, protected)
            write_profile_defaults(data_dir, config_dir, protected_hooks(protected))
            self.snapshot_protected()
            env["PORT"] = str(self.smoke_port)  # a bare `npm start` inside a turn must not hit the grading port
            self.driver = DryRunDriver() if dry_run else OctosDriver(
                octos_bin, self.output_dir, env, data_dir, int(os.environ.get("OCTOS_MAX_ITERATIONS", "500")),
                events_log=self.output_dir / ".arc" / "octos-events.jsonl")
            self.driver.hooks = protected_hooks(protected)
            threading.Thread(target=_port_watchdog, args=(self.web_port, self.output_dir, watchdog_stop),
                             daemon=True).start()
            try:
                if not self.evolution and (len(ordered) >= self.skeleton_min_nodes
                                           or os.environ.get("OCTOS_SKELETON_ALWAYS") == "1"):
                    self.skeleton(tree)
                    self.driver.end_scope("node")
                elif not self.evolution:
                    log(f"[flow] {len(ordered)}-node tree: skeleton folded into the first node turn")
                for index, node in enumerate(ordered, 1):
                    node_id = str(node.get("id"))
                    if self.time_up():
                        log(f"[flow] time budget exhausted; skipping {node_id}")
                        self.mark("implementation_started", node_id)
                        self.mark("implementation_failed", node_id, "skipped: time budget exhausted")
                        self.impl_failed.append(node_id)
                        continue
                    if node_id in unchanged:
                        self.regression_cycle(node)
                    else:
                        self.node_cycle(node, ordered, index, len(ordered))
                    self.driver.end_scope("node")

                if not self.time_up():
                    self.final_acceptance()
                    self.driver.end_scope("node")
                undecided = [i for i in node_ids if self.test_verdict.get(i) is None and i not in self.impl_failed]
                final_ok = None
                if undecided and not self.time_up():
                    log(f"[flow] final check turn for nodes without a local verdict: {undecided}")
                    final_ok, _ = self.turn(FINAL_CHECK_PROMPT.format(smoke=self.smoke_port, port=self.web_port,
                                                                      tests=self.tests_prompt_for(None),
                                                                      performance=self.perf_text(), ui=self.ui_contract()),
                                            self.node_timeout, "final check")
                    self.commit("chore: final verification pass")
                rehearsed = self.rehearsal()
                for node_id in undecided:
                    if rehearsed and final_ok is not False:
                        self.mark("test_passed", node_id, "final check and startup rehearsal passed")
                    else:
                        self.mark("test_failed", node_id, "final check or startup rehearsal failed")
                    self.test_verdict[node_id] = bool(rehearsed and final_ok is not False)
            finally:
                watchdog_stop.set()
                if self.driver:
                    self.driver.close()
                self.cleanup_playwright()
                self.stop_llm_proxy()
            for node_id in node_ids:  # final per-node verdicts (full-suite run may have changed them)
                if self.test_verdict.get(node_id) is True:
                    self.mark("test_passed", node_id, "acceptance specs pass (node run and full parallel suite)")
                elif self.test_verdict.get(node_id) is False:
                    self.mark("test_failed", node_id, "acceptance specs failing")
            self.mark_folders()
            self.commit("chore: traceability and acceptance state")
            failed = [i for i in node_ids if self.test_verdict.get(i) is not True]
            if failed:
                self.events.mark_run_completed(f"completed; nodes not verified: {', '.join(failed)}")
            else:
                self.events.mark_run_completed("all requirement nodes implemented and verified")
            _reap_stray_processes("postflight")
            _postflight_structure_check(self.output_dir)
            _free_web_port(self.web_port)
            self.write_preview_ready()
            return 0
        except Exception as exc:  # the platform judges by events, not exit code
            log(f"[flow] aborted: {exc!r}")
            watchdog_stop.set()
            if self.driver:
                self.driver.close()
            self.cleanup_playwright()
            self.stop_llm_proxy()
            for node in ordered:
                node_id = str(node.get("id"))
                if node_id not in self.test_verdict:
                    self.mark("test_failed", node_id, f"run aborted: {str(exc)[:200]}")
            try:
                self.mark_folders()
            except Exception:  # noqa: BLE001
                pass
            _reap_stray_processes("exception")
            _postflight_structure_check(self.output_dir)
            _free_web_port(self.web_port)
            self.events.mark_run_failed(str(exc)[:1000])
            return 0

    def mark_folders(self) -> None:
        """The platform counts FOLDER nodes as requirements too ("45 requirements
        and 32 scenarios" for a 32-leaf tree); derive their state from their
        atomic descendants so the functional-rate denominator is covered."""
        for folder_id, leaves in self.folder_children.items():
            if not leaves:
                continue
            verdicts = [self.test_verdict.get(leaf) for leaf in leaves]
            self.events.mark_design_started(folder_id)
            self.events.mark_design_done(folder_id, f"{len(leaves)} atomic children designed")
            self.events.mark_implementation_started(folder_id)
            if all(v is not None for v in verdicts) or any(leaf in self.impl_failed for leaf in leaves):
                done = [leaf for leaf in leaves if leaf not in self.impl_failed]
                if done:
                    self.events.mark_implementation_done(folder_id, f"{len(done)}/{len(leaves)} atomic children implemented")
                else:
                    self.events.mark_implementation_failed(folder_id, "no atomic child implemented")
            if all(v is True for v in verdicts):
                self.events.mark_test_passed(folder_id, f"all {len(leaves)} atomic children pass")
            else:
                failing = [leaf for leaf, v in zip(leaves, verdicts) if v is not True]
                self.events.mark_test_failed(folder_id, f"children not verified: {', '.join(failing)}")

    def write_preview_ready(self) -> None:
        artifacts_dir = os.environ.get("ARCBENCH_ARTIFACTS_DIR")
        if artifacts_dir:
            try:
                Path(artifacts_dir).mkdir(parents=True, exist_ok=True)
                (Path(artifacts_dir) / "preview-ready.json").write_text(
                    json.dumps({"ready": True, "reason": "octos bundle completed"}) + "\n", encoding="utf-8")
            except OSError:
                pass


# ---------------------------------------------------------------- main

def minimal_probe_body(model: str) -> bytes:
    """Fallback chat probe that cannot bill reasoning: thinking disabled, one output token."""
    return json.dumps({"model": model, "messages": [{"role": "user", "content": "OK"}], "max_tokens": 1,
                       "thinking": {"type": "disabled"}}).encode()


def endpoint_is_up(status: int) -> bool:
    """Any non-5xx HTTP answer proves the endpoint is reachable (401/404 included)."""
    return status < 500


def probe_endpoint() -> None:
    """Wait out endpoint/proxy outages (up to 10 min) without spending tokens:
    GET /models first (unbilled; any non-5xx answer = up). Only if that never
    answers, one chat request with thinking disabled and max_tokens=1.
    The old probe ("Reply with exactly: OK", max_tokens=4) let the model reason
    before its 4-token answer — about ¥0.0008 per run, a third of a Smoke task."""
    key = os.environ.get("OPENAI_API_KEY", "")
    base = os.environ.get("OPENAI_BASE_URL")
    if not (key and base):
        return
    import urllib.request as _ur
    import urllib.error as _ue
    headers = {"Content-Type": "application/json", "Authorization": "Bearer " + key}
    deadline = time.time() + 600
    attempt = 0
    while True:
        attempt += 1
        try:
            with _ur.urlopen(_ur.Request(base.rstrip("/") + "/models", headers=headers), timeout=30) as resp:
                log(f"[probe] GET /models -> HTTP {resp.status} (endpoint up, no tokens spent)")
                return
        except _ue.HTTPError as exc:
            if endpoint_is_up(exc.code):
                log(f"[probe] GET /models -> HTTP {exc.code} (endpoint up, no tokens spent)")
                return
            log(f"[probe] attempt {attempt}: GET /models -> HTTP {exc.code}")
        except Exception as exc:  # noqa: BLE001
            log(f"[probe] attempt {attempt}: GET /models -> {exc}")
            # Some gateways expose only chat/completions: one minimal, reasoning-free request.
            try:
                req = _ur.Request(base.rstrip("/") + "/chat/completions", headers=headers, method="POST",
                                  data=minimal_probe_body(os.environ.get("MODEL", "deepseek-chat")))
                with _ur.urlopen(req, timeout=60) as resp:
                    log(f"[probe] minimal chat probe -> HTTP {resp.status}")
                    return
            except _ue.HTTPError as exc2:
                if endpoint_is_up(exc2.code):
                    log(f"[probe] minimal chat probe -> HTTP {exc2.code} (endpoint up)")
                    return
                log(f"[probe] attempt {attempt}: chat -> HTTP {exc2.code}")
            except Exception as exc2:  # noqa: BLE001
                log(f"[probe] attempt {attempt}: chat -> {exc2}")
        if time.time() >= deadline:
            log("[probe] endpoint still failing after 10min; proceeding anyway")
            return
        time.sleep(30)


def main() -> int:
    parser = argparse.ArgumentParser(description="Octos agent bundle for ARC-Bench")
    parser.add_argument("requirement_path", nargs="?", default=os.environ.get("ARCBENCH_TASK_DIR", "/workspace/task"))
    parser.add_argument("--output-dir", default=None)
    parser.add_argument("--type", "--app-type", dest="app_type", default="web")
    parser.add_argument("--web-port", type=int,
                        default=int(os.environ.get("ARCBENCH_WEB_PORT", os.environ.get("ARC_WEB_PORT", "3000"))))
    args = parser.parse_args()

    if os.environ.get("OCTOS_ARC_ENGINE") == "rust":
        # Kernel harness (`octos arc run`); this module keeps the default Python path.
        import rust_engine
        return rust_engine.main(args)

    key = os.environ.get("OPENAI_API_KEY", "")
    print(f"[env] OPENAI_BASE_URL={os.environ.get('OPENAI_BASE_URL', '<unset>')}", flush=True)
    print(f"[env] MODEL={os.environ.get('MODEL', '<unset>')}", flush=True)
    print(f"[env] OPENAI_API_KEY={'set(len=%d)' % len(key) if key else '<unset>'}", flush=True)
    print(f"[env] ARCBENCH_TEMPLATE_DIR={os.environ.get('ARCBENCH_TEMPLATE_DIR', '<unset>')}", flush=True)
    print(f"[env] ARCBENCH_TASK_DIR={os.environ.get('ARCBENCH_TASK_DIR', '<unset>')}", flush=True)
    print(f"[env] argv requirement_path={args.requirement_path}", flush=True)
    req_src = Path(args.requirement_path).resolve()
    if args.output_dir:
        output_dir = Path(args.output_dir).resolve()
    elif os.environ.get("ARCBENCH_TEMPLATE_DIR"):
        output_dir = Path(os.environ["ARCBENCH_TEMPLATE_DIR"]).resolve()
    else:
        output_dir = Path.cwd() / "workspace" / f"run-{time.strftime('%Y%m%d-%H%M%S')}"
    output_dir.mkdir(parents=True, exist_ok=True)

    on_platform = bool(os.environ.get("ARCBENCH_TEMPLATE_DIR"))
    if on_platform:
        req_dir = req_src
    else:
        req_dir = output_dir / "requirements"
        if req_dir.exists():
            shutil.rmtree(req_dir)
        shutil.copytree(req_src, req_dir)
    return Flow(args, output_dir, req_dir).run()


if __name__ == "__main__":
    sys.exit(main())
