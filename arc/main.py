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
    OCTOS_NODE_TIME_BUDGET    cap per node incl. repairs (default 3000; was 1500)
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
from contextlib import contextmanager
import json
import os
import re
import shutil
import shlex
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
    workers_for_memory, process_cwd, workspace_contains, free_owned_ports,
    AcceptanceRunner, AppServer, RunSummary, acceptance_work_dir, clip_ends, container_memory_limit, ensure_playwright,
    failure_signature, failure_summaries, failure_source_context, find_playwright_by_search, find_playwright_root, map_specs_to_nodes,
    nodes_for_failures, playwright_candidates, playwright_version_hint, restore_tree,
    mutated_by_tests, restore_worktree, snapshot_worktree, startup_error_digest, tree_digest,
    workers_for_final, reap_workspace_processes)
from codegen import (FORMAT_INSTRUCTIONS, dedupe_nav_links, delimiter_drift,  # noqa: E402
                     parse_file_blocks, unchanged_rewrites, unparsed_reply_digest,
                     write_files)
from guard import TurnMonitor  # noqa: E402
from llm_proxy import LlmProxy, configured_model_routes  # noqa: E402
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


def _reap_stray_processes(tag: str, output_dir: Path) -> None:
    """Report resource use and stop matching descendants or workspace processes.

    A killed grader does not by itself establish memory exhaustion. Never
    attribute another run's processes merely from a browser or Node name.
    """
    me = os.getpid()
    def _run(cmd: list[str]) -> str:
        try:
            return subprocess.run(cmd, capture_output=True, text=True,
                                  timeout=20).stdout
        except (OSError, subprocess.TimeoutExpired) as exc:
            return f"<{cmd[0]} unavailable: {exc}>"
    log(f"[reap:{tag}] memory:\n" + _run(["free", "-m"]).rstrip())
    # `free` shows the host; cgroup counters help distinguish container
    # memory pressure from other causes of a killed grading process.
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
    rows = []
    for line in ps.splitlines()[1:]:
        parts = line.split(None, 4)
        if len(parts) != 5 or not parts[0].isdigit() or not parts[1].isdigit():
            continue
        rows.append((int(parts[0]), int(parts[1]), parts[4]))
    descendants = {me}
    while True:
        expanded = descendants | {pid for pid, parent, _ in rows if parent in descendants}
        if expanded == descendants:
            break
        descendants = expanded
    victims = []
    for pid, _, args in rows:
        if pid in (me, os.getppid()):
            continue
        if not any(marker in args.lower() for marker in
                   ("chrom", "headless_shell", "playwright", "octos serve", "node ", "npm ", "/node")):
            continue
        if pid in descendants or workspace_contains(process_cwd(pid), output_dir):
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




def _free_web_port(web_port: int, output_dir: Path) -> None:
    """Release only listeners attributable to this workspace."""
    free_owned_ports([web_port], output_dir)



def _port_watchdog(web_port: int, output_dir: Path, stop: threading.Event) -> None:
    """Kill OUR processes that bind the grading port during generation (the
    runner terminates a run that serves the grading port early). Foreign
    listeners are left alone: the runner host is shared."""
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
            if workspace_contains(cwd, output_dir):
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
    # Preserve source filenames and nested assets. Routes belong to the application;
    # extensionless aliases can shadow HTML routes and acquire a binary MIME type.
    "frontend/package.json": {"name": "f", "private": True, "scripts": {"build": "node -e \"const f=require('fs');f.rmSync('dist',{recursive:true,force:true});f.cpSync('src','dist',{recursive:true})\""}},
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


def inline_sources(output_dir: Path, max_chars: int = 90000, exts: tuple = (".js", ".mjs", ".cjs", ".html", ".css", ".json")) -> str:
    """Quote the app's source files (frontend sources, backend JS) so a repair
    turn edits immediately instead of spending its request budget on reads.

    Bounded, largest first: whatever has to be dropped should be the file least
    likely to need editing, and quoting smallest first made that the largest
    one. In cloud e767e871a6c6 the seed data, the lockfiles and the secondary
    pages were quoted and `frontend/src/index.html` — the whole UI, and where
    nearly every failure lives — was the one file omitted.
    """
    files: list[Path] = []
    for part in ("frontend", "backend"):
        base = output_dir / part
        if base.is_dir():
            for path in sorted(base.rglob("*")):
                rel = path.relative_to(output_dir)
                if any(seg in ("node_modules", "dist", ".git") for seg in rel.parts):
                    continue
                if path.is_file() and path.suffix in exts:
                    files.append(path)
    parts, total = [], 0
    for path in sorted(files, key=lambda p: (-p.stat().st_size, str(p))):
        try:
            text = path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        if total + len(text) > max_chars:
            # A task with a hundred nodes grows one dominant UI file past the
            # whole budget on its own. Dropping it left the prompt quoting the
            # stylesheet and the seed data and not the file every repair edits,
            # which is the failure the largest-first order was meant to prevent.
            # Give any file too big to quote whole a share of the budget instead
            # of nothing, capped at half so one file cannot crowd out the rest;
            # the elision marker keeps it from reading as the complete file.
            room = min(max_chars // 2, max_chars - total)
            if room >= 2000:
                total += room
                parts.append(f"--- {path.relative_to(output_dir)} --- (too large to quote whole, "
                             f"{len(text)} chars; the part shown is clipped, read it for the rest)\n"
                             f"{clip_ends(text, room)}\n")
                continue
            parts.append(f"--- {path.relative_to(output_dir)} --- (omitted, {len(text)} chars; read it if you must change it)\n")
            continue
        total += len(text)
        parts.append(f"--- {path.relative_to(output_dir)} ---\n{text.rstrip()}\n")
    return ("Current source files (quoted; edit them directly, no need to read):\n" + "".join(parts)) if parts else ""


SOURCE_EXTS = (".html", ".js", ".mjs", ".cjs", ".css", ".json")  # visibility and layout failures may originate in CSS


def app_source_chars(output_dir: Path, exts: tuple = SOURCE_EXTS) -> int:
    """Total size of the generated app's own sources.

    This is what the codegen context budget is really spent on, and crossing it
    is the most expensive event in a run: from that node on, every node repairs
    through tool mode instead of one request. On bookstack that was about 19
    requests per node instead of one, for the last seven nodes.
    """
    total = 0
    for path in app_source_files(output_dir, exts):
        try:
            total += len(path.read_text(encoding="utf-8", errors="replace"))
        except OSError:
            continue
    return total


def app_source_files(output_dir: Path, exts: tuple = SOURCE_EXTS) -> list[Path]:
    files: list[Path] = []
    for part in ("frontend", "backend"):
        base = output_dir / part
        if not base.is_dir():
            continue
        for path in sorted(base.rglob("*")):
            rel = path.relative_to(output_dir)
            if any(seg in ("node_modules", "dist", ".git") for seg in rel.parts):
                continue
            if path.is_file() and path.suffix in exts:
                files.append(path)
    return files


def spec_terms(spec_text: str) -> set[str]:
    """Identifiers, paths and quoted strings a spec mentions (≥ 3 chars), lower-cased."""
    terms = set(re.findall(r"[A-Za-z_][A-Za-z0-9_-]{2,}", spec_text))
    terms |= set(re.findall(r"['\"`](/[^'\"`\s]{1,60})['\"`]", spec_text))
    terms |= set(re.findall(r"['\"`]([^'\"`\n]{3,40})['\"`]", spec_text))
    stop = {"await", "page", "expect", "const", "test", "async", "import", "from", "playwright", "toBeVisible",
            "toHaveText", "getByRole", "getByTestId", "getByLabel", "getByText", "click", "fill", "goto", "name",
            "button", "link", "true", "false", "null", "let", "var", "return", "function"}
    return {t.lower() for t in terms if t not in stop}


def _ranked_sources(output_dir: Path, spec_text: str) -> list[tuple[int, int, int, Path, str]]:
    """The ranking both `relevant_sources` and `quoted_source_paths` read, so the text
    the model sees and the set the harness enforces can never drift apart."""
    files = app_source_files(output_dir)
    if not files:
        return []
    terms = spec_terms(spec_text)
    scored = []
    for path in files:
        try:
            text = path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        low = text.lower()
        hits = sum(1 for t in terms if t in low)
        rel = path.relative_to(output_dir)
        is_backend = rel.parts[0] == "backend"
        priority = 2 if path.suffix == ".json" else (0 if is_backend else 1)
        scored.append((priority, -hits, len(text), rel, text))
    scored.sort(key=lambda x: (x[0], x[1], x[2]))
    return scored


def quoted_source_paths(output_dir: Path, spec_text: str, max_chars: int) -> set[str]:
    """Exactly the files `relevant_sources` quotes whole for the same arguments."""
    quoted, total = set(), 0
    for _, _hits, size, rel, _text in _ranked_sources(output_dir, spec_text):
        if total + size > max_chars:
            continue
        total += size
        quoted.add(str(rel))
    return quoted


def relevant_sources(output_dir: Path, spec_text: str, max_chars: int) -> str:
    """Quote the existing sources a node most likely touches: every backend entry
    file first (the router every node extends), then pages ranked by how many
    of the spec's terms (locators, texts, routes) they contain, until the budget
    is spent. JSON state follows code; the rest are listed by name so the
    model knows they exist. The budget counts file contents, not headings."""
    scored = _ranked_sources(output_dir, spec_text)
    if not scored:
        return ""
    parts, omitted, total = [], [], 0
    for _, neg_hits, size, rel, text in scored:
        if total + size > max_chars:
            omitted.append(f"{rel} ({size} chars, {-neg_hits} spec terms)")
            continue
        total += size
        parts.append(f"--- {rel} ---\n{text.rstrip()}\n")
    out = "Current source files (quoted; return every file you change, complete):\n" + "".join(parts)
    if omitted:
        # Was "unchanged unless the requirement needs them", which invited exactly the
        # failure this now forbids: a turn that returns "every file you change,
        # complete" and decides it needs a file it was never shown rewrites that file
        # from nothing and drops whatever was in it -- behaviour belonging to other
        # nodes' specs. Cloud 12306 99196f2e802b lost 29 nodes that way and not one to
        # an unbuildable spec. The harness enforces this too (`codegen_turn` refuses a
        # block for an existing unquoted file); saying it here keeps the reply useful
        # instead of merely rejected.
        out += ("Files NOT shown to you (do not return these -- you cannot see their "
                "contents, so rewriting one would delete behaviour other requirements "
                "depend on; if the requirement truly needs one, say so in one line "
                "instead of returning it): " + "; ".join(omitted) + "\n")
    return out


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
    "https://github.com/octos-org/octos-arc/releases/download/v2.0.3-rc.11-arc.13/"
    "octos-bundle-x86_64-unknown-linux-gnu.tar.gz"
)


def _cached_runtime_matches(cache_dir: Path, url: str) -> bool:
    try:
        return (cache_dir / "source-url.txt").read_text() == url
    except OSError:
        return False


def _download_octos(dest_dir: Path) -> str:
    """Fetch the Linux octos binary at runtime via gh-proxy mirrors first (the
    runner's path to GitHub stalls / kills HTTP/2 streams)."""
    import tarfile
    import urllib.request

    dest_dir.mkdir(parents=True, exist_ok=True)
    tarball = dest_dir / "octos-bundle.tar.gz"
    url = os.environ.get("OCTOS_RELEASE_URL", OCTOS_RELEASE_URL)
    if not _cached_runtime_matches(dest_dir, url):
        # A complete archive from an older URL must not satisfy a new download.
        tarball.unlink(missing_ok=True)


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
    marker = dest_dir / f"source-url-{os.getpid()}.tmp"
    try:
        marker.write_text(url)
        marker.replace(dest_dir / "source-url.txt")
    finally:
        marker.unlink(missing_ok=True)
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
    url = os.environ.get("OCTOS_RELEASE_URL", OCTOS_RELEASE_URL)
    if (cache_dir / "octos").is_file() and _cached_runtime_matches(cache_dir, url):
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

    @contextmanager
    def without_tools(self):
        """No-op: a dry run has no kernel and therefore no tools to take away.

        #140 gave the real driver `without_tools()` and called it from every codegen
        turn, but did not give the dry-run driver the same method, so
        OCTOS_ARC_DRYRUN=1 aborted at the first codegen node with
        AttributeError('DryRunDriver' object has no attribute 'without_tools') —
        i.e. the free structural-parity path from round 31 has been broken since then.
        """
        yield

    def end_scope(self, *args, **kwargs) -> None:
        pass

    def close(self, *args, **kwargs) -> None:
        pass


class PermanentProviderError(RuntimeError):
    """Account failures require external action, not another generation attempt."""


def permanent_provider_error(text: str) -> bool:
    lowered = text.lower()
    codes = re.findall(r"\bhttp(?:/\d(?:\.\d)?)?\s+(\d{3})\b", lowered)
    return any(code in {"401", "402", "403"} for code in codes) or any(term in lowered for term in (
        "insufficient_balance", "quota exhausted", "balance is exhausted", "invalid_api_key",
        "authentication failed", "unauthorized"))


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
        self.tools_disabled = False

    @contextmanager
    def without_tools(self):
        """Tool policy belongs to the kernel profile, so never reuse a profile across modes."""
        previous = self.tools_disabled
        if not previous:
            self.close()
        self.tools_disabled = True
        try:
            yield
        finally:
            if not previous:
                self.close()
            self.tools_disabled = previous

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
                tools_disabled=self.tools_disabled,
            )
            self._session.open()
        return self._session

    def run(self, prompt: str, timeout: int, monitor: TurnMonitor | None = None) -> tuple[bool, str]:
        self.monitor = monitor
        if self.mode == "chat" and not self.tools_disabled:
            fn = lambda remaining: run_octos(self.octos_bin, self.cwd, prompt, self.env, self.data_dir,  # noqa: E731
                                   remaining, self.max_iterations)
        else:
            fn = lambda remaining: self._run_stdio(prompt, remaining)  # noqa: E731
        try:
            ok, text = self._run_with_heartbeat(lambda: self._run_with_retries(fn, timeout))
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
        if permanent_provider_error(text):
            return False
        codes = re.findall(r"\bhttp(?:/\d(?:\.\d)?)?\s+(\d{3})\b", lowered)
        if codes:
            return any(code in {"408", "425", "429", "500", "502", "503", "504"} for code in codes)
        return any(k in lowered for k in (
            "temporarily unavailable", "rate limit", "timeout", "timed out",
            "connection reset", "overloaded", "failed to send", "streaming request"))

    def _run_with_retries(self, fn, timeout: float, attempts: int = 3) -> tuple[bool, str]:
        deadline = time.monotonic() + max(0, timeout)
        ok, text = False, "octos turn timed out"
        for attempt in range(1, attempts + 1):
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                break
            ok, text = fn(remaining)
            if ok or not self._transient(text) or attempt == attempts:
                break
            wait = 30 * attempt
            if wait >= deadline - time.monotonic():
                log("[driver] remaining turn budget cannot accommodate retry backoff")
                break
            log(f"[driver] transient error, retry {attempt + 1}/{attempts} after {wait}s: {text[:200]}")
            time.sleep(wait)
            self.close()
        return ok, text

    def _run_stdio(self, prompt: str, timeout: float) -> tuple[bool, str]:
        deadline = time.monotonic() + timeout
        try:
            session = self._get_session()
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return False, "octos turn timed out"
            return session.run_turn(prompt, timeout=remaining)
        except Exception as exc:  # noqa: BLE001
            self.close()
            if self.tools_disabled:
                return False, f"tool-free stdio driver error: {exc}"[:1000]
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return False, "octos turn timed out"
            chat_ok, chat_text = run_octos(self.octos_bin, self.cwd, prompt, self.env, self.data_dir,
                                           remaining, self.max_iterations)
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

# Measured on cloud 6e82a7ff571c's own app (32 official specs, 3 runs each, a
# pristine store per run). Its note actions were `visibility: hidden; opacity: 0`
# revealed on hover, and every one of the twelve graded failures was a note-card
# action, so a clause telling the model not to hide them looked obvious. It is
# wrong. Baseline 16/16/16; fading opacity alone 15/14/15; leaving the controls
# always visible 15/15/15. Both alternatives fixed REQ-2.5.4 and REQ-2.7.4 and
# broke REQ-2.6.1, REQ-2.7.2 and REQ-2.8.3. Hiding an item's controls until the
# pointer arrives is what keeps a name-based lookup landing on the hovered card,
# which is what the per-item bullet below already says. Do not add a clause
# against hover-hiding without measuring it on a real app first.
UI_CONTRACT_CORE = """\
UI behavior follows the requirement and the current application:
- Use semantic controls, accessible names and labels appropriate to each action. Preserve required routes, text, visibility, enabled states and interactions. Choose input types and validation behavior from the requirements; hidden views, dialogs and dynamic rendering are allowed when needed.
- Keep IDs unique and label associations correct. Repeated text and links can be valid. If an actual locator is ambiguous, inspect its scope and the intended interaction instead of deleting unrelated content.
- A control repeated once per item needs an accessible name that says which item it acts on. Identical names across items leave a name-based lookup resolving to an arbitrary one, and a control that stays exposed after the pointer leaves its item makes that worse.
- Keep simultaneously available controls independently operable by pointer and keyboard. When adding controls, update their shared layout so their hit areas do not overlap and intercept each other's input.
- Derive state ownership and persistence from requirements: distinguish per-view, per-session and shared data. Do not reset persisted user data on startup. For persistent data, initialize required records only for a new store or an explicit migration. Later startups must preserve user edits, deletions and archive state; a missing record does not mean the store is new. Reset data only when the requirements explicitly demand it. Provide a loading state when initialization is asynchronous.
- Use local assets where practical. Add styling, animation, asynchronous updates or external services when required; keep interactions responsive and report failures clearly.
- Use supplied visual references when relevant. Public tests are examples of required behavior, not permission to hardcode test outcomes or omit untested requirements.
"""

CODEGEN_SYSTEM = "You write complete, minimal web apps. Reply only with file blocks in the requested format."

CODEGEN_PROMPT = """\
Requirement {node_id}: {description}

Public acceptance example (implement the full requirement):
{spec}
Files: frontend/src/index.html (+ one html per further route); backend/server.js = CommonJS (require) Node http server, never redeclaring the CommonJS globals (__dirname, __filename, require, module, exports), on process.env.PORT||{port} serving ../frontend/dist files (index.html for /, <name>.html for /<name>) plus any API routes and persistence the requirement needs, 404 for anything else, handling request errors without hiding unexpected process failures.{ports} Initial package.json files already exist (build copies src/* to dist; start runs server.js). Preserve existing architecture; update manifests when required by dependencies or build changes.
For persistent data, initialize required records only for a new store or an explicit migration. Later startups must preserve user edits, deletions and archive state; a missing record does not mean the store is new. Reset data only when the requirements explicitly demand it.
Rules: implement the requirement for general valid inputs and preserve existing behavior. Use required labels and accessible controls, with unique IDs and correct label associations. Derive storage, rendering, styling and validation from the task; do not hardcode test outputs. Return only requested file blocks. {size_rule}
"""

CODEGEN_SIZE_SMALL = 'Prefer a small implementation, but do not omit required behavior, accessibility, styling or validation to meet an arbitrary line count.'
CODEGEN_SIZE_FULL = "Keep the implementation concise while preserving all required behavior and the existing architecture. Derive navigation, authentication, storage and validation from the requirements. Public tests illustrate contracts; handle other valid inputs too. Do not force a navigation placeholder, cookie name, redirect, validation message or rendering strategy. Fix actual ambiguous controls in their intended scope without deleting legitimate repeated links or text. Keep simultaneously available controls independently operable by pointer and keyboard. When adding controls, update their shared layout so their hit areas do not overlap and intercept each other's input."

# Tiny-spec tier (OCTOS_ARC_TINY_SPEC_CHARS, default 1500; OCTOS_ARC_TINY=0 disables): the prompt is the
# spec's own statements only, the reply is one HTML file, the server is a fixed harness scaffold (no task
# logic), thinking is off. First-pass failure falls back to the compact codegen tier for the same node.
TINY_SYSTEM = 'Reply with HTML only. Honor supplied selectors and accessible names. getByTestId targets data-testid; it does not target id.'

TINY_PROMPT = """\
Task and public acceptance example (implement general behavior):
{spec}
Reply with the page markup only: a concise self-contained page implementing the full task, including required styling, controls and state. Do not hardcode test outputs.
"""

TINY_PROMPT_EVOLUTION = """\
Current index.html:
{page}
Task and additional acceptance example (preserve existing behavior):
{spec}
Reply with the complete updated page markup only: a concise self-contained page implementing the full task, including required styling, controls and state. Do not hardcode test outputs.
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
- Treat examples as examples unless the requirement explicitly identifies initial records or enumerated values. Implement general handling for other valid inputs. Preserve required initial data without overwriting existing user data; do not invent broad lists or fixed sample accounts.
"""

UI_CONTRACT_SESSION = """\
- Derive authentication routes, redirects, labels and session lifetime from the requirements and existing app. Keep authentication state isolated between users; preserve sessions only as required. Failed authentication must not create a session or mutate protected data. Choose error disclosure appropriate to the security requirements.
"""

UI_CONTRACT = UI_CONTRACT_CORE + UI_CONTRACT_DATA + UI_CONTRACT_SESSION  # full set (multi-node tasks)

PERFORMANCE_CONTRACT = """\
Performance and robustness:
- Measure slow operations using observed timings and the configured runtime budgets; do not assume a fixed CPU slowdown or browser count. A timeout can reflect a missing element, incorrect state or an unresolved request; inspect the actual failure before changing performance settings.
- Keep request handlers responsive and avoid unnecessary work. Do not introduce password-hashing implementations or reduce cryptographic strength to improve timings. Follow the required authentication behavior, using an existing authentication service when available; do not replace authentication with plaintext password storage or bypass credential checks.
- Set cookie flags, scope and lifetime from the deployment protocol and session requirements. Use HttpOnly for session cookies and Secure on HTTPS; do not hardcode a host or lifetime. Preserve required client-side interactions and persistent storage semantics.
"""

ARCHITECTURE_CONTRACT = """\
Runtime integration:
- Preserve the platform contract: frontend/ has npm run build producing frontend/dist/; backend/ has npm start and reads PORT (default {port}). Within that contract, preserve the existing application architecture and choose libraries or storage appropriate to the requirements and available environment.
- Prefer existing dependencies and avoid unnecessary installation. Do not prohibit frameworks, native modules or durable storage when the task requires them. Use the configured package registry.
- Handle expected request errors with appropriate responses, including 404 for missing resources. Log unexpected failures; do not suppress uncaught exceptions and continue serving potentially corrupt state. Preserve data integrity and use the runtime's recovery mechanism.
"""

VERIFY_FULL = """\
Verify briefly before you finish — the harness runs the official acceptance tests for this node right after your turn and hands you the failures, so do not build your own test suite: `npm run build` in frontend/, start the backend with `ARC_EXTRA_PORTS=0 PORT={smoke} npm start`, one curl per new endpoint (one success, one error case), stop the server.
"""

VERIFY_MINIMAL = """\
You have no shell in this turn. The harness runs `npm run build`, starts the backend and runs the official Playwright specs after your turn, then supplies any failures. Work within the configured request and output budgets. Preserve the existing application structure and create or edit the files needed by the requirements; do not combine unrelated modules merely to reduce file count. Use the supplied file listing and source evidence first, and inspect additional files when needed to resolve uncertainty. Batch independent small edits where practical, split changes that would exceed the response budget, and avoid rereading unchanged files without a reason. Check syntax and imports before finishing, then give a brief summary.
"""

PORT_RULES = """\
Ports: run your own smoke servers ONLY with `ARC_EXTRA_PORTS=0 PORT={smoke} npm start` (port {smoke}). NEVER bind port {port} — the runner watches it and terminates the run. Stop every server you started before you finish. Do not run git; the harness commits.
For a background server in a one-shot shell, redirect the entire command group, including stdin, so descendants cannot hold the tool's capture pipes open:
```sh
(cd backend && exec env ARC_EXTRA_PORTS=0 PORT={smoke} npm start) < /dev/null > smoke-server.log 2>&1 &
echo $!
```
Set the shell tool workdir to the application directory and run this command unchanged. Keep every cd inside the redirected parentheses; prepending cd ... && outside them creates another background shell that retains the capture pipes. Retain the printed PID for cleanup; inspect smoke-server.log and confirm HTTP readiness before testing. Do not assume a successful background launch means the app is ready. Rebuild frontend/ after source changes. Stop your server before ending the turn so the harness can start its own.
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

CODEGEN_REPAIR_SUFFIX = """
This request has no tools. Use the supplied acceptance specification and helpers below as read-only evidence; do not emit read/shell instructions or claim to have executed them. Return every application file you change as a complete file block. The harness executes acceptance after your response.
{spec}
"""

REPAIR_PROMPT = """\
The official acceptance tests for requirement node {node_id} just ran against your app: {passed}/{total} passed. Failing tests (Feature / where it failed / what was observed / the last steps before failure):
{failures}
{test_location}
{corrections}{slow}{sources}
Fix frontend/ and/or backend/ so these tests pass without breaking the passing ones. Work within the configured request budget. Use the supplied evidence to identify the cause, read relevant sources when needed, and make focused edits. For a failed post-action assertion, trace the preceding actions and identify the element and record actually acted on. With repeated controls, inspect locator scope, ordering, visibility, and hover/focus state before assuming a storage or rendering failure. Preserve keyboard access and the required interaction semantics when resolving ambiguity. Preserve behavior beyond the tested inputs. The harness rebuilds and re-runs the official tests right after your turn. The spec files are read-only ground truth.
For persistent data, initialize required records only for a new store or an explicit migration. Later startups must preserve user edits, deletions and archive state; a missing record does not mean the store is new. Reset data only when the requirements explicitly demand it.
""" + PORT_RULES

FINAL_CHECK_PROMPT = """\
Final end-to-end check of the web application in the current directory:
1. `npm run build` in frontend/ — fix any error.
2. Kill leftover servers, start the backend with `ARC_EXTRA_PORTS=0 PORT={smoke} npm start`, confirm `curl http://127.0.0.1:{smoke}/` serves the app and every API endpoint answers (success and error cases).
3. Audit required flows and states against the contracts below. Check accessible names, unique IDs and correct label associations. Resolve observed locator ambiguity in its intended scope; repeated text and destinations can be legitimate.
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
PUBLIC ACCEPTANCE TESTS (examples to validate the full requirement; report conflicts instead of silently discarding requirements) live under {tests_dir}. Files: {files}. They define routes, hrefs, accessible names, option labels, exact texts, error wording and action order. Never modify, copy or delete them.
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


TRUNCATION_CORRECTION = (
    "Your previous response was cut off by the output limit and nothing was saved. "
    "Write exactly ONE file per response, complete, and stop -- do not try to emit "
    "several files in one turn."
)


def truncation_correction(ok: bool, text: str) -> str | None:
    """Tell the next round that the last one died on the output limit.

    The implement turn already retries once on truncation ("one file per response").
    Repair and rewrite turns had no handling at all, so a truncated repair burned the
    round silently and the node then ran out of budget.

    Seen on ticket-booking with glm-5.3-flash: the rewrite turn ran 582s and came back
    `output_truncated: Model output was truncated (max_tokens); the response is
    incomplete`, with `completion_tokens` exactly 32768 -- the proxy's floor, which the
    kernel's own 4096 is raised to. Nothing was written, REQ-1 never passed, and the
    node hit its 1500s cap. A reasoning model spends part of that budget on reasoning,
    so the room left for the file is smaller than the number suggests.

    This adds no retry (the round budget is already spent); it makes the *next* round
    aware, which is the cheap half of the fix.
    """
    if ok or "truncated" not in (text or "").lower():
        return None
    return TRUNCATION_CORRECTION


def repair_wrote_nothing_correction(ok: bool, text: str) -> str | None:
    """A repair turn whose reply contained no file blocks at all.

    `truncation_correction`'s docstring already noted that "repair and rewrite turns
    had no handling at all" and fixed the truncation half on those two call sites.
    The no-blocks half was left behind: `no_files_correction` is only applied on the
    implement path, so a repair reply that lost everything on the wrapper was told
    nothing and the next round repeated it. Seen directly on a local ticket-booking
    run -- the repair returned a ```json fence with no delimiters, nothing reached
    disk, and the following round reported the identical failure.

    Why not just reuse `no_files_correction` here: that message ends with "Ignore any
    suggestion that your previous files failed; there were none." On the implement
    path that is true. On a repair round it is false and actively misleading -- there
    *are* previous files, from the implement turn, and they *did* fail. So this states
    the wrapper problem while keeping the failing-code context intact.
    """
    if ok or "no <<<FILE>>> blocks" not in (text or ""):
        return None
    return ("Your last repair wrote no files: the reply contained no "
            "<<<FILE path>>> ... <<<END FILE>>> blocks, so nothing reached disk. The "
            "application is still exactly the code that just failed, and the failures "
            "you were given still stand -- they are not evidence about whatever fix you "
            "had in mind, because that fix was never applied. Send the change again and "
            "wrap every file you touch exactly as:\n"
            "<<<FILE relative/path>>>\ncontents\n<<<END FILE>>>\n"
            "Return each file whole, not a diff or a fragment, and do not put the "
            "blocks inside a markdown code fence.")


def no_files_correction(text: str) -> str | None:
    """Feedback that names the real problem when a turn wrote nothing at all.

    Seen on both local models: the turn produced complete, correct code and lost
    it on the wrapper. qwen2.5-coder:7b returned a whole working counter page
    fenced as ```html with no delimiters; the 1.5B returned five blocks whose
    opening markers were a character short. Nothing reaches disk, the app is
    unchanged, and every spec fails.

    What the model was told next was misleading twice over: the generic
    correction says the implementation "did not complete", and the repair prompt
    opens with "Your previous files (quoted below) failed every test". There were
    no previous files, and the code may well have been right -- so the model is
    sent looking for a logic bug that does not exist while the actual defect, the
    wrapper, goes unmentioned.

    Returns None for every other kind of failure, so the generic correction still
    applies where it is accurate.

    One case had to be added after watching a real bundle run: glm-5.3-flash
    answered `The existing implementation already fully satisfies REQ-1 (count
    starts at ...` and returned no files, which is the *right* answer -- the tiny
    tier had already written a correct page. The first version of this message
    told it to "return every file you change", which pushes a model that
    correctly changed nothing into rewriting a working file. So the message now
    names both cases and says the tests will confirm the no-change one.
    """
    if "no <<<FILE>>> blocks" not in (text or ""):
        return None
    return ("Your last reply wrote no files: it contained no <<<FILE path>>> ... <<<END FILE>>> "
            "blocks, so nothing reached disk and the application is unchanged -- the code itself "
            "may have been correct. Ignore any suggestion that your previous files failed; there "
            "were none. Two cases, and only you know which applies: if you did mean to change "
            "something, return each changed file whole, wrapped exactly as:\n"
            "<<<FILE relative/path>>>\ncontents\n<<<END FILE>>>\n"
            "If instead the requirement is already satisfied by the code as it stands, say so in "
            "one line and change nothing -- the acceptance tests will confirm it. Do not rewrite a "
            "working file merely to produce output.")


def unchanged_correction(files: dict[str, str], idle: list[str]) -> str | None:
    """Tell the next round that the last one returned the same bytes it was given.

    There are exactly three ways a reply can fail to move the application forward,
    and until now only two of them said so:

      no blocks at all        `no_files_correction`
      truncated mid-file      `truncation_correction`
      every file identical    nothing -- the turn logged one line and reported success

    The third is the quietest and the most expensive. `unchanged_rewrites` already
    computes it (it is why that helper exists), but the result went only to a log
    line while `codegen_turn` returned True; the outer loop then paid a full spec
    run to discover the app had not changed, and handed the next round the same
    evidence, which invites the same reply. `last_repair_diff` catches the
    cross-round case afterwards from git, but only for repair commits and only
    once the round is already spent.

    Deliberately not treated as a failed turn. The turn *did* complete, and
    "the file is already correct" is a legitimate conclusion -- the lesson from
    `no_files_correction` was that pushing a model which correctly changed nothing
    into rewriting a working file makes things worse. So this names both cases and
    asks for the one thing a repeat cannot provide: where else the failure could be.

    Returns None unless *every* returned file was byte-identical. A reply that
    changed one file and re-sent three untouched ones did make progress, and the
    existing `(N unchanged: ...)` log line is the right amount of attention for it.
    """
    if not files or len(idle) != len(files):
        return None
    shown = ", ".join(sorted(idle)[:4])
    more = f" (and {len(idle) - 4} more)" if len(idle) > 4 else ""
    return (f"Every file your last reply returned was byte-for-byte identical to what was already "
            f"on disk -- {len(idle)} file(s): {shown}{more}. Nothing changed, so the same failures "
            f"will reproduce exactly; sending those bytes again cannot move the result. Two cases, "
            f"and only you know which applies: if you meant to make an edit, it did not survive -- "
            f"make it and return that file whole. If you concluded the file is already correct, do "
            f"not return it a third time; say so in one line and name where else the failure can "
            f"come from -- a different file, the server wiring, the seeded data, or the test's own "
            f"assumption about ports or selectors.")


def phase_for_label(label: str) -> str:
    """Which routing phase a turn belongs to, from the label it was given.

    This decides which model a request is sent to whenever model routes are
    configured, and it was an inline conditional with no test of its own -- the
    routing rules were covered, the thing that feeds them was not. A rule that
    says `"phases": ["repair"]` is worth exactly as much as this function's
    agreement about what a repair is called.

    The labels are the ones the flow actually emits: `REQ-3.2 implement`,
    `REQ-3.2 repair 1/5`, `REQ-3.2 rewrite (repair 1)`, `full-suite repair 1/3`,
    `final check`, `design`.
    """
    if any(word in label for word in ("repair", "rewrite")):
        return "repair"
    if "final check" in label:
        return "verify"
    if "design" in label:
        return "design"
    return "implement"


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


FAILURE_FIELD_PREFIXES = ("- Feature:", "Feature:", "Failed at:", "Observation:", "Steps:")


def failure_console_lines(failures: str, max_chars: int = 800) -> list[str]:
    """Console lines for one acceptance failure block.

    `Observation:` carries the build/start log tail on the lines that follow it,
    and a prefix-only filter dropped all of them -- a node that died with
    `npm start exited early (rc=1):` logged that and nothing about why. The
    continuation is folded into the observation and clipped at both ends, so
    the first line (which names the error) survives a long traceback.
    """
    lines = (failures or "").splitlines()
    out: list[str] = []
    index = 0
    while index < len(lines):
        stripped = lines[index].strip()
        if stripped.startswith("Failed at:"):
            out.append(" ".join(stripped.split())[:max_chars])
        elif stripped.startswith("Observation:"):
            body = [stripped]
            index += 1
            while index < len(lines) and not lines[index].strip().startswith(FAILURE_FIELD_PREFIXES):
                body.append(lines[index].strip())
                index += 1
            out.append(clip_ends(" ".join(" ".join(body).split()), max_chars))
            continue
        index += 1
    return out


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

def regression_checkpoint_due(index: int, total: int, start: int) -> bool:
    """Start with short checks, then cap gaps at twice the interval; skip the final node.

    The last interval always checks. Cloud 3ffe9702bf15 / 746c81a2b5aa run 32
    nodes, so the doubling leaves nodes 25-32 unchecked until the full suite --
    the widest gap, over the most layered state, right where a regression costs
    the most to find.
    """
    if start <= 0 or index < start or index >= total or index % start:
        return False
    multiple = index // start
    return multiple == 1 or multiple % 2 == 0 or index + start >= total


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
        # 1500 → 3000。这个上限的作用是「别让一个节点吃掉全部预算」，
        # 但那件事 `remaining / nodes_left` 已经在做了——它每个节点重算一次，
        # 超支会自动把后面的份额压下去。而这个 **硬上限** 做的是另一件事：
        # 它把快节点省下来的余额**锁住不让慢节点用**。
        #
        # keep @ D 的实测，直接说明代价（2026-09-19）：
        #     全程预算 48,000s，实际用掉 30,240s —— **剩 17,760s（37%）没用**
        #     而六个失败节点各自只超出单节点上限 13–18 秒就被砍断：
        #     implement 900s（0.6 × 1500 的上限）+ 验收循环 613s = 1513s > 1500s
        #     于是 `left < min_repair_seconds` 成立，修复停在 round 1，节点判负。
        # 六个节点在 37% 的余额面前，因为差十几秒而失败。
        #
        # 抬到 3000 之后，`node_timeout`（1200）会先卡住 implement 轮，
        # 剩下的 ≥1800s 归验收循环——够两到三轮修复，而不是恰好一轮。
        # 上限仍在（不是取消），且 `remaining / nodes_left` 继续保护后面的节点。
        #
        # 2026-09-19 用实测数据量化了这个改动到底消掉多少。先把饥饿写成一个判据：
        # 节点跑完第一轮（实测约 600s）后，余额还要 ≥ OCTOS_MIN_REPAIR_SECONDS(300)
        # 才起得了第二轮修复，所以
        #     被挤死 ⟺ node_budget − implement < 600 + 300 = 900s
        # 代入云端实测的 implement 耗时（相邻 test 时刻之差再减去验收循环）：
        #
        #     场景              节点   cap1500 预测   cap3000 预测   实际失败
        #     5 路并发 keep      31    8（26%）       0（0%）        6
        #     隔离 keep          31    1（3%）        0（0%）        0
        #     4 路并发 12306     72    36（50%）      0（0%）        判定中约 23/73
        #
        # 三个场景都对得上（预测略高于实际——有些被挤的节点第一轮就过了，
        # 所以它是上界，方向正确）。cap 3000 下三个场景预测被挤死的都是 **0**，
        # 连 5 路并发也不例外，即这个机制被整个消掉而不只是缓解。
        #
        # 根因是 implement 撞上 `0.6 × node_budget_cap` 这个上限：同一道 keep，
        # implement 中位数随并发度从 84s（隔离）涨到 204s 再到 390s（5 路），
        # 触顶 900s 的节点从 3% 涨到 13% 再到 26%——所以并发才是主因，不是应用体积。
        self.node_budget_cap = int(os.environ.get("OCTOS_NODE_TIME_BUDGET", "3000"))
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
        self.checkpoint_regressions: set[str] = set()
        self.impl_failed: list[str] = []
        self.pending_corrections: list[str] = []
        # 上一个 codegen 轮到底有没有把文件写到盘上。None = 未知（工具模式经工具写，
        # 不走块解析），False = 确定什么都没写。只有 False 会改变判定，所以
        # 工具模式的行为逐字不变。
        self.last_codegen_wrote: bool | None = None
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

    def inline_source_chars(self) -> int:
        """How much of the app to quote into a turn that edits with tools.

        This is an input budget and the context window bounds it;
        `codegen_context_chars` is an output budget, bounding what a tool-free
        turn is asked to re-emit. They default to the same number and are easy
        to mistake for one thing, but raising that one to quote more source
        would also start asking codegen turns for larger files than they should
        be asked for. Keep them separate so either can move on its own.

        The default is unchanged. Whether a larger share of a 1048576 token
        window helps a repair more than it dilutes it is not something the
        harness can answer offline; it needs a cloud run against the same task.

        What is measured, on the four large tasks of 2026-09-16, each about
        halfway through its nodes:

            task           nodes    files  source   quoted  omitted  seen
            stackoverflow  32/66    14     152050   4       10       63%
            prestashop     44/86    15     115636   7        8       79%
            12306          48/117   10     110394   5        5       86%
            ctrip          58/125    7     109559   3        4       86%

        Every repair prompt on those tasks already hides four to ten files, and
        the sources roughly double again by the last node. The budget they are
        competing for is about 22000 tokens of a 1048576 token window. The
        number was chosen as an output budget for codegen re-emission, not as
        an input budget, so raising it would not overturn a measured decision --
        but it would still be a guess until a cloud A/B says otherwise.

        Scope, before that A/B is priced: `sources_text` is read at three call
        sites and all three are repairs. A tool-mode implement turn is given no
        quoted source, but it is not left blind: it gets `source_listing` --
        every app file with its size -- and is told to read backend/server.js
        and the page it extends. That is deliberate, and the tool events match
        it exactly, so do not "fix" implement turns by quoting sources into
        them. On the same stackoverflow run the split was 33
        implement turns to 4 repairs, so the omission above reaches about a
        tenth of the turns -- the ones that decide the score, but a tenth. Its
        tool events show 148 reads, and `backend/server.js` alone accounts for
        28 of the last 60; most of those fall in implement turns that were
        never handed the file, so they are not the prompt failing to save a
        read.
        """
        return int(os.environ.get("OCTOS_ARC_INLINE_SOURCE_CHARS", str(self.codegen_context_chars())))

    def sources_text(self) -> str:
        # A repair has to understand the code before editing it, so it cannot be
        # quoted less than the turn that wrote the code was.
        limit = self.inline_source_chars()
        return inline_sources(self.output_dir, limit) + "\n" if limit > 0 else ""

    def corrections_text(self) -> str:
        if not self.pending_corrections:
            return ""
        text = "Corrections from the harness:\n" + "\n".join(f"- {c}" for c in self.pending_corrections) + "\n"
        self.pending_corrections = []
        return text

    def turn(self, prompt: str, timeout: int, label: str, expect_verification: bool = True,
             request_budget: int | None = None) -> tuple[bool, str]:
        proxy = getattr(self, "llm_proxy", None)
        # `verify_text` has the proxy drop the shell tools in minimal mode. A turn
        # that cannot run a command must not then be told off for not running one.
        no_shell = bool(getattr(proxy, "extra_drop_tools", None))
        monitor = TurnMonitor(self.protected_prefixes(),
                              expect_verification=expect_verification and not no_shell,
                              allowed_prefixes=[".arc/design/", str(self.output_dir / ".arc" / "design")])
        if proxy is not None:
            # Per-turn reasoning: OCTOS_ARC_IMPLEMENT_REASONING (e.g. "none") applies
            # to first implement turns of small tasks; rewrite/repair keep the base mode.
            base_mode = getattr(self, "base_reasoning_mode", proxy.mode)
            impl_mode = os.environ.get("OCTOS_ARC_IMPLEMENT_REASONING", "")  # auto already gives "none" to 1-node tasks
            is_implement = label.endswith(" implement") or label.startswith("skeleton")
            proxy.mode = impl_mode if (impl_mode and is_implement and self.minimal_mode(getattr(self, "n_nodes", 99))) else base_mode
            if request_budget is None:
                # A repair has to understand a failure before it can edit, so it
                # cannot need less room than the turn that wrote the code. Both
                # follow the same default; the per-turn timeout and the cost
                # guard still bound them. Cloud 746c81a2b5aa and 3ffe9702bf15 hit
                # the old fixed cap of 10 on 17% and 23% of their repair turns —
                # including the last repair of every node that stayed broken —
                # while spending 7% of the token budget and 7% of the time.
                default = "20" if self.minimal_mode(getattr(self, "n_nodes", 99)) else "0"
                request_budget = int(os.environ.get("OCTOS_ARC_REPAIR_REQUESTS", default)) if "repair" in label else \
                    int(os.environ.get("OCTOS_ARC_IMPLEMENT_REQUESTS", default))
            proxy.phase = phase_for_label(label)
            proxy.begin_turn(request_budget)
        # A model turn may change application files, even when it later fails.
        getattr(self, "probe_summaries", {}).clear()
        t0 = time.time()
        self.turn_count += 1
        ok, text = self.driver.run(prompt, max(60, int(timeout)), monitor)
        elapsed = time.time() - t0
        log(f"[flow] {label} {'ok' if ok else 'FAILED'} in {elapsed:.0f}s "
            f"(tools={monitor.tool_calls} wrote={monitor.wrote_files} verified={monitor.verified}): {text[-240:]!r}")
        # 记下修复轮真实花了多久，**按模式分桶**。`repair_needs()` 用它判断
        # 「还够不够再来一轮」——那个判断原先用一个固定的 300 秒，实测一轮约 600 秒。
        #
        # 为什么必须分桶：本仓库自己的记录是「工具模式单节点慢 19.6 倍（392s vs 20s）」。
        # 混在一个中位数里，一轮工具模式修复就会把门槛抬到天上去，
        # 于是后面的节点连一轮**便宜的** codegen 修复都不敢起——那正好是反效果。
        if "repair" in label or "rewrite" in label:
            mode = "tool" if getattr(self, "codegen_blocked", False) else "codegen"
            seen = getattr(self, "repair_durations", None)
            if not isinstance(seen, dict):
                seen = {}
            seen[mode] = (seen.get(mode, []) + [elapsed])[-12:]
            self.repair_durations = seen
        if not ok and permanent_provider_error(text):
            raise PermanentProviderError(text[:1000])
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

    def slow_test_ms(self) -> int:
        """Half the budget a test is graded against.

        The threshold was a flat 3000 ms, tuned when the harness also imposed a
        4000 ms action deadline — a test near 3 s was then near failing. With the
        deadlines matched to the grader (10 s per test) it is not: in cloud
        3ffe9702bf15 three of the twenty-five tests the grader passed ran between
        3.4 s and 3.9 s, and each was handed to the model as something to
        optimise. Editing passing code to make it faster is a regression risk
        taken for nothing. Above half the budget there is real cause to look.
        """
        configured = os.environ.get("OCTOS_ARC_SLOW_MS")
        if configured:
            return int(configured)
        timeout = getattr(getattr(self, "runner", None), "timeout_ms", 10000)
        return max(1000, timeout // 2)

    def perf_text(self) -> str:
        return PERFORMANCE_CONTRACT if self.perf_contract and self.needs_session else ""

    def classify_tree(self, tree: dict) -> None:
        """Keyword-gate the optional contract blocks so a counter never reads
        session/performance rules; derive behavior from the supplied requirements."""
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
        """One-request generation per node (OCTOS_ARC_CODEGEN=0 disables; OCTOS_ARC_CODEGEN_MAX_NODES caps the
        tree size, default unlimited). Per node, `codegen_context_fits` decides whether the spec plus the
        relevant sources fit the prompt budget; otherwise that node uses tool mode."""
        return (os.environ.get("OCTOS_ARC_CODEGEN", "1") != "0" and getattr(self, "llm_proxy", None) is not None
                and not getattr(self, "codegen_blocked", False)
                and getattr(self, "n_nodes", 99) <= int(os.environ.get("OCTOS_ARC_CODEGEN_MAX_NODES", "999")))

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

    def codegen_context_chars(self) -> int:
        return int(os.environ.get("OCTOS_ARC_CODEGEN_CONTEXT_CHARS", "90000"))

    def codegen_quote_chars(self) -> int:
        """How much existing source an implement turn may be handed.

        Split out of `codegen_context_chars()` so the two can be measured apart.
        That one number was doing four jobs at once -- quoting budget, inline
        budget, whole-prompt ceiling, and the spec-too-large test -- so an
        experiment that moved it moved all four.

        It had to be moved to learn anything: on smoke-evolution--counter with a
        seeded 65 KB app, qwen2.5-coder:7b scored 2/2 three times out of three at
        9,000 and 1/2 three times out of three at 40,000, and the large-budget
        failures all looked the same -- not one file marker emitted, the whole
        reply discarded. That symptom is on the input side; a bigger *output*
        allowance should be more permissive, not less. But with one knob for both
        it stays an inference.

        Defaults to `codegen_context_chars()`, so nothing changes until someone
        sets OCTOS_ARC_CODEGEN_QUOTE_CHARS. The point is to make the separating
        experiment possible, not to guess the answer before running it.
        """
        return int(os.environ.get("OCTOS_ARC_CODEGEN_QUOTE_CHARS", str(self.codegen_context_chars())))

    def codegen_source_fit_chars(self) -> int:
        """How much existing source may be quoted before a node falls to tool mode.

        Deliberately not `codegen_context_chars()`: that one is the *output* budget
        bounding what a tool-free turn is asked to re-emit (#192 separated the two, and
        this gate was the one place still conflating them). Sizing the gate by the output
        budget means the app outgrowing 90000 characters pushes every later node into tool
        mode, and tool mode is what actually costs the run.

        Measured on cloud stackoverflow 97848d542ac8 (66 nodes, 2026-09-17), at node 44:

            path                  nodes   median   total
            codegen, 1 request    20        65 s   31.7 min
            tool mode             24       574 s   244.0 min

        55% of the nodes, 89% of the node wall-clock, 8.8x the median per node -- and the
        split is chronological: early nodes fit, later ones do not. First-pass rate was
        44/44, so this is not repair churn, it is the tool-mode path itself. The finished
        run put it at 65% of nodes, 90% of wall-clock, 19.6x the median (392 s vs 20 s).

        Raising this does not make the prompt bigger -- a codegen turn still quotes only
        `relevant_sources(..., codegen_context_chars() - len(spec))`. It decides
        eligibility, not prompt size.

        **Reverted to the output budget on 2026-09-18: raising it bought speed with
        correctness, and correctness is the eligibility gate.** The 400000 default was
        submitted as 95da0dc11e93 and measured against 55b63aa8e5ac's runs of the same
        tasks (`arc/postmortem.py` classifies lost nodes):

            run                        path            result   regressed  never-passed
            stackoverflow 97848d542ac8 65% tool mode   66/66    0          0
            stackoverflow 34ca94da0075 100% codegen    33/49*   9          7
            12306         99196f2e802b 100% codegen    46/75*   29         0
            (* mid-run)

        The tool-mode run finished with **zero** regressions; the codegen-only runs
        regressed 9 and 29 nodes, and 12306's losses were *entirely* regressions -- not one
        node it could not build, only nodes it built and then broke.

        That is this function's own invariant being violated, and the docstring of
        `codegen_context_fits` below states it: do not request complete file replacements
        with omitted source bodies. A node that can see only part of a shared file (the
        backend entry of a 117-node app is far past any budget) and is asked to return that
        file complete will drop the handlers it never saw -- which belong to other nodes'
        specs. Tool mode is slow precisely because it reads and edits in place instead.

        So tool mode's 19.6x per-node cost is the price of not losing behaviour on a large
        tree, and the measurements above say it is worth paying: `efficiency_eligible` on
        the leaderboard is pass rate >= 80%, so a cheaper run that drops below it scores
        nothing at all.

        Keeping the env knob: a gate sized by the *files a node will actually rewrite*
        (rather than the whole app) would recover most of the speed without breaking the
        invariant, but that is a different change and is unmeasured.

        The repair-side budget (`inline_source_chars`, #199) is left alone on purpose:
        that one is about whether more quoted source helps or dilutes a repair, which is a
        different question and still unmeasured.
        """
        return int(os.environ.get("OCTOS_ARC_CODEGEN_SOURCE_FIT_CHARS",
                                  str(self.codegen_context_chars())))

    def codegen_context_fits(self, spec_text: str) -> bool:
        """Do not request complete file replacements with omitted source bodies.
        Large existing applications use tool mode so the model can read and edit
        their files without fitting every source into one request.
        """
        limit = self.codegen_source_fit_chars()
        if len(spec_text) >= self.codegen_context_chars() * 0.6:
            return False
        remaining = max(8000, limit - len(spec_text))
        for path in app_source_files(self.output_dir):
            try:
                remaining -= len(path.read_text(encoding="utf-8", errors="replace"))
            except OSError:
                return False
            if remaining < 0:
                return False
        return True

    def codegen_repair_prompt(self, node_id: str, prompt: str) -> str | None:
        """One-request repair prompt, or None when it cannot be built safely.

        `codegen_repair_block` records which of the three unrelated conditions
        took the expensive branch, because they call for different responses: a
        node with no spec is nothing to act on, a clipped source means the app
        has outgrown the context budget, and an oversized prompt says by how
        much. The run log used to report all three as one sentence with no
        numbers.
        """
        self.codegen_repair_block = ""
        spec = self.spec_bodies(node_id)
        if not spec or spec == "(none)":
            self.codegen_repair_block = "no spec to quote for this node"
            return None
        # Tool-free repairs must see the source instead of instructions to read it.
        # Requote using the total context allowance, then check the complete prompt
        # (including requirements, acceptance, headings and format instructions).
        current_sources = self.sources_text()
        if current_sources.strip() and current_sources in prompt:
            sources = inline_sources(self.output_dir, self.codegen_context_chars()) + "\n"
            # A clipped file is no safer here than an omitted one: this turn has
            # no tools, so it cannot read the part it was not shown, and it is
            # asked to re-emit the file whole. Fall back to a tool-based repair.
            if any(line.startswith("--- ") and (" --- (omitted," in line
                                                or " --- (too large to quote whole," in line)
                   for line in sources.splitlines()):
                self.codegen_repair_block = (
                    f"app sources no longer quote whole ({app_source_chars(self.output_dir)} source chars "
                    f"vs {self.codegen_context_chars()} allowed)")
                return None
            prompt = prompt.replace(current_sources, sources, 1)
        full = prompt + CODEGEN_REPAIR_SUFFIX.format(spec=spec)
        size = len(full) + len(FORMAT_INSTRUCTIONS)
        if size > self.codegen_context_chars():
            self.codegen_repair_block = f"repair prompt is {size} chars vs {self.codegen_context_chars()} allowed"
            return None
        return full

    def tiny_mode(self, spec_chars: int) -> bool:
        threshold = int(os.environ.get("OCTOS_ARC_TINY_SPEC_CHARS", "1500"))
        return os.environ.get("OCTOS_ARC_TINY", "1") != "0" and 0 < spec_chars < threshold

    def tiny_turn(self, node_id: str, specs: list[str], timeout: int, requirement: dict) -> bool:
        """Tiny-spec tier: harness writes the manifests and a fixed static server, the
        model returns one index.html for the spec's statements. Returns True only when
        the node's specs pass right away; otherwise the caller falls back to the compact tier."""
        write_codegen_manifests(self.output_dir)
        server = self.output_dir / "backend" / "server.js"
        if not server.exists():
            server.parent.mkdir(parents=True, exist_ok=True)
            extra = [p for p in spec_base_ports(self.tests_dir) if p != self.web_port]
            server.write_text(TINY_SERVER_JS.format(port=self.web_port, extra_ports=json.dumps(extra)), encoding="utf-8")
        spec = "Requirement: " + json.dumps(requirement, ensure_ascii=False) + "\nPublic example:\n" + self.spec_bodies(node_id)
        page = self.output_dir / "frontend" / "src" / "index.html"
        if page.is_file():
            prompt = TINY_PROMPT_EVOLUTION.format(page=page.read_text(encoding="utf-8", errors="replace").strip(), spec=spec)
        else:
            prompt = TINY_PROMPT.format(spec=spec)
        ok, _ = self.codegen_turn(prompt, timeout, f"{node_id} implement (tiny)", spec_chars=len(spec),
                                  system=TINY_SYSTEM, format_instructions="", raw_target="frontend/src/index.html")
        # Four different conditions used to print the same "produced no page", and only
        # one of them is that. Reading a bundle run where the page had just been written
        # ("wrote 1 file(s): ['frontend/src/index.html']" on the line above), the message
        # sent me looking for a bug that was not there -- the real reason was that the
        # task had no spec files to check the page against. Say which one fired.
        reason = ("the turn did not complete" if not ok else
                  "no frontend/src/index.html was written" if not page.is_file() else
                  "no runner to serve it" if self.runner is None else
                  "no spec files to check it against" if not specs else None)
        if reason:
            log(f"[flow] {node_id}: tiny tier unverified ({reason}); compact tier next")
            return False
        summary = self.run_specs(specs)
        passed = (not summary.error) and summary.total and summary.passed == summary.total
        log(f"[flow] {node_id}: tiny tier {'passed' if passed else 'failed'} its specs"
            f" ({summary.passed}/{summary.total})" if not summary.error else f"[flow] {node_id}: tiny tier could not run specs")
        if not passed:
            log(f"[acceptance] {node_id} first-attempt failure: {summary.error or failure_summaries(summary)}")
        return bool(passed)

    def codegen_reasoning(self, spec_chars: int) -> str | None:
        """Reasoning effort for a codegen turn, derived from the size of the spec it
        must satisfy (OCTOS_ARC_CODEGEN_REASONING_CHARS, default 5000): small specs are
        generated correctly without reasoning; large ones keep the base mode."""
        if os.environ.get("OCTOS_ARC_REASONING", "auto") != "auto":
            return None
        threshold = int(os.environ.get("OCTOS_ARC_CODEGEN_REASONING_CHARS", "5000"))
        return "none" if spec_chars and spec_chars < threshold else None

    def drop_unseen_rewrites(self, files: dict[str, str], label: str) -> dict[str, str]:
        """Refuse a whole-file replacement for an existing file the turn was not shown.

        A codegen turn is asked to return every file it changes, complete. When the
        source budget omitted a file, the turn cannot produce it correctly -- what it
        returns is a file written from nothing, and writing that deletes the behaviour
        other requirements depend on. Cloud 12306 99196f2e802b lost 29 nodes to
        regressions and not one to a spec it could not build.

        New files are allowed (nothing to destroy). Files that were quoted are allowed.
        Only "existing on disk, never shown" is refused, and the turn is told, so the
        next round asks for what it needs instead of silently losing it.
        """
        quoted = getattr(self, "codegen_quoted", None)
        if not files or quoted is None:
            return files
        kept, refused = {}, []
        for rel, body in files.items():
            if rel in quoted or not (self.output_dir / rel).exists():
                kept[rel] = body
            else:
                refused.append(rel)
        if refused:
            log(f"[codegen] {label}: refused {len(refused)} rewrite(s) of file(s) never shown "
                f"to this turn: {refused[:6]}; this node switches to tool mode")
            self.pending_corrections.append(
                "A previous turn returned complete replacements for files it had not been "
                "shown: " + ", ".join(refused[:6]) + ". Those were discarded, not written -- "
                "rewriting a file you cannot see deletes behaviour other requirements "
                "depend on. Change only files quoted to you; if you need another one, name "
                "it in one line and stop.\n")
            # Refusing alone would leave the node unable to finish: it asked for a file it
            # genuinely needs and got nothing. Tool mode is where that is possible -- it
            # reads and edits in place instead of re-emitting whole files, which is also
            # why it never had this failure mode. So the refusal is not a dead end, it is
            # the signal that this node belongs on the other path.
            self.codegen_blocked = True
        return kept

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
            with self.driver.without_tools():
                ok, text = self.turn((prompt + "\n" + format_instructions) if format_instructions else prompt, timeout, label,
                                     expect_verification=False,
                                     request_budget=int(os.environ.get("OCTOS_ARC_CODEGEN_REQUESTS", "3")))
        finally:
            proxy.no_tools = False
            proxy.system_override = None
            self.base_reasoning_mode = saved_base
        files = parse_file_blocks(text) if ok else {}
        drifted = delimiter_drift(text) if ok else []
        if drifted:
            log(f"[codegen] {label}: accepted {len(drifted)} block(s) with a non-canonical "
                f"delimiter: {drifted[:4]}")
        if ok and not files and raw_target:
            html = strip_code_fences(text)
            if looks_like_markup(html):
                files = {raw_target: html}
        files = self.drop_unseen_rewrites(files, label)
        if files:
            idle = unchanged_rewrites(self.output_dir, files)
            written = write_files(self.output_dir, files)
            same = f" ({len(idle)} unchanged: {idle[:4]})" if idle else ""
            log(f"[codegen] {label}: wrote {len(written)} file(s): {written[:8]}{same}")
            # Every file identical means this turn produced no change at all. Say so now,
            # rather than letting the next round rediscover it from a spec run that was
            # always going to fail the same way. Not a failed turn -- see the helper.
            self.last_codegen_wrote = bool(written)
            stalled = unchanged_correction(files, idle)
            if stalled:
                log(f"[codegen] {label}: no progress -- every returned file was byte-identical")
                self.pending_corrections.append(stalled)
            deduped = dedupe_nav_links(self.output_dir)
            if deduped:
                log(f"[codegen] {label}: removed static nav links duplicating the NAV placeholder in {deduped}")
            return True, text
        if ok:
            self.last_codegen_wrote = False
            log(f"[codegen] {label}: reply contained no file blocks; {unparsed_reply_digest(text)}")
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

    def repair_requirements(self, node_id: str | None = None) -> str:
        nodes = getattr(self, "requirement_nodes", {})
        selected = [nodes[node_id]] if node_id in nodes else ([] if node_id is not None else nodes.values())
        text = "\n\n".join(describe_node(node) for node in selected)
        if not text:
            return ""
        return ("Original requirements (read-only; preserve details even when acceptance does not assert them):\n"
                + text + "\n")

    def repair_test_location(self, specs: list[str] | None = None) -> str:
        if not self.tests_dir:
            return ""
        context = (f"Application directory: {self.output_dir.resolve()}. "
                   f"Read-only acceptance directory: {self.tests_dir.resolve()}. "
                   "Relative spec paths in failure reports refer to this directory. "
                   "Read relevant specs and helpers here when needed.\n")
        runner = getattr(self, "runner", None)
        if runner:
            helper = BUNDLE_DIR / "verify_app.py"
            binary = runner.root / "node_modules" / ".bin" / "playwright"
            if helper.is_file() and binary.is_file():
                args = ["env"]
                browsers = getattr(runner, "env_extra", {}).get("PLAYWRIGHT_BROWSERS_PATH")
                if browsers is not None:
                    args.append(f"PLAYWRIGHT_BROWSERS_PATH={browsers}")
                args.extend([sys.executable, str(helper), "--app", str(self.output_dir.resolve()),
                             "--tests", str(self.tests_dir.resolve()), "--playwright", str(runner.root)])
                workers = runner.workers if specs else workers_for_final(
                    getattr(self, "mem_limit", None), int(os.environ.get("OCTOS_ARC_FINAL_WORKERS", "4")))
                args.extend(["--workers", str(workers)])
                for spec in specs or []:
                    args.extend(["--spec", spec])
                context += ("Isolated acceptance entry (builds and starts a disposable application copy):\n"
                            f"```sh\n{shlex.join(args)}\n```\n"
                            "Use this command for acceptance checks so test writes do not alter the source application's data. "
                            "Edit the source application, not the disposable copy or read-only tests. "
                            "The command prints failures and a report path. The harness re-runs acceptance after your edits.\n")
        return context

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

    def last_repair_diff(self, max_chars: int = 900) -> str:
        """What the previous repair commit actually changed.

        The harness commits after every repair, so when a round reproduces the
        round before it, git can say whether anything moved and where. Cloud
        e767e871a6c6 ran four rounds over eleven failures that never budged, and
        the in-repo record of 91aaecaf31af and 5747e6bcf530 is that repeated
        repairs re-emit the same files. "You changed these and nothing moved" is
        a different instruction from "it failed again".
        """
        git = getattr(getattr(self, "runtime", None), "git", None)
        if git is None:
            return ""  # a diagnostic must never be what breaks the repair loop
        try:
            result = git.run(["diff", "HEAD~1", "HEAD", "--stat", "--", "frontend", "backend"], check=False)
        except Exception as exc:  # noqa: BLE001
            log(f"[git] could not read the last repair: {exc}")
            return ""
        stat = (getattr(result, "stdout", "") or "").strip()
        if not stat:
            return ("\n\nThe previous repair left frontend/ and backend/ unchanged, so this result is the "
                    "same code measured twice. Make an edit this time.")
        return ("\n\nThe previous repair changed this and the failures did not move:\n"
                + clip_ends(stat, max_chars))

    def unfinished_repair_note(self, text: str, max_chars: int = 700) -> str:
        """Hand the previous repair turn's closing words to the next round.

        A repair turn can end before it edits anything: reading one large source
        file eats the turn, or the per-turn timeout lands mid-plan. Cloud
        e767e871a6c6 lost all three full-suite rounds that way and finished at
        20/32 -- "No files were modified ... Next step is to apply those edits to
        frontend/src/index.html", then "Need one more turn to read the note-action
        click handlers". That run was capped at ten requests per repair, a cap
        since lifted, but the waste it exposed is not about the cap: each round
        said exactly where it had got to, the message was dropped, and the next
        round paid to read the same file again. Hand it forward. The conclusion
        sits at the end of the message, so keep the tail.
        """
        text = (text or "").strip()
        if not text:
            return ""
        if len(text) > max_chars:
            text = "… " + text[-max_chars:].lstrip()
        return ("\n\nWhere the last repair attempt stopped, in its own closing words. It ran on a tool "
                "budget and may have run out before it could edit anything, so treat this as work already "
                "done: continue from it rather than reading the same files again.\n" + text)

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
        routes_configured = bool(json.loads(configured_model_routes() or "[]"))
        if mode == "passthrough" and not routes_configured:
            return
        if not upstream.startswith("http"):
            if routes_configured:
                raise ValueError("model routing requires an HTTP provider endpoint")
            return
        try:
            dump = (self.output_dir / ".arc" / "llm-requests") if os.environ.get("OCTOS_ARC_PROXY_DUMP") == "1" else None
            self.llm_proxy = LlmProxy(upstream, mode, self.output_dir / ".arc" / "llm-usage.jsonl", dump_dir=dump,
                                      destream=os.environ.get("OCTOS_ARC_DESTREAM", "1") != "0",
                                      trim=os.environ.get("OCTOS_ARC_TRIM_PROMPT", "1") != "0",
                                      min_max_tokens=int(os.environ.get("OCTOS_ARC_MAX_TOKENS", "32768"))).start()
        except OSError as exc:
            if routes_configured:
                raise
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
        summary: RunSummary | None = None
        try:
            err = server.build()
            if err is None:
                err = server.start()
            if err is not None:
                return RunSummary(error=err)
            summary = self.runner.run(specs, f"http://127.0.0.1:{self.smoke_port}", workers=workers)
            return summary
        finally:
            server.stop()
            # Ask before restoring: afterwards there is nothing left to compare.
            if summary is not None:
                summary.stores_written = mutated_by_tests(git_run)
            restore_worktree(git_run)

    def record_tests(self, node_id: str, specs: list[str], summary: RunSummary) -> None:
        try:
            for r in summary.results:
                test_id = re.sub(r"[^A-Za-z0-9._-]+", "-", r.title)[:120]
                self.runtime.traceability.upsert_test(test_id=test_id, req_id=node_id, type="e2e",
                                                      file_path=r.file or None, passed=r.ok, emit_event=False)
        except Exception as exc:  # noqa: BLE001
            log(f"[trace] test rows not recorded: {exc}")

    def can_rewrite_from_scratch(self) -> bool:
        """A failing node does not justify replacing previously verified behavior."""
        return not (any(v is True for v in getattr(self, "test_verdict", {}).values())
                    or any(r.passed > 0 for r in getattr(self, "probe_summaries", {}).values()))

    def acceptance_loop(self, node_id: str, specs: list[str], deadline: float,
                        rebuild_prompt=None) -> bool | None:
        """Returns True/False for a real verdict, None when no local run happened.
        `rebuild_prompt(failures)` (optional) yields a full re-implementation
        prompt; it is used only before any behavior has passed verification.
        A failing extension is repaired without replacing working features."""
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
                failures = (f"- Feature: app startup\n  Failed at: build/start\n"
                            f"  Observation: {startup_error_digest(summary.error, 600)}\n"
                            f"  Steps: npm run build -> npm start")
                summary = RunSummary(passed=0, total=max(1, len(specs)))
                passed = 0
            else:
                passed = summary.passed
                failures = failure_summaries(summary) + failure_source_context(summary, self.tests_dir)
                self.record_tests(node_id, specs, summary)
            log(f"[acceptance] {node_id} round {attempt}: {passed}/{summary.total}")
            was_codegen = self.codegen_mode()
            normalized = failure_signature(summary) if summary.results else failures
            if normalized and normalized == previous_failures:
                # An identical failure is only evidence about the *fix* if the previous
                # round actually changed the app. When the last repair wrote nothing --
                # a reply with no <<<FILE>>> blocks, or every block refused -- the code
                # under test is byte-for-byte what already failed, so an identical
                # observation is guaranteed and says nothing at all.
                #
                # Observed directly on a local ticket-booking run:
                #   [acceptance] REQ-1 round 0: 0/1
                #   [flow] REQ-1 rewrite (repair 1) ok in 3s (wrote=False): '```json\n{ ... }'
                #   [acceptance] REQ-1 round 1: 0/1
                #   [flow] REQ-1: identical failure twice; switching repairs to tool mode
                # The reply was a ```json fence with no delimiters. Charging that to
                # "tried the same fix twice" then told the model to recheck the
                # assumptions behind its repair -- debugging reasoning that was never
                # the problem, while the actual defect (the wrapper) went unmentioned
                # and one of only two cheap codegen rounds was spent.
                #
                # `None` means "unknown" (tool-mode turns write through tools, not
                # through parsed blocks) and keeps the original behaviour.
                # getattr, not attribute access: this is a diagnostic refinement and it
                # must never be the thing that breaks the repair loop. A Flow built
                # without __init__ (tests do this) would otherwise raise here and take
                # the whole acceptance loop down -- the same rule last_repair_diff states.
                if getattr(self, "last_codegen_wrote", None) is False:
                    log(f"[flow] {node_id}: identical failure, but the last repair wrote no files -- "
                        f"the app never changed, so this repeat is not evidence about the fix; "
                        f"staying in codegen mode")
                else:
                    # Cloud 91aaecaf31af: three codegen rounds, identical observation.
                    self.codegen_blocked = True
                    self.pending_corrections.append(
                        'Repeated attempts produced the same observed failure. Recheck the assumptions behind the repair: inspect expected and received values, preceding actions, locator scope, and actual application state. Change the cause supported by this evidence. Do not manufacture the expected output or bypass the underlying operation; preserve behavior for other inputs.')
                    log(f"[flow] {node_id}: identical failure twice; switching repairs to tool mode")
            previous_failures = normalized
            if attempt >= int(os.environ.get("OCTOS_ARC_CODEGEN_REPAIRS", "2")) and passed < summary.total \
                    and self.codegen_mode():
                # Cloud 91aaecaf31af / 5747e6bcf530: repeated codegen repairs re-emit the same files.
                # One cheap codegen repair (failure digest + quoted sources) is allowed; then tools.
                self.codegen_blocked = True
                log(f"[flow] {node_id}: codegen attempt {attempt} still failing; repairs use tool mode")
            for line in failure_console_lines(failures):
                log(f"[acceptance]   {line}")
            if summary.total and passed == summary.total:
                self.commit(f"{node_id} (accepted): {passed}/{summary.total} acceptance tests pass")
                return True
            if passed > best_passed:
                if best_passed >= 0:
                    self.commit(f"{node_id} (repair {attempt}): {passed}/{summary.total} pass")
                best_passed, best_sha, regressions, stalls = passed, self.head(), 0, 0
            elif passed == best_passed and attempt > 0:
                stalls += 1
                if stalls >= 2 and not (was_codegen and self.codegen_blocked):
                    # Let a newly selected repair strategy run once, within existing budgets.
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
            if left < self.repair_needs() or self.time_up():
                # A repair turn that starts with only a couple of minutes left
                # times out too (keep-local-3); keep the best state instead.
                log(f"[flow] {node_id}: {left:.0f}s left, below the {self.repair_needs():.0f}s a repair needs "
                    f"(measured from {len(getattr(self, 'repair_durations', []))} repair turn(s)); "
                    f"keeping the best state")
                break
            self.snapshot_sources(node_id, attempt)
            slow = summary.slow(self.slow_test_ms())
            slow_text = ("These tests exceeded the configured slow-test threshold: " + "; ".join(slow) +
                         ". Inspect the failed operations and measured timings before optimizing.\n" + self.perf_text()) if slow else ""
            if passed == 0 and rebuild_prompt is not None and not rewrite_used \
                    and best_passed <= 0 and self.can_rewrite_from_scratch() \
                    and os.environ.get("OCTOS_ARC_REWRITE_ON_ZERO", "1") != "0":
                rewrite_used = True
                log(f"[flow] {node_id}: nothing passed; one full rewrite turn instead of a patch")
                prompt = rebuild_prompt(failures or "(no detail)")
                if self.codegen_mode():
                    r_ok, r_text = self.codegen_turn(
                        prompt, min(self.node_timeout, left), f"{node_id} rewrite (repair {attempt + 1})",
                        spec_chars=getattr(self, "current_spec_chars", 0))
                else:
                    r_ok, r_text = self.turn(
                        prompt, min(self.node_timeout, left), f"{node_id} rewrite (repair {attempt + 1})",
                        request_budget=int(os.environ.get("OCTOS_ARC_IMPLEMENT_REQUESTS", "20")))
                note = truncation_correction(r_ok, r_text)
                if note:
                    log(f"[flow] {node_id}: rewrite turn hit the output limit; telling the next round")
                    self.pending_corrections.append(note)
                lost = repair_wrote_nothing_correction(r_ok, r_text)
                if lost:
                    log(f"[flow] {node_id}: rewrite turn wrote no files; naming the wrapper for the next round")
                    self.pending_corrections.append(lost)
                continue
            prompt = REPAIR_PROMPT.format(node_id=node_id, passed=passed, total=summary.total,
                                          failures=failures or "(no detail)", test_location=self.repair_test_location(specs),
                                          corrections=self.corrections_text(),
                                          slow=slow_text, smoke=self.smoke_port, port=self.web_port,
                                          sources=self.repair_requirements(node_id) + self.sources_text())
            compact = self.codegen_repair_prompt(node_id, prompt) if self.codegen_mode() else None
            if compact is not None:
                p_ok, p_text = self.codegen_turn(
                    compact, min(self.node_timeout, left),
                    f"{node_id} repair {attempt + 1}/{self.repair_rounds}",
                    spec_chars=getattr(self, "current_spec_chars", 0))
            else:
                if self.codegen_mode():
                    self.codegen_blocked = True
                    log(f"[flow] {node_id}: one-request repair unavailable "
                        f"({getattr(self, 'codegen_repair_block', '') or 'reason not recorded'}); using tools")
                p_ok, p_text = self.turn(
                    prompt, min(self.node_timeout, left),
                    f"{node_id} repair {attempt + 1}/{self.repair_rounds}")
            note = truncation_correction(p_ok, p_text)
            if note:
                log(f"[flow] {node_id}: repair turn hit the output limit; telling the next round")
                self.pending_corrections.append(note)
            lost = repair_wrote_nothing_correction(p_ok, p_text)
            if lost:
                log(f"[flow] {node_id}: repair turn wrote no files; naming the wrapper for the next round")
                self.pending_corrections.append(lost)
        # Failed repairs can leave dirty files without changing HEAD. Restore the files,
        # even when the current commit already equals the best recorded commit.
        if best_passed > 0 and best_sha:
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

    def repair_needs(self) -> float:
        """再来一轮修复，实际需要多少秒——**按这个模型自己的表现算**，不是按一个猜的常数。

        原先这里是固定的 `min_repair_seconds`（默认 300）。意图是对的，代码注释写得很清楚：
        「只剩几分钟才开始的修复轮同样会超时，不如保住当前最好的状态」。
        但那个数字是猜的，而 keep @ D 的实测是**一轮修复约 600 秒**——整整两倍。
        猜小了的后果不是保守，是**反过来**：在只剩 300–600 秒时它会放行一轮注定被砍断的修复，
        那一轮什么都留不下（甚至可能留下改了一半的文件），时间也没了。

        所以改成用观察值：取最近若干轮的中位数、留 10% 余量，并且**不低于**配置的下限
        （下限仍然是 `OCTOS_MIN_REPAIR_SECONDS`，所以这条改动只会让判断更严、不会更松）。
        还没跑过修复轮时就用下限，行为与原先一致。

        只留最近 12 轮：模型和题目会变，很久以前的耗时不该继续影响现在的决定。
        """
        # 取**接下来真正会跑的那个模式**的观察值：节点一旦转入工具模式，
        # 下一轮就是工具模式，拿 codegen 的耗时去估它会严重低估（本仓库记录相差 19.6 倍）。
        buckets = getattr(self, "repair_durations", None)
        mode = "tool" if getattr(self, "codegen_blocked", False) else "codegen"
        if isinstance(buckets, dict):
            seen = [d for d in buckets.get(mode, []) if d and d > 0]
        else:                       # 兼容：万一还是旧的列表形态
            seen = [d for d in (buckets or []) if d and d > 0]
        if not seen:
            return float(self.min_repair_seconds)
        seen = sorted(seen)
        median = seen[len(seen) // 2] if len(seen) % 2 else (seen[len(seen) // 2 - 1] + seen[len(seen) // 2]) / 2
        return max(float(self.min_repair_seconds), median * 1.1)

    def banked_surplus(self, index: int, total: int) -> float:
        """已经**省下来**的预算里，可以拿给这个节点用的部分。

        `remaining / nodes_left` 假设剩下每个节点都花一样多。实际不是：大多数节点几秒就过，
        少数难节点要好几分钟。keep @ D 全程只用掉预算的 63%，而它丢掉的六个节点里有三个
        在**开局附近**——那时余额还没攒出来，份额恰好 1500，离它们需要的 1813 差 287 秒，
        于是修复停在第一轮。开局保守的代价，就是早期的难节点拿不到它们需要的时间。
        （抬高 `node_budget_cap` 救不了这三个：第 4 个节点的份额只有 1592，上限根本没顶到。）

        这里只动用**已经证明省下来的**那部分，不向未来借钱：

            按进度本该花掉的 = 总预算 × 已完成节点数 / 总节点数
            结余             = max(0, 本该花掉的 − 实际花掉的)

        进度落后时结余为 0，行为逐字不变——所以一个已经超支的运行不会因此继续超支。
        只拿结余的一半，另一半留给后面，免得开局几个难节点把余额吃光、把尾巴饿到地板。

        四个约束是叠加的不是替换的：`remaining / nodes_left` 仍是基数、
        `node_budget_cap` 仍封顶、240 秒地板仍在。
        """
        budget = getattr(self, "budget", 0) or 0
        if budget <= 0 or total <= 0:
            return 0.0
        done = max(0, index - 1)
        should_have_used = budget * done / total
        actually_used = budget - self.remaining()
        return max(0.0, should_have_used - actually_used) / 2.0

    def node_cycle(self, node: dict, ordered: list[dict], index: int, total: int) -> None:
        node_id = str(node.get("id"))
        specs = list(self.spec_map.get(node_id) or [])
        self.codegen_blocked = False  # a previous node's fallback to tool mode must not leak into this one
        # None = 本轮没有「引用了哪些源码」这回事（tiny 档位、工具模式、骨架轮，或应用还不存在），
        # 守卫不介入；只有真正按预算引用过源码的 codegen 轮才有拒写的依据。
        self.codegen_quoted = None
        if index > 1:
            reap_workspace_processes(self.output_dir, log)
        nodes_left = total - index + 1
        # 用 try 取结余：这是一个**预算上的改良**，绝不能成为压垮节点循环的那一环。
        # 既有测试用 Mock 搭 Flow，`banked_surplus` 会返回 Mock，`float + Mock` 直接 TypeError
        # 把整个 node_cycle 带崩——和 `last_codegen_wrote` 用 getattr 读取是同一条规矩。
        try:
            surplus = float(self.banked_surplus(index, total))
        except Exception:  # noqa: BLE001
            surplus = 0.0
        node_budget = min(self.node_budget_cap,
                          max(240, self.remaining() / nodes_left + surplus))
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
        corrections = self.corrections_text()
        prompt = corrections + prompt
        codegen_prompt = None
        implement_timeout = min(self.node_timeout, self.implement_fraction * node_budget, deadline - time.time())
        tiny_ok = False
        if not corrections and self.codegen_mode() and self.tiny_mode(len(self.spec_bodies(node_id))):
            tiny_ok = self.tiny_turn(node_id, specs, implement_timeout, node)
            self.current_spec_chars = len(self.spec_bodies(node_id))
        if tiny_ok:
            ok, text = True, "tiny tier: specs pass"
        elif self.codegen_mode() and self.codegen_context_fits(self.spec_bodies(node_id)):
            spec_text = self.spec_bodies(node_id)
            self.current_spec_chars = len(spec_text)
            # Small specs (by size, an input-derived measure) get the compact rule; the multi-page
            # mechanisms only apply when the spec is large enough to need sessions/navigation.
            small = self.codegen_reasoning(self.current_spec_chars) == "none"
            compact = CODEGEN_PROMPT.format(node_id=node_id, description=str(node.get("description") or "").strip(),
                                            spec=spec_text, port=self.web_port, ports=self.codegen_ports_clause(),
                                            size_rule=CODEGEN_SIZE_SMALL if small else CODEGEN_SIZE_FULL)
            if self.has_app():  # existing app (evolution or later nodes): quote the relevant sources
                budget = max(8000, self.codegen_quote_chars() - len(spec_text))
                self.codegen_quoted = quoted_source_paths(self.output_dir, spec_text, budget)
                compact = (compact.replace("Files:", "Existing app below; keep everything that works and output "
                                           "every changed file complete. Files:", 1)
                           + relevant_sources(self.output_dir, spec_text, budget))
            compact = corrections + compact
            codegen_prompt = compact
            write_codegen_manifests(self.output_dir)
            ok, text = self.codegen_turn(compact, implement_timeout, f"{node_id} implement", spec_chars=self.current_spec_chars)
        else:
            if self.codegen_mode():
                log(f"[flow] {node_id}: codegen context exceeded ({len(self.spec_bodies(node_id))} spec chars "
                    f"+ {app_source_chars(self.output_dir)} source chars vs {self.codegen_context_chars()} allowed); "
                    f"tool mode from here")
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
        can_verify_existing = self.has_app() and self.runner is not None and bool(specs)
        if not ok and not timed_out and not can_verify_existing:
            self.mark("implementation_failed", node_id, text[-500:])
            self.impl_failed.append(node_id)
            return
        if not ok and not timed_out:
            log(f"[flow] {node_id}: generation did not complete; testing the existing app")
            self.pending_corrections.append(
                no_files_correction(text) or
                "The implementation turn did not complete. Judge the existing files using acceptance results; "
                "preserve working behavior and repair only failures supported by those results.")
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
        self.mark("implementation_done", node_id, (text[-500:] or None) if ok else "implementation incomplete; existing code awaiting acceptance")
        self.commit(f"{node_id} (implement): {node.get('name', '')}")

        def rebuild_prompt(failures: str) -> str:
            if self.codegen_mode() and codegen_prompt:
                spec_bodies = self.spec_bodies(node_id)
                budget = max(8000, self.codegen_quote_chars() - len(codegen_prompt))
                self.codegen_quoted = quoted_source_paths(self.output_dir, spec_bodies, budget)
                return (codegen_prompt + "\nYour previous files (quoted below) failed every test. Failures:\n" + failures
                        + "\n" + relevant_sources(self.output_dir, spec_bodies, budget)
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

    def regression_checkpoint(self, index: int, total: int) -> None:
        start = int(os.environ.get("OCTOS_ARC_REGRESSION_CHECKPOINT", "4"))
        if (not regression_checkpoint_due(index, total, start) or self.runner is None
                or not self.tests_dir or self.remaining() < self.min_repair_seconds):
            return
        tracked = getattr(self, "checkpoint_regressions", set())
        self.checkpoint_regressions = tracked
        verified = {node: self.spec_map.get(node, []) for node, verdict in self.test_verdict.items()
                    if verdict is True or node in tracked}
        specs = sorted({spec for paths in verified.values() for spec in paths})
        if len(specs) < 2:
            return
        workers = workers_for_final(getattr(self, "mem_limit", None),
                                    int(os.environ.get("OCTOS_ARC_FINAL_WORKERS", "4")))
        summary = self.run_specs(specs, workers=workers, grader_like=True)
        if summary.error or summary.killed:
            log(f"[acceptance] checkpoint {index}: no reliable verdict; {summary.error or 'runner killed'}")
            return
        grouped = nodes_for_failures(summary.results, verified)
        log(f"[acceptance] checkpoint {index}: {summary.passed}/{summary.total}; "
            f"regressed nodes {sorted(node for node in grouped if node)}")
        for node in grouped:
            if node in verified:
                tracked.add(node)
                self.test_verdict[node] = False
                self.mark("test_failed", node, "previously passing behavior failed a regression checkpoint")
        for node in list(tracked):
            paths = verified.get(node, [])
            observed = [[r for r in summary.results if Path(r.file or "").name == Path(path).name]
                        for path in paths]
            if observed and all(rows and all(r.ok for r in rows) for rows in observed):
                tracked.remove(node)
                self.test_verdict[node] = True
                self.mark("test_passed", node, "previously regressed behavior passed its checkpoint specs")
        if grouped:
            # Build the evidence to fit the correction instead of cutting it to
            # length afterwards: a head-only slice of richer evidence drops the
            # last regressions entirely and always drops the source context.
            budget = int(os.environ.get("OCTOS_ARC_CHECKPOINT_EVIDENCE", "12000"))
            evidence = (failure_summaries(summary, max_snapshots=budget // 2)
                        + failure_source_context(summary, self.tests_dir))
            self.pending_corrections.append(
                "Previously passing behavior failed when checked together after recent changes. "
                "Repair the observed failures while preserving other working behavior. "
                "Tests ran together against one server; use this evidence when implementing the next node.\n"
                + clip_ends(evidence, budget))
            self.repair_regressions(index, specs, verified, tracked, summary, grouped, workers)

    def repair_regressions(self, index: int, specs: list[str], verified: dict, tracked: set,
                           summary: RunSummary, grouped: dict, workers: int) -> None:
        """Fix what a checkpoint found before building anything else on top.

        Queueing the evidence for the next node's turn does not work: that turn
        is busy with its own node, and its acceptance run only covers its own
        spec, so nothing re-checks the regression until the next checkpoint. In
        cloud e767e871a6c6 checkpoint 8 reported REQ-2.3.1, REQ-2.3.2 and
        REQ-2.3.3 broken; by checkpoint 16 the same three were still broken and
        four more had joined them, with no recovery recorded in between.
        """
        # Two rounds, not one. Cloud keep 4e18c76637ae (32 nodes, 2026-09-17) regressed
        # five nodes across checkpoints 8 and 16; one repair round per checkpoint cleared
        # two of them (REQ-2.5.3/2.5.4, confirmed at the next checkpoint) and left
        # REQ-2.2 and REQ-2.4 broken for 97 minutes and three checkpoints until the final
        # suite fixed them. A 32-node tree leaves the final suite enough budget to be that
        # backstop; a 125-node one spends the budget on nodes, so a regression found early
        # has to be cleared where it is found.
        rounds = int(os.environ.get("OCTOS_ARC_CHECKPOINT_REPAIRS", "2"))
        for attempt in range(rounds):
            if not grouped or self.remaining() < self.min_repair_seconds or self.wound_down():
                return
            failing = sorted(node for node in grouped if node) or ["the regressed behaviours"]
            failures = failure_summaries(summary) + failure_source_context(summary, self.tests_dir)
            # Capture the outcome instead of discarding it. This turn used to be called
            # for effect only: a truncated or unfinished checkpoint repair looked exactly
            # like one that simply failed to fix anything, and the next attempt was handed
            # no hint. That matters more here than anywhere else -- checkpoint recovery is
            # the entire measured gap between submission A's keep (32/32, 8 nodes recovered
            # at checkpoints) and D's (24 passed in-loop, 0 recovered). Same treatment the
            # node-level repair path already gets.
            c_ok, c_text = self.turn(REPAIR_PROMPT.format(
                node_id=", ".join(failing), passed=summary.passed, total=summary.total, failures=failures,
                test_location=self.repair_test_location(),
                sources=self.repair_requirements() + self.sources_text(),
                corrections=self.corrections_text(), slow="",
                smoke=self.smoke_port, port=self.web_port),
                min(self.suite_repair_timeout(), max(120, self.remaining() - 200)),
                f"checkpoint {index} repair {attempt + 1}/{rounds}")
            note = truncation_correction(c_ok, c_text)
            if note:
                log(f"[flow] checkpoint {index} repair {attempt + 1}: hit the output limit; "
                    f"telling the next attempt")
                self.pending_corrections.append(note)
            elif not c_ok:
                # Not treated as fatal -- the specs below still decide. But say it, so a
                # checkpoint that recovered nothing because its repair never ran is
                # distinguishable from one whose repair ran and was wrong.
                log(f"[flow] checkpoint {index} repair {attempt + 1}: turn did not complete "
                    f"({str(c_text)[-160:]})")
            self.commit(f"fix: checkpoint {index} regression repair {attempt + 1}")
            summary = self.run_specs(specs, workers=workers, grader_like=True)
            if summary.error:
                return
            grouped = nodes_for_failures(summary.results, verified)
            log(f"[acceptance] checkpoint {index} after repair: {summary.passed}/{summary.total}; "
                f"still regressed {sorted(node for node in grouped if node)}")
            for node in verified:
                if node and node not in grouped:
                    tracked.discard(node)
                    self.test_verdict[node] = True
                elif node:
                    tracked.add(node)
                    self.test_verdict[node] = False

    def final_acceptance(self) -> None:
        """Run EVERY spec file together against one server with the configured workers.
        Per-node runs cannot see cross-node interference through shared server
        state; this pass can, and it repairs the nodes whose tests fail."""
        if self.runner is None or not self.tests_dir:
            return
        all_specs = sorted(str(p.relative_to(self.tests_dir)) for p in self.tests_dir.rglob("*.spec.ts"))
        unverified = [n for n, v in self.test_verdict.items() if v is not True] or \
            [n for n in self.spec_map if n and self.spec_map[n] and n not in self.test_verdict]
        if len(all_specs) < 2 and not unverified:
            return  # single spec already judged by the node run
        # One more round than the identical-failure escalation needs, so the
        # changed approach actually gets to run.
        rounds = int(os.environ.get("OCTOS_FINAL_REPAIR_ROUNDS", "3"))
        workers = workers_for_final(getattr(self, "mem_limit", None), int(os.environ.get("OCTOS_ARC_FINAL_WORKERS", "4")))
        previous_failing: frozenset | None = None
        repeated = False  # the last round reproduced the round before it
        # Which behaviours the per-node runs judged good, before this pass starts
        # overwriting the verdicts with full-suite ones.
        passed_alone = {node for node, verdict in self.test_verdict.items() if verdict is True}
        passed_a_round: set = set()  # nodes this pass has already seen pass once
        # Measured, not yet acted on: a repair killed at the per-turn timeout can
        # leave the tree part edited. Cloud 6e82a7ff571c went 27/32, had its
        # repair cut at 1200s, and measured 17/32 next round, so the rounds after
        # it repair the damage rather than the five failures it started with. The
        # restore below still delivers the best round, so only the intervening
        # rounds are spent. Repairing from `best` instead was tried and reverted:
        # the evidence then comes from a round where the flaky specs passed, which
        # silently drops the intermittent note. Any retry needs to keep both.
        best: dict | None = None  # L17: best full-suite round (passed, sha, summary, grouped)
        regressions = 0  # consecutive rounds behind the best one
        last_passed = -1
        unfinished = ""  # what the previous repair turn said it had left to do
        wrote_last = False  # whether that turn got as far as committing an edit
        for attempt in range(rounds + 1):
            summary = self.run_specs(all_specs, workers=workers, grader_like=True)
            while summary.error and summary.killed and workers > 1:
                # Cloud 29c840566f36: the runner was OOM-killed under a 512 MiB
                # cgroup. The memory a suite needs is not known before running it,
                # so give the box a count it can hold instead of abandoning the
                # repairs on the first kill.
                workers = max(1, workers // 2)
                log(f"[acceptance] full suite runner was killed; retrying with {workers} worker(s)")
                summary = self.run_specs(all_specs, workers=workers, grader_like=True)
            if summary.error and summary.killed:
                log(f"[acceptance] full suite could not run ({summary.error[:120]}); keeping per-node verdicts")
                break
            if summary.error:
                # The app does not even start the way the grader starts it: every node fails.
                log(f"[acceptance] full suite (grader-like start) failed: {summary.error[:300]}")
                for node_id in self.spec_map:
                    if node_id:
                        self.test_verdict[node_id] = False
                grouped = {None: []}
                failures = (f"- Feature: application startup exactly as the grader runs it (only PORT set)\n"
                            f"  Failed at: npm start\n  Observation: {startup_error_digest(summary.error, 700)}\n"
                            f"  Steps: npm run build -> npm start")
                summary = RunSummary(passed=0, total=len(all_specs))
            else:
                grouped = nodes_for_failures(summary.results, self.spec_map)
                failures = failure_summaries(summary) + failure_source_context(summary, self.tests_dir)
                failures += self.interference_note(grouped, passed_alone, summary.stores_written)
                failures += self.intermittent_note(grouped, passed_a_round)
                failures += self.worker_parity_note(workers)
                owners = {Path(path).name: node for node, paths in self.spec_map.items()
                          for path in (paths or [])}
                passed_a_round |= {owners.get(Path(r.file or "").name)
                                   for r in summary.results if r.ok} - {None}
            log(f"[acceptance] full suite round {attempt}: {summary.passed}/{summary.total}; failing nodes "
                f"{sorted(k for k in grouped if k) or ('all' if None in grouped and not summary.results else [])}")
            self.record_full_suite(summary, grouped)
            last_passed = summary.passed
            if best is None or summary.passed > best["passed"]:
                if attempt > 0:
                    self.commit(f"chore: full acceptance suite {summary.passed}/{summary.total} (best so far)")
                best = {"passed": summary.passed, "sha": self.head(), "summary": summary, "grouped": grouped}
                regressions = 0
            elif summary.passed < best["passed"]:
                # The per-node loop already does this; the full-suite pass did not.
                # A repair cut at the per-turn timeout leaves the tree part
                # written: cloud 6e82a7ff571c went 27/32 -> repair killed at 1200s
                # -> 17/32, and the rounds after it repair that instead of the
                # five failures the pass began with. Two rounds behind the best
                # is a trend rather than one flaky spec, which is the same
                # threshold the node loop uses.
                #
                # Two and not one, and the same run shows why: 6e82a7ff571c went
                # 27/32, lost a repair to the timeout, measured 17/32, lost the
                # next repair to the timeout as well -- and then measured 28/32,
                # past the best it had. A rollback on the first dip would have
                # returned to 27 and never reached 28. Damage from a cut turn is
                # recoverable, so this waits for a trend and the pass still
                # delivers its best round either way.
                regressions += 1
                if regressions >= 2 and best["sha"]:
                    self.restore_app(best["sha"])
                    self.pending_corrections.append(
                        f"Your last two full-suite repairs made the tests worse; the harness restored frontend/ and "
                        f"backend/ to the best state ({best['passed']}/{best['summary'].total}). Start from that code.")
                    log(f"[acceptance] full suite: two rounds behind {best['passed']}/{best['summary'].total}; "
                        f"restored the best state")
                    # The tree is the best round's again, so the verdicts have to be
                    # too, and the restore at the end of the pass has nothing left
                    # to do.
                    self.record_full_suite(best["summary"], best["grouped"])
                    last_passed = best["passed"]
                    regressions = 0
            if not grouped:
                self.commit(f"chore: full acceptance suite {summary.passed}/{summary.total} pass (full suite)")
                return
            # A spec that has passed once in this pass and fails now is unstable;
            # letting it count as progress hides a stall in everything else.
            unstable = frozenset(spec for node in passed_a_round
                                 for spec in (self.spec_map.get(node) or []))
            failing_signature = failure_signature(summary, unstable)
            if previous_failing is not None and failing_signature == previous_failing:
                if repeated:
                    log("[acceptance] full suite: failures unchanged after a changed approach; stopping repairs")
                    break
                # Cloud 3ffe9702bf15: the suite stalled at 26/32 and the run ended
                # with most of its budget unspent. One identical round means the
                # repair missed the cause, not that the cause cannot be fixed --
                # tell it so, the way the per-node path already does, and retry.
                repeated = True
                failures += self.last_repair_diff()
                # A repair that ran out before editing has an unfinished plan worth
                # continuing; one that edited and moved nothing does not -- there the
                # instruction below is to change the cause, so carrying its reasoning
                # forward would argue against the correction in the same prompt.
                if wrote_last:
                    unfinished = ""
                    log("[acceptance] full suite: same failures as the previous round; changing repair approach")
                    self.pending_corrections.append(
                        'Repeated attempts produced the same observed failure. Recheck the assumptions behind the repair: inspect expected and received values, preceding actions, locator scope, and actual application state. Change the cause supported by this evidence. Do not manufacture the expected output or bypass the underlying operation; preserve behavior for other inputs.')
                else:
                    # The previous repair committed nothing, so the suite just measured the
                    # same code twice and identical failures are guaranteed -- they are not
                    # evidence that the repair missed the cause. `last_repair_diff()` above
                    # already added the accurate line for this case ("left frontend/ and
                    # backend/ unchanged ... Make an edit this time"), and `unfinished` is
                    # deliberately kept so the half-finished plan carries forward. Adding
                    # "recheck the assumptions behind the repair" on top of that argues
                    # against it in the same prompt: one line says finish what you started,
                    # the other says abandon your reasoning and change the cause.
                    # Same defect, and the same fix, as the per-node identical-failure path.
                    log("[acceptance] full suite: same failures, but the previous repair committed "
                        "nothing -- the suite measured identical code, so this repeat is not "
                        "evidence about the repair; keeping the unfinished plan")
            else:
                repeated = False
            previous_failing = failing_signature
            if attempt == rounds or self.remaining() < 240 or self.wound_down():
                break
            failing = sorted(k for k in grouped if k) or ["all nodes"]
            failures += self.unfinished_repair_note(unfinished)
            prompt = REPAIR_PROMPT.format(
                node_id=", ".join(failing), passed=summary.passed, total=summary.total, failures=failures,
                test_location=self.repair_test_location(),
                sources=self.repair_requirements() + self.sources_text(),
                corrections=self.corrections_text() + "The full suite runs all spec files against one "
                "server; tests from different files must not interfere through shared server state "
                "(e.g. a counter that every browser session shares). Keep persisted data only where the "
                "requirement demands persistence.\n",
                slow="", smoke=self.smoke_port, port=self.web_port)
            _, unfinished = self.turn(prompt, min(self.suite_repair_timeout(), max(120, self.remaining() - 200)),
                                      f"full-suite repair {attempt + 1}/{rounds}")
            wrote_last = self.commit(f"fix: full-suite repair {attempt + 1}")
        # L17 (ported from the Rust harness): deliver the best full-suite round, not the last one.
        if best is not None and best["sha"] and last_passed < best["passed"]:
            log(f"[acceptance] full suite: last round {last_passed} < best {best['passed']}; restoring the best state")
            self.restore_app(best["sha"])
            self.record_full_suite(best["summary"], best["grouped"])
            self.commit(f"chore: keep best full-suite state {best['passed']}/{best['summary'].total}")

    def suite_repair_timeout(self) -> int:
        """How long a repair that answers the whole suite at once may take.

        `node_timeout` bounds a turn about one node; this bounds a turn about
        every failing node together, and the two are not the same size of job.
        They default to the same number. Measured on the runs of 2026-09-16, an
        hour apart, because the first reading was misleading. At 207 node-phase
        turns not one had reached the 1200s cap, longest 1049s, while both of
        6e82a7ff571c's full-suite repairs were cut at it -- which looked like
        the suite repairs being the long ones. By 261 turns prestashop had a
        node-phase turn cut at 1200s on a 2 GiB container, and six turns sat
        past 890s. So the long turns are not particular to the full suite, nor
        to 6e82a7ff571c's 512 MiB box; six runs sharing one provider queue
        behind each other. Keep the two separate anyway, so the suite one can
        move without shortening or lengthening every node turn with it.
        """
        return int(os.environ.get("OCTOS_SUITE_REPAIR_TIMEOUT", str(self.node_timeout)))

    def final_acceptance_passes(self) -> None:
        """Repeat the full-suite pass while it still fails and the budget allows.

        Cloud 3ffe9702bf15 delivered 25/32 after spending 5754 s of a 48000 s
        budget: one pass ran, its repairs stalled, and the run ended with the
        rest of the time unused. Every pass keeps its own best state, so a
        repeat starts from a state at least as good as the one before it.
        """
        passes = int(os.environ.get("OCTOS_FINAL_SUITE_PASSES", "3"))
        for attempt in range(passes):
            if self.time_up() or self.remaining() < self.min_repair_seconds * 3:
                break
            if attempt:
                if all(verdict is not False for verdict in self.test_verdict.values()):
                    break
                log(f"[flow] full suite still failing with {self.remaining():.0f}s left; "
                    f"pass {attempt + 1}/{passes}")
            self.final_acceptance()
            if self.driver:
                self.driver.end_scope("node")

    GRADER_WORKERS = 4  # from the config that grades a run: workers: 4, fullyParallel: false

    @classmethod
    def worker_parity_note(cls, workers: int) -> str:
        """Say so when the suite could not be run the way it will be graded.

        A memory limit can force the count below the grader's (`workers_for_final`),
        and then this run is not the run that scores the app.

        An earlier version of this note claimed the count decides *which* tests
        fail, from two runs at each of one and four workers. More repeats
        disproved it: the same two nodes swap places between repeats at a fixed
        count as well, so that difference is the flakiness #175 reports, not the
        worker count. What is left is the parity gap itself, which is reason
        enough not to read the list as the set that will be scored.
        """
        if workers >= cls.GRADER_WORKERS:
            return ""
        return (f"\n\nThis suite ran with {workers} worker(s); grading runs {cls.GRADER_WORKERS} against one "
                "server, so this is not the run that scores the app. Fix the cause these failures share "
                "rather than the exact list, which a differently loaded run need not reproduce.")

    @staticmethod
    def intermittent_note(grouped: dict, passed_a_round: set) -> str:
        """Name the behaviours that already passed once in this pass.

        Running the same 32 specs three times over one unchanged app gave 25,
        24 and 25: REQ-2.7.1 failed once and passed twice. A repair told only
        that it failed goes looking for a missing feature, when what it has is a
        race — and the grader runs with retries off, so it stays worth fixing.
        """
        flaky = sorted(node for node in grouped if node and node in passed_a_round)
        if not flaky:
            return ""
        return ("\n\nThese already passed in an earlier round of this same pass and fail now: "
                f"{', '.join(flaky)}. The behaviour exists; what is missing is that it settles "
                "reliably. Look for a race, an unawaited update or state left over from another "
                "session rather than for an unimplemented feature.")

    @staticmethod
    def interference_note(grouped: dict, passed_alone: set, stores_written: list | None = None) -> str:
        """Name the behaviours that only fail in company.

        Cloud 746c81a2b5aa lost REQ-5.1 this way: it passed on its own and failed
        in the suite, where every spec drives the same server. Saying which
        failures are of that kind separates "the behaviour is wrong" from "the
        behaviour does not survive another session touching the same state" —
        the evidence for both looks identical otherwise.

        The claim is only as fresh as the per-node verdict it rests on, and later
        nodes change the app. Measured on one delivered app, seven of its eight
        suite failures also failed with the rest of the suite removed, so the note
        asks for the check rather than asserting the conclusion.
        """
        solo = sorted(node for node in grouped if node and node in passed_alone)
        if not solo:
            return ""
        note = ("\n\nThese passed their own node run earlier and fail now that every spec drives one "
                f"server: {', '.join(solo)}. That earlier result is from before the later nodes were "
                "built, so check the behaviour still works on its own: if it does, the difference is "
                "state shared with the other sessions; if it does not, it was broken since.")
        if stores_written:
            note += (" The suite run left these files changed on disk: "
                     f"{', '.join(stores_written)} — whatever one session writes there is still there for "
                     "the next.")
        return note

    def record_full_suite(self, summary: RunSummary, grouped: dict) -> None:
        """Per-node verdicts and traceability from one full-suite round."""
        for node_id, specs in self.spec_map.items():
            if node_id and specs and summary.results:
                self.record_tests(node_id, specs, RunSummary(results=[r for r in summary.results
                                  if Path(r.file or "").name in {Path(p).name for p in specs}]))
                self.test_verdict[node_id] = node_id not in grouped

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
            # Capture and log, but deliberately DO NOT append a correction here.
            # REHEARSAL_REPAIR_PROMPT takes error/port/smoke only -- it has no
            # `corrections` placeholder, so it never drains the corrections channel.
            # An append would therefore miss the next rehearsal attempt entirely and
            # instead surface in whatever unrelated prompt drains the channel next,
            # telling that turn about a truncation that had nothing to do with it.
            # The loop re-measures with a real build/start either way, so the log line
            # is the whole value: it separates "the repair turn never finished" from
            # "the repair ran and the app still will not start".
            reh_ok, reh_text = self.turn(
                REHEARSAL_REPAIR_PROMPT.format(error=clip_ends(err, 1200), port=self.web_port, smoke=self.smoke_port),
                self.node_timeout, f"rehearsal repair {attempt}")
            if not reh_ok:
                log(f"[rehearsal] repair {attempt} did not complete ({str(reh_text)[-160:]}); "
                    f"the next attempt re-measures with a real build/start")
            elif truncation_correction(False, reh_text):
                log(f"[rehearsal] repair {attempt} hit the output limit; "
                    f"its edit may be partial and the next build will say so")
            self.commit("fix: startup rehearsal repair")
        return False

    def postflight(self) -> None:
        """Hand the box back before grading starts.

        The grader launches four Chromium workers in this cgroup the moment the
        harness returns. Cloud 746c81a2b5aa left 512 MiB to share: the suite the
        harness had just measured at 30/32 was killed four tests in and all 32
        scored as skipped. Servers a model turn left running are memory we can
        still give back, and reaping only between nodes leaves the last ones
        alive for exactly the run that cannot afford them.
        """
        if self.driver:
            self.driver.close()
        self.cleanup_playwright()
        self.stop_llm_proxy()
        strays = reap_workspace_processes(self.output_dir, log)
        if strays:
            log(f"[flow] reaped {strays} leftover process(es) before grading")

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
            self.requirement_nodes = {str(node.get("id")): node for node in ordered}
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
                # Raised from 2.5M/node to 8M/node. The token limb of this guard exists to
                # bound *cost*, and that is no longer a thing it can buy: the platform meters
                # its own access key, a submission that brings its own key records 0, and
                # `_cost_efficiency` returns None for cost <= 0 -- so spend has no effect on
                # the leaderboard at all. Meanwhile tripping it is expensive in the only
                # currency left: `wound_down()` shuts off every repair turn for the rest of
                # the run, including the checkpoint repairs that recovered 8 of submission A's
                # 32 keep nodes.
                #
                # And the old margin was razor thin, measured not guessed: A's keep finished
                # on 78,211,655 tokens against an 80,000,000 limit -- 97.8%, i.e. it did not
                # trip by 2.2%. Any run slightly less frugal than the best one we have would
                # have lost all of its repairs to a guard protecting nothing.
                #
                # Safe to raise because wall-clock is guarded separately and does not depend
                # on this number: every repair site already gates on `self.remaining()` /
                # `time_up()` against OCTOS_TIME_BUDGET (main.py 2467, 2826, 2891, 3087, 3138).
                # 8M/node is 10x the calibrated 0.8M/node and 3.3x A's measured 2.44M/node.
                self.max_total_tokens = max(6_000_000, 8_000_000 * self.n_nodes)
            if self.max_turns < 0:
                # Turns are left alone: unlike tokens, turn count is a proxy for wall-clock
                # rather than for money, and that is still a real constraint.
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
                if not self.evolution and self.codegen_mode() and os.environ.get("OCTOS_SKELETON_ALWAYS") != "1":
                    log(f"[flow] {len(ordered)}-node tree: codegen mode, harness manifests replace the skeleton turn")
                elif not self.evolution and (len(ordered) >= self.skeleton_min_nodes
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
                    self.regression_checkpoint(index, len(ordered))
                    self.driver.end_scope("node")

                self.final_acceptance_passes()
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
                self.postflight()
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
            _reap_stray_processes("postflight", self.output_dir)
            _postflight_structure_check(self.output_dir)
            _free_web_port(self.web_port, self.output_dir)
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
            _reap_stray_processes("exception", self.output_dir)
            _postflight_structure_check(self.output_dir)
            _free_web_port(self.web_port, self.output_dir)
            self.events.mark_run_failed(str(exc)[:1000])
            return 1 if isinstance(exc, PermanentProviderError) else 0

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
    from llm_proxy import open_upstream  # loopback must bypass the system proxy (6f41b052)
    headers = {"Content-Type": "application/json", "Authorization": "Bearer " + key}
    deadline = time.time() + 600
    attempt = 0
    while True:
        attempt += 1
        try:
            with open_upstream(_ur.Request(base.rstrip("/") + "/models", headers=headers), 30, base) as resp:
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
                with open_upstream(req, 60, base) as resp:
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
