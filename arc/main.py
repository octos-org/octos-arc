#!/usr/bin/env python3
"""ARC-Bench adapter — glue only. Four jobs, nothing else:
  1. read the platform's env vars and paths;
  2. emit this task's pipeline into the dir the kernel scans, start the kernel;
  3. name that pipeline on the first turn so the KERNEL runs the
     implement -> acceptance -> repair loop (octos-pipeline's DAG scheduler);
  4. collect the kernel's events into the 7 tables and runner-events.jsonl.

No orchestration here: no rounds, no budget arithmetic, no model routing, no
acceptance runner. That policy lives in arc-policy.toml and prompts/.
"""
from __future__ import annotations

import argparse, functools, json, os, re, shlex, shutil, sys, tempfile, time, tomllib
from pathlib import Path

import yaml

BUNDLE_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(BUNDLE_DIR))

from arcbench_agent_runtime import AgentRuntime  # noqa: E402
from octos_stdio import OctosStdioSession  # noqa: E402


log = functools.partial(print, flush=True)



#: policy key -> (arc-policy.toml key, env override, default)
_POLICY = {
    "name": ("name", "OCTOS_ARC_PIPELINE_NAME", "arc_build"),
    "repairs": ("repair_rounds", "OCTOS_REPAIR_ROUNDS", 5),
    "node_timeout": ("node_timeout_seconds", "OCTOS_NODE_TIMEOUT", 1200),
    "verify_timeout": ("verify_timeout_seconds", "OCTOS_ARC_VERIFY_TIMEOUT", 900),
    "max_iterations": ("max_iterations", "OCTOS_MAX_ITERATIONS", 40),
    "run_timeout": ("run_timeout_seconds", "OCTOS_TIME_BUDGET", 3600),
    "tools": ("node_tools", "OCTOS_ARC_NODE_TOOLS", "read_file,write_file,edit_file,glob,grep,list_dir"),
    "reasoning": ("reasoning_effort", "OCTOS_ARC_REASONING", "none"),
    "max_output_tokens": ("max_output_tokens", "OCTOS_ARC_MAX_TOKENS", 65536),
}


def policy() -> dict:
    """arc-policy.toml is the single source of tunables; env vars still win."""
    path = BUNDLE_DIR / "arc-policy.toml"
    data = tomllib.loads(path.read_text(encoding="utf-8")) if path.is_file() else {}
    pipe = data.get("pipeline", {})
    out = {}
    for key, (toml_key, env_key, default) in _POLICY.items():
        value = os.environ.get(env_key, pipe.get(toml_key, default))
        out[key] = type(default)(value)
    return out



def load_tree(req_dir: Path) -> dict:
    req = req_dir / "requirements.yaml"
    if not req.is_file():
        req = req_dir / "requirements.yml"
    data = yaml.safe_load(req.read_text(encoding="utf-8"))
    if isinstance(data, dict) and "id" not in data:
        for wrapper in ("root", "requirement"):
            if isinstance(data.get(wrapper), dict):
                data = data[wrapper]
                break
    if not isinstance(data, dict) or "id" not in data:
        raise SystemExit(f"invalid requirements.yaml in {req_dir}")
    return data


def atomic_nodes(tree: dict) -> list[dict]:
    """Atomic requirements in dependency order (FOLDERs are grouping only)."""
    flat: dict[str, dict] = {}

    def walk(node: dict) -> None:
        if str(node.get("type", "")).upper() != "FOLDER":
            flat[str(node["id"])] = node
        for child in node.get("children") or []:
            walk(child)

    walk(tree)
    ordered: list[dict] = []
    seen: set[str] = set()

    def visit(nid: str, stack: set[str]) -> None:
        if nid in seen or nid in stack or nid not in flat:
            return
        stack.add(nid)
        for dep in flat[nid].get("dependencies") or []:
            visit(str(dep), stack)
        seen.add(nid)
        ordered.append(flat[nid])

    for nid in flat:
        visit(nid, set())
    return ordered


def describe(node: dict) -> str:
    lines = [f"Name: {node.get('name', '')}"]
    if node.get("description"):
        lines.append(str(node["description"]).strip())
    for sc in node.get("scenarios") or []:
        lines.append(f"Scenario: {sc.get('name', '')}")
        for step in sc.get("steps") or []:
            if isinstance(step, dict):
                lines.append(f"  {step.get('keyword', '')} {str(step.get('content', '')).strip()}")
    return "\n".join(lines)


def locate_tests(tree: dict) -> Path | None:
    """ARCBENCH_TESTS_DIR, then /workspace/tests. The bundle never ships the
    public specs -- they are task data, not submission content."""
    for cand in filter(None, [os.environ.get("ARCBENCH_TESTS_DIR"), "/workspace/tests"]):
        p = Path(cand)
        if p.is_dir() and any(p.rglob("*.spec.ts")):
            return p.resolve()
    local = os.environ.get("OCTOS_ARC_LOCAL_TESTS")
    if local and Path(local).is_dir():
        return Path(local).resolve()
    return None


_SPEC_ID = re.compile(r"^([A-Za-z]+-[\d.]+)")


def map_specs(tests_dir: Path | None, node_ids: list[str]) -> dict[str, list[str]]:
    """`REQ-1.2-login.spec.ts` -> node `REQ-1.2`; equal counts pair in order."""
    mapping: dict[str, list[str]] = {nid: [] for nid in node_ids}
    if tests_dir is None:
        return mapping
    by_id: dict[str, list[str]] = {}
    for path in sorted(tests_dir.rglob("*.spec.ts")):
        m = _SPEC_ID.match(path.name)
        by_id.setdefault(m.group(1) if m else path.name, []).append(
            str(path.relative_to(tests_dir)))
    key = lambda s: tuple(int(p) for p in re.findall(r"\d+", s))  # noqa: E731
    unmatched = []
    for sid in sorted(by_id, key=key):
        if sid in mapping:
            mapping[sid].extend(by_id[sid])
        else:
            unmatched.append(sid)
    free = [nid for nid in node_ids if not mapping[nid]]
    if unmatched and len(unmatched) == len(free):
        for sid, nid in zip(unmatched, free):
            mapping[nid].extend(by_id[sid])
    return mapping



def dot_quote(text: str) -> str:
    return text.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n")


def sanitize(node_id: str) -> str:
    return "n_" + re.sub(r"[^A-Za-z0-9_]", "_", node_id)


def untemplate(text: str) -> str:
    """Neutralise `{...}` in quoted task content: the validator reads any
    `{token}` of [A-Za-z0-9_-.:] as a template variable and REJECTS the graph
    when it is unbound, so a Playwright excerpt with `async ({ page }) =>` kills
    the run. Doubling puts a `{` inside the candidate, which the same check then
    refuses as a variable name, and reads as the usual escape to a model."""
    return text.replace("{", "{{").replace("}", "}}")


def build_pipeline(nodes, specs, tests_dir, out, pol, ports) -> str:
    """One implement + one acceptance node per requirement, chained by
    dependency, with a failure back-edge to the implement node (= repair round).

    DAG-scheduler constraints, all load-bearing: no Parallel/DynamicParallel, no
    converge, no suggested_next; forward edges carry no label and default
    weight; a back-edge must carry a condition holding a `retry` marker and
    target the start node or one with a forward predecessor; and
    `find_start_node` ignores back-edges, so the node named `start` is what
    keeps validate rule 1 satisfied.
    """
    read = lambda n: (BUNDLE_DIR / "prompts" / f"{n}.md").read_text(encoding="utf-8")  # noqa: E731
    tmpl = read("pipeline-implement")
    ports_clause = read("port-contract").replace("{ports}", ", ".join(map(str, ports))
                                                            ).replace("{port}", str(ports[0])) if len(ports) > 1 else ""
    lines = [f'digraph {pol["name"]} {{',
             '    start [handler="noop", label="Start"]']
    prev = "start"
    for node in nodes:
        nid = str(node["id"])
        impl, check = f"impl_{sanitize(nid)}", f"check_{sanitize(nid)}"
        spec_text = ""
        for rel in specs.get(nid, [])[:2]:
            body = (tests_dir / rel).read_text(encoding="utf-8", errors="replace") if tests_dir else ""
            spec_text += f"\n----- {rel} -----\n{untemplate(body[:12000])}\n"
        body = (tmpl.replace("{node_id}", nid)
                    .replace("{description}", untemplate(describe(node)))
                    .replace("{spec}", spec_text or "(no public example for this requirement)")
                    .replace("{port}", str(ports[0]))
                    .replace("{ports}", ports_clause))
        lines.append(
            f'    {impl} [handler="codergen", label="{dot_quote(nid)}", '
            f'tools="{pol["tools"]}", max_iterations="{pol["max_iterations"]}", '
            f'max_retries="{pol["repairs"]}", timeout_secs="{pol["node_timeout"]}", '
            f'prompt="{dot_quote(body)}"]')
        # The validator verifies its own CWD = the pipeline run dir, the only
        # place this node's write_file calls can land. ShellCheckHandler runs it
        # via `sh -c`, so quote every path or a directory with a space in its
        # name splits into "file not found" and the node fails forever.
        cmd = " ".join(shlex.quote(str(part)) for part in
                       [sys.executable, BUNDLE_DIR / "verify_node.py",
                        tests_dir or out, ports[0], *specs.get(nid, [])])
        lines.append(
            f'    {check} [handler="shell_check", label="verify {dot_quote(nid)}", '
            f'timeout_secs="{pol["verify_timeout"]}", prompt="{dot_quote(cmd)}"]')
        lines.append(f"    {prev} -> {impl}")
        lines.append(f"    {impl} -> {check}")
        # The repair round. `retry` is the marker that makes this a legal
        # back-edge; the DAG scheduler hands the failing check's output to the
        # implement node and re-runs the region below it.
        lines.append(f'    {check} -> {impl} [condition="outcome.status == \\"fail\\" '
                     f'&& context.retry_budget != \\"exhausted\\""]')
        prev = check
    lines.append("}")
    return "\n".join(lines) + "\n"



def kernel_env(pol: dict, config_dir: Path) -> dict:
    env = os.environ.copy()
    api_key = env.get("OPENAI_API_KEY", "")
    base_url = env.get("OPENAI_BASE_URL", "")
    model = env.get("OCTOS_MODEL") or env.get("MODEL", "")
    provider = env.get("OCTOS_PROVIDER") or ("deepseek" if "deepseek" in base_url else "openai")
    key_env = "OPENAI_API_KEY"
    if provider not in ("openai", "anthropic") and api_key:
        key_env = f"{provider.upper()}_API_KEY"
        env.setdefault(key_env, api_key)
    config = {
        "provider": provider, "model": model,
        "sandbox": {"allow_network": True},
        "memory": {"refresh": {"enabled": False}},
        # K1/K2 through the kernel's own controls (no local proxy): a disabled
        # effort emits `reasoning_effort: "none"`; the output ceiling is the
        # gateway budget. max_iterations bounds the DISPATCH turn -- run_pipeline
        # is spawn_only and acks "started in background", so an unbounded loop
        # re-dispatches it (observed: 3 concurrent runs of the same graph).
        "gateway": {"max_output_tokens": pol["max_output_tokens"],
                    "reasoning_effort": pol["reasoning"],
                    "max_iterations": 2},
    }
    if provider not in ("openai", "anthropic") and base_url:
        config["base_url"] = base_url
    config_dir.mkdir(parents=True, exist_ok=True)
    (config_dir / "config.json").write_text(json.dumps(config, indent=2), encoding="utf-8")
    env["OCTOS_CONFIG_DIR"] = str(config_dir)
    # K4: name the turn's tool surface -- run_pipeline hands the kernel the loop.
    env["OCTOS_STDIO_SOLO_TOOLS"] = "run_pipeline"
    env["OCTOS_PIPELINE_DAG"] = "1"      # the DAG scheduler: retries + critique feedback
    env.setdefault("OCTOS_DISABLE_STREAMING", "1")
    env.setdefault("OCTOS_DANGER_FULL_ACCESS", "1")
    env.setdefault("npm_config_registry", "https://registry.npmmirror.com")
    env["_ARC"] = json.dumps({"provider": provider, "model": model, "key_env": key_env,
                              "base_url": base_url})
    return env


def find_octos() -> str:
    for cand in [os.environ.get("OCTOS_BIN"), shutil.which("octos"),
                 BUNDLE_DIR.parent / "target" / "release" / "octos"]:
        if cand and Path(cand).is_file():
            return str(Path(cand).resolve())
    raise SystemExit("no octos binary: set OCTOS_BIN")



def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("requirement_path", nargs="?")
    ap.add_argument("--output-dir"); ap.add_argument("--type", default="web")
    ap.add_argument("--web-port", type=int, default=43100)
    args = ap.parse_args()
    pol = policy()

    req_dir = Path(args.requirement_path or os.environ.get("ARCBENCH_TASK_DIR") or ".").resolve()
    out = Path(args.output_dir or os.environ.get("ARCBENCH_OUTPUT_DIR") or "./arc-output").resolve()
    out.mkdir(parents=True, exist_ok=True)
    template = os.environ.get("ARCBENCH_TEMPLATE_DIR")
    if template and Path(template).is_dir() and not (out / "frontend").is_dir():
        shutil.copytree(template, out, dirs_exist_ok=True)

    tree = load_tree(req_dir)
    nodes = atomic_nodes(tree)
    node_ids = [str(n["id"]) for n in nodes]
    log(f"[arc] {len(nodes)} atomic nodes: {node_ids}")

    runtime = AgentRuntime.from_env(project_dir=str(out))
    runtime.events.mark_run_started("octos-arc pipeline adapter")
    runtime.git.ensure_repo()
    runtime.traceability.init_store()                      # all 7 tables exist
    runtime.traceability.store_requirement_tree(tree)      # requirements + scenarios
    tests_dir = locate_tests(tree)
    specs = map_specs(tests_dir, node_ids)
    log(f"[arc] tests at {tests_dir}; mapping { {k: v for k, v in specs.items() if v} }")
    for nid, rels in specs.items():
        for rel in rels:                                   # tests table
            runtime.traceability.upsert_test(test_id=rel, req_id=nid, type="e2e",
                                             file_path=rel, passed=None)

    # Some specs hardcode a port the grader does not start the app on; the app
    # must answer on both. The grading port always comes first.
    spec_text = "".join((tests_dir / r).read_text(errors="replace")
                        for rels in specs.values() for r in rels
                        if tests_dir and (tests_dir / r).is_file())
    ports = [args.web_port] + sorted(
        {int(p) for p in re.findall(r"(?:localhost|127\.0\.0\.1):(\d{4,5})", spec_text)}
        - {args.web_port})

    # SHORT temp path, never under the deliverable: `serve` binds
    # <data_dir>/.octos-goal-control.sock and a path over SUN_LEN (~104B) makes
    # the kernel die before the handshake. Also keeps scratch out of the bundle.
    data_dir = Path(tempfile.mkdtemp(prefix="octos-data-"))
    (data_dir / "pipelines").mkdir(parents=True, exist_ok=True)
    dot = build_pipeline(nodes, specs, tests_dir, out, pol, ports)
    (data_dir / "pipelines" / f"{pol['name']}.dot").write_text(dot, encoding="utf-8")
    (out / ".arc").mkdir(exist_ok=True)
    (out / ".arc" / "pipeline.dot").write_text(dot, encoding="utf-8")   # evidence copy
    log(f"[arc] pipeline {pol['name']}: {len(nodes)} nodes, max_retries={pol['repairs']}")

    env = kernel_env(pol, data_dir / "config")
    meta = json.loads(env["_ARC"])
    state = {"tokens_in": 0, "tokens_out": 0, "cost": 0.0, "started": time.time()}
    session = OctosStdioSession(find_octos(), out, env, data_dir,
                                on_event=lambda m, p: record(m, p, state))
    try:
        try:
            session.bootstrap_profile(meta["provider"], meta["model"], meta["base_url"],
                                      meta["key_env"])
        except Exception:
            # A kernel that never answers the handshake is the one failure the
            # bare traceback cannot explain; its stderr always can.
            log(f"[arc] kernel stderr:\n{session.stderr_tail(30)}")
            raise
        session.open()
        ok, reply = session.run_turn(
            f'Call the run_pipeline tool now with pipeline="{pol["name"]}" and '
            f'input="Build the application described by requirements {", ".join(node_ids)}". '
            f'Call it exactly once and do not write any files yourself.',
            timeout=min(pol["run_timeout"], 900))
        log(f"[arc] dispatch turn ok={ok}: {reply[:160]}")
        wait_for_pipeline(session, state, pol, data_dir)
    finally:
        session.close()

    collect_app(data_dir, out)
    summary = pipeline_summary(data_dir, pol) or {}
    passed = bool(summary.get("success"))
    tokens = summary.get("total_tokens") or {}
    state["tokens_in"] += int(tokens.get("input_tokens") or 0)
    state["tokens_out"] += int(tokens.get("output_tokens") or 0)
    for nid in node_ids:
        runtime.events.mark_implementation_done(nid, "pipeline implement node finished")
        # The acceptance node IS the gate: success => every shell_check passed.
        (runtime.events.mark_test_passed if passed else runtime.events.mark_test_failed)(
            nid, f"pipeline success={passed}")
    runtime.git.add_all(); runtime.git.commit("arc: pipeline run")
    seconds = round(time.time() - state["started"])
    runtime.events.mark_run_completed(f"success={passed} tokens_in={state['tokens_in']} "
                                      f"tokens_out={state['tokens_out']} cost={state['cost']}")
    (out / ".arc" / "run-summary.json").write_text(json.dumps({
        "success": passed, "seconds": seconds, "cost": state["cost"],
        "tokens_in": state["tokens_in"], "tokens_out": state["tokens_out"],
        "nodes_executed": summary.get("nodes_executed")}, indent=1), encoding="utf-8")
    log(f"[arc] done in {seconds}s; success={passed}; "
        f"tokens {state['tokens_in']}/{state['tokens_out']}; cost {state['cost']}")
    return 0


def collect_app(data_dir: Path, out: Path) -> None:
    """Move the built app into ARCBENCH_OUTPUT_DIR. run_pipeline gives every run
    its own dir under `pipeline-runs/<run_id>/` and fences each node's file tools
    to it, so the app is NOT in the deliverable dir and the kernel exposes no
    knob to redirect it. This collects the same bytes acceptance just verified.
    """
    runs = [r for r in sorted(data_dir.glob("profiles/*/data/pipeline-runs/*"),
                              key=lambda p: p.stat().st_mtime if p.exists() else 0)
            if r.is_dir() and r.name != "latest"]
    if not runs:
        return log("[arc] no pipeline run dir found; nothing to collect")
    copied = [part for part in ("frontend", "backend") if (runs[-1] / part).is_dir()]
    for part in copied:
        shutil.copytree(runs[-1] / part, out / part, dirs_exist_ok=True,
                        ignore=shutil.ignore_patterns("node_modules", ".git"))
    log(f"[arc] collected {copied or 'nothing'} from {runs[-1].name}")


NODE_RE = re.compile(r"Pipeline '[^']*' running: (\S+)")


def record(method: str, params: dict, state: dict) -> None:
    """K3: token/cost accounting rides the kernel's own events."""
    if method == "progress/updated":
        cost = (params.get("metadata") or {}).get("token_cost") or {}
        state["cost"] = max(state["cost"], float(cost.get("session_cost") or 0.0))
    elif method == "turn/completed":
        state["tokens_in"] += int(params.get("tokens_in") or 0)
        state["tokens_out"] += int(params.get("tokens_out") or 0)
    elif method == "tool/progress":
        hit = NODE_RE.search(str(params.get("message") or ""))
        if hit and state.get("last") != hit.group(1):
            state["last"] = hit.group(1)
            log(f"[pipeline] node {hit.group(1)}")


def pipeline_summary(data_dir: Path, pol: dict) -> dict | None:
    """`.octos/runs/<run_id>/summary.json`, written when a run ends, is the
    authoritative completion signal: run_pipeline is spawn_only, so the dispatch
    turn only acks "started in background" and no tool result follows."""
    for path in data_dir.glob(f"profiles/*/data/.octos/runs/{pol['name']}-*/summary.json"):
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        if data.get("graph_id") == pol["name"]:
            return data
    return None


def wait_for_pipeline(session, state: dict, pol: dict, data_dir: Path) -> None:
    """`run_pipeline` is spawn_only: the dispatch turn returns as soon as the
    pipeline is queued, so the glue waits here for the background run."""
    import queue
    deadline = state["started"] + pol["run_timeout"]
    idle_limit = pol["verify_timeout"] + 120
    last_progress = time.time()
    while time.time() < deadline:
        summary = pipeline_summary(data_dir, pol)
        if summary is not None:
            log(f"[arc] pipeline finished: success={summary.get('success')} "
                f"nodes_executed={summary.get('nodes_executed')} "
                f"in {round(summary.get('duration_ms', 0) / 1000)}s")
            return
        try:
            frame = session._notifications.get(timeout=5.0)
        except queue.Empty:
            if session.proc.poll() is not None:
                log("[arc] kernel exited")
                return
            if time.time() - last_progress > idle_limit:
                log(f"[arc] no pipeline progress for {idle_limit}s; stopping the wait")
                return
            continue
        method = frame.get("method", "")
        session.on_event(method, frame.get("params") or {})
        if method == "tool/progress":
            last_progress = time.time()
    log("[arc] run budget exhausted; keeping whatever the pipeline produced")


if __name__ == "__main__":
    sys.exit(main())
