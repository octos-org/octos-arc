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

import argparse, functools, json, os, re, shlex, shutil, subprocess, sys, tempfile, time, tomllib
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
    # Wall clock one requirement may spend repairing after its first failed
    # check; past it the requirement is left as is and the build moves on.
    "repair_window": ("repair_window_seconds", "OCTOS_ARC_REPAIR_WINDOW", 1800),
    "node_timeout": ("node_timeout_seconds", "OCTOS_NODE_TIMEOUT", 1200),
    "verify_timeout": ("verify_timeout_seconds", "OCTOS_ARC_VERIFY_TIMEOUT", 1800),
    "max_iterations": ("max_iterations", "OCTOS_MAX_ITERATIONS", 40),
    "run_timeout": ("run_timeout_seconds", "OCTOS_TIME_BUDGET", 3600),
    "node_budget": ("node_budget_seconds", "OCTOS_NODE_TIME_BUDGET", 600),
    "min_node_seconds": ("min_node_seconds", "OCTOS_ARC_MIN_NODE_SECONDS", 120),
    "final_reserve_seconds": ("final_reserve_seconds", "OCTOS_ARC_FINAL_RESERVE", 600),
    "final_repairs": ("final_repair_rounds", "OCTOS_ARC_FINAL_REPAIRS", 2),
    # Every Nth acceptance node also re-runs the specs of earlier requirements
    # that passed, so a regression is repaired while its cause is fresh
    # rather than all at once at the end. 0 = off.
    "regression_every": ("regression_checkpoint", "OCTOS_ARC_REGRESSION_CHECKPOINT", 4),
    "tools": ("node_tools", "OCTOS_ARC_NODE_TOOLS", "read_file,write_file,edit_file,glob,grep,list_dir"),
    "reasoning": ("reasoning_effort", "OCTOS_ARC_REASONING", "none"),
    # 0 = trust the kernel's model catalog. Set it for an endpoint serving a
    # smaller window than the model's nominal one (a local server loaded with
    # 32k): the node's worker then trims to fit instead of overflowing into
    # empty responses.
    "context_window": ("context_window", "OCTOS_ARC_CONTEXT_WINDOW", 0),
    # Total time for ONE non-streaming LLM request (the platform proxy rejects
    # SSE, so streaming stays off). The kernel default, 300 s, cut off a
    # write_file call generating a large page -- and a timed-out request is
    # an internal error that ends the node's conversation.
    "llm_timeout": ("llm_timeout_seconds", "OCTOS_ARC_LLM_TIMEOUT", 900),
    # Output cap of one worker LLM call. Unset, a worker takes the model's
    # catalog maximum (131072 for glm-5.3-flash): one runaway reply can then
    # outlast any request timeout.
    "node_max_output_tokens": ("node_max_output_tokens", "OCTOS_ARC_NODE_MAX_TOKENS", 32768),
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



_IMPORT = re.compile(r"""from\s+['"](\.{1,2}/[^'"]+)['"]""")


def spec_helpers(tests_dir: Path | None, rels: list[str]) -> list[str]:
    """Local modules the specs import (`./support/e2e`, `./helpers`). The
    selectors and flows a spec exercises often live there, so the model needs
    them as much as the spec itself."""
    found: list[str] = []
    for rel in rels if tests_dir else []:
        text = (tests_dir / rel).read_text(encoding="utf-8", errors="replace")
        for mod in _IMPORT.findall(text):
            base = (tests_dir / rel).parent / mod
            for cand in (Path(f"{base}{ext}") for ext in ("", ".ts", ".js", "/index.ts")):
                if cand.is_file() and tests_dir.resolve() in cand.resolve().parents:
                    found.append(str(cand.resolve().relative_to(tests_dir.resolve())))
                    break
    return list(dict.fromkeys(found))


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


def build_pipeline(nodes, specs, tests_dir, out, pol, ports, deadline, spec_map=None) -> str:
    """seed -> (implement -> acceptance) per requirement in dependency order ->
    full-suite regression check with its own fix loop.

    Loop semantics (all enforced by the kernel's DAG scheduler):
    * a failing acceptance node fires its back-edge to the implement node (the
      repair round) until verify_node.py prints STOP -- it counts attempts and
      watches the run deadline, so repairs are bounded by policy and by time,
      not by the scheduler's 10-run loop fuse;
    * forward edges out of an acceptance node fire on pass OR fail, so one
      requirement the model cannot finish never prunes the rest of the build
      (an unconditional edge is fail-closed and would);
    * implement nodes are continue_on_error: a timed-out turn still hands
      whatever it wrote to acceptance instead of pruning everything below.

    DAG-scheduler constraints, all load-bearing: no Parallel/DynamicParallel, no
    converge, no suggested_next; forward edges carry no label and default
    weight; a back-edge must carry a condition holding a `retry` marker and
    target the start node or one with a forward predecessor; and
    `find_start_node` ignores back-edges, so the node named `start` is what
    keeps validate rule 1 satisfied.
    """
    read = lambda n: (BUNDLE_DIR / "prompts" / f"{n}.md").read_text(encoding="utf-8")  # noqa: E731
    ports_clause = read("port-contract").replace("{ports}", ", ".join(map(str, ports))
                                                            ).replace("{port}", str(ports[0])) if len(ports) > 1 else ""

    def verify(*args) -> str:
        # The validator verifies its own CWD = the pipeline run dir, the only
        # place write_file calls can land. ShellCheckHandler runs it via
        # `sh -c`, so quote every path or a directory with a space in its name
        # splits into "file not found" and the node fails forever.
        return dot_quote(" ".join(shlex.quote(str(a)) for a in
                                  [sys.executable, BUNDLE_DIR / "verify_node.py", *args]))

    window = f'context_window="{pol["context_window"]}", ' if pol["context_window"] else ""
    # The gateway section of config.json never reaches the profile runtime,
    # so worker reasoning and output caps ride on each node.
    window += (f'reasoning_effort="{pol["reasoning"]}", '
               f'max_output_tokens="{pol["node_max_output_tokens"]}", ')

    def impl_node(name, label, prompt) -> str:
        return (f'    {name} [handler="codergen", label="{dot_quote(label)}", {window}'
                f'tools="{pol["tools"]}", max_iterations="{pol["max_iterations"]}", '
                f'max_retries="0", continue_on_error="true", timeout_secs="{pol["node_timeout"]}", '
                f'prompt="{dot_quote(prompt)}"]')

    fail = 'outcome.status == \\"fail\\"'
    settled = f'outcome.status == \\"pass\\" || {fail}'
    # A codergen node that ends Fail (e.g. out of iterations) must still hand
    # what it wrote to acceptance; an unconditional edge would prune it.
    anyway = f'{settled} || outcome.status == \\"error\\"'
    # `retry` in the condition is what makes these legal back-edges.
    repair = (f'{fail} && !outcome.contains(\\"{STOP}\\") '
              f'&& context.retry_budget != \\"exhausted\\"')
    # run_pipeline kills the whole run at its timeout -- 1800 s unless the
    # graph says otherwise. The adapter owns the budget, so the graph carries
    # it (plus the final reserve); kernel_env raises the clamp ceiling to match.
    lines = [f'digraph {pol["name"]} {{',
             f'    graph [default_timeout_secs="{pol["run_timeout"] + pol["final_reserve_seconds"]}"]',
             '    start [handler="noop", label="Start"]',
             f'    seed [handler="shell_check", label="seed workspace", timeout_secs="120", '
             f'prompt="{verify("--seed", out)}"]',
             "    start -> seed"]
    every = pol["regression_every"]
    regress = lambda i: ["--regress", spec_map] if spec_map and every and i % every == 0 and i < len(nodes) else []  # noqa: E731
    prev, prev_cond = "seed", None
    tmpl, total = read("pipeline-implement"), len(nodes)
    for index, node in enumerate(nodes, 1):
        nid = str(node["id"])
        impl, check = f"impl_{sanitize(nid)}", f"check_{sanitize(nid)}"
        spec_text = ""
        rels = specs.get(nid, [])[:2]
        for rel in [*rels, *spec_helpers(tests_dir, rels)]:
            body = (tests_dir / rel).read_text(encoding="utf-8", errors="replace") if tests_dir else ""
            spec_text += f"\n----- {rel} -----\n{untemplate(body[:12000])}\n"
        body = (tmpl.replace("{node_id}", nid)
                    .replace("{description}", untemplate(describe(node)))
                    .replace("{spec}", spec_text or "(no public example for this requirement)")
                    .replace("{port}", str(ports[0]))
                    .replace("{ports}", ports_clause))
        lines.append(impl_node(impl, nid, body))
        # Keep enough time for one attempt at every requirement still to come
        # plus the regression pass; a node past that line stops repairing.
        reserve = (total - index) * pol["min_node_seconds"] + pol["final_reserve_seconds"]
        lines.append(
            f'    {check} [handler="shell_check", label="verify {dot_quote(nid)}", '
            f'timeout_secs="{pol["verify_timeout"]}", prompt="{verify(tests_dir or out, ports[0], "--tag", nid, "--attempts", pol["repairs"] + 1, "--deadline", int(deadline - reserve), "--repair-window", pol["repair_window"], *regress(index), *specs.get(nid, []))}"]')
        lines.append(f'    {prev} -> {impl}' + (f' [condition="{prev_cond}"]' if prev_cond else ""))
        lines.append(f'    {impl} -> {check} [condition="{anyway}"]')
        lines.append(f'    {check} -> {impl} [condition="{repair}"]')
        prev, prev_cond = check, settled
    # Regression pass: every public spec against the finished app. A later
    # requirement can break an earlier one; this is where that gets repaired.
    everything = sorted({r for rels in specs.values() for r in rels})
    if total > 1 and everything:
        lines += [
            f'    check_all [handler="shell_check", label="verify all", '
            f'timeout_secs="{pol["verify_timeout"]}", prompt="{verify(tests_dir or out, ports[0], "--tag", "ALL", "--best", 1, "--attempts", pol["final_repairs"] + 1, "--deadline", int(deadline - pol["final_reserve_seconds"] // 2), *everything)}"]',
            impl_node("fix_all", "regressions", read("pipeline-regression")
                      .replace("{port}", str(ports[0])).replace("{ports}", ports_clause)),
            '    done [handler="noop", label="Done"]',
            f'    {prev} -> check_all [condition="{prev_cond}"]',
            # An all-conditional router whose conditions all miss falls back to
            # its lowest-named target, so `done` (< `fix_all`) also catches the
            # STOP case; without the pass edge a passing suite would "repair".
            f'    check_all -> done [condition="outcome.status == \\"pass\\""]',
            f'    check_all -> fix_all [condition="{fail} && !outcome.contains(\\"{STOP}\\")"]',
            '    fix_all -> check_all [condition="context.retry_budget != \\"exhausted\\""]']
    lines.append("}")
    return "\n".join(lines) + "\n"


#: verify_node.py prints this when an acceptance node must not be retried again.
STOP = "ARC_NO_MORE_REPAIRS"


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
        # Reasoning, output caps and timeouts do NOT go here: the profile
        # runtime never reads this file's gateway section. They ride on the
        # graph's nodes and the env below.
    }
    if provider not in ("openai", "anthropic") and base_url:
        config["base_url"] = base_url
    config_dir.mkdir(parents=True, exist_ok=True)
    (config_dir / "config.json").write_text(json.dumps(config, indent=2), encoding="utf-8")
    env["OCTOS_CONFIG_DIR"] = str(config_dir)
    # K4: name the turn's tool surface -- run_pipeline hands the kernel the loop.
    env["OCTOS_STDIO_SOLO_TOOLS"] = "run_pipeline"
    # ...and only OUR pipeline: the session is woken by its own background
    # run and has been seen starting `deep_research` from that wake-up.
    env["OCTOS_PIPELINE_ALLOW"] = pol["name"]
    # ...and do not wake it for every finished node; the run reports once.
    env["OCTOS_PIPELINE_NODE_CONTINUATIONS"] = "0"
    env["OCTOS_PIPELINE_TIMEOUT_MAX_SECS"] = str(pol["run_timeout"] + pol["final_reserve_seconds"])
    # Fixed, not a default: the dispatch model copies "Max: 3600" from the
    # tool schema into timeout_secs, and a model-supplied value would win.
    env["OCTOS_PIPELINE_TIMEOUT_SECS"] = env["OCTOS_PIPELINE_TIMEOUT_MAX_SECS"]
    env["OCTOS_PIPELINE_DAG"] = "1"      # the DAG scheduler: retries + critique feedback
    env.setdefault("OCTOS_DISABLE_STREAMING", "1")   # platform proxies reject SSE
    # The profile runtime builds its provider without the gateway section, so
    # the request timeout travels by env as well.
    env["OCTOS_LLM_TIMEOUT_SECS"] = str(pol["llm_timeout"])
    # Ride out a minute or two of refused / reset connections (1+2+...+60s)
    # instead of failing the node after 7s; timeouts are never retried.
    env["OCTOS_LLM_MAX_RETRIES"] = "8"
    # ...and the dispatch session's reasoning level by the stdio override.
    env["OCTOS_STDIO_REASONING_EFFORT"] = pol["reasoning"]
    env.setdefault("OCTOS_DANGER_FULL_ACCESS", "1")
    env.setdefault("npm_config_registry", "https://registry.npmmirror.com")
    env["_ARC"] = json.dumps({"provider": provider, "model": model, "key_env": key_env,
                              "base_url": base_url})
    return env


#: The container ships no octos, and it must be OUR kernel: K4's policy-driven
#: tool surface and the `shell_check` DOT spelling are kernel changes the stock
#: release does not have. Without them `run_pipeline` never appears in the tool
#: surface, so the pipeline is never triggered. Published from the refactor
#: branch as a Linux x86_64 bundle; override with OCTOS_RELEASE_URL.
OCTOS_RELEASE_URL = (
    "https://github.com/octos-org/octos-arc/releases/download/v2.0.3-rc.11-arc.17/"
    "octos-bundle-x86_64-unknown-linux-gnu.tar.gz"
)


def _octos_url() -> str:
    return os.environ.get("OCTOS_RELEASE_URL", OCTOS_RELEASE_URL)


def _cached_octos(cache_dir: Path) -> str | None:
    """The cached binary, but only if it came from the URL in force now."""
    try:
        if (cache_dir / "octos").is_file() and (cache_dir / "source-url.txt").read_text() == _octos_url():
            return str(cache_dir / "octos")
    except OSError:
        pass
    return None


def _tarball_ok(tarball: Path) -> bool:
    import tarfile
    try:
        with tarfile.open(tarball) as tf:
            return tf.getmember("octos") is not None
    except Exception:  # noqa: BLE001
        return False


def _download_octos(cache_dir: Path) -> str:
    """gh-proxy mirrors first: the runner's own path to GitHub stalls HTTP/2.
    12 rotating attempts plus a member check, so a truncated file is never run."""
    import tarfile, urllib.request
    cache_dir.mkdir(parents=True, exist_ok=True)
    tarball, url = cache_dir / "octos-bundle.tar.gz", _octos_url()
    if _cached_octos(cache_dir) is None:
        tarball.unlink(missing_ok=True)     # an older URL's archive is stale
    mirrors = [f"{prefix}/{url}" for prefix in ("https://ghfast.top", "https://gh-proxy.com")] + [url]
    for attempt in range(1, 13):
        if _tarball_ok(tarball):
            break
        mirror = mirrors[(attempt - 1) % len(mirrors)]
        log(f"[octos] download attempt {attempt} ({mirror}) ...")
        if shutil.which("curl"):
            cmd = ["curl", "-fsSL", "--http1.1", "-C", "-", "--connect-timeout", "30",
                   "--speed-limit", "10240", "--speed-time", "60", "--retry", "2",
                   "-o", str(tarball), mirror]
            try:
                subprocess.run(cmd, check=False, timeout=600)
            except subprocess.TimeoutExpired:
                log(f"[octos] attempt {attempt} stalled 600s; rotating mirror")
        else:
            try:
                urllib.request.urlretrieve(mirror, tarball)
            except Exception as exc:  # noqa: BLE001
                log(f"[octos] download error: {exc}")
    if not _tarball_ok(tarball):
        raise RuntimeError(f"failed to download our octos release after 12 attempts: {url}")
    with tarfile.open(tarball) as tf:
        for member in ("octos", "octos-sandbox"):
            try:
                tf.extract(member, cache_dir, filter="data")
            except KeyError:
                pass
    for name in ("octos", "octos-sandbox"):
        if (cache_dir / name).is_file():
            (cache_dir / name).chmod(0o755)
    (cache_dir / "source-url.txt").write_text(url)
    return str(cache_dir / "octos")


def find_octos() -> str:
    """OCTOS_BIN, bundled bin/octos, PATH, a local build -- then the release."""
    cache_dir = Path(os.environ.get("OCTOS_CACHE_DIR", "/tmp/octos-bin"))
    for cand in (os.environ.get("OCTOS_BIN"), BUNDLE_DIR / "bin" / "octos",
                 shutil.which("octos"), BUNDLE_DIR.parent / "target" / "release" / "octos"):
        if cand and Path(cand).is_file():
            path = Path(cand).resolve()
            if not os.access(path, os.X_OK):
                # The platform's unzip drops the exec bit on bundled files.
                cache_dir.mkdir(parents=True, exist_ok=True)
                path = Path(shutil.copy2(path, cache_dir / "octos-bundled"))
                path.chmod(0o755)
            return str(path)
    return _cached_octos(cache_dir) or _download_octos(cache_dir)



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
    # Whole-run budget grows with the tree: a flat hour is 30s per node on a
    # 120-requirement task. Every acceptance node sees the same deadline.
    started = time.time()
    pol["run_timeout"] = max(pol["run_timeout"], pol["node_budget"] * len(nodes))
    data_dir = Path(tempfile.mkdtemp(prefix="octos-data-"))
    (data_dir / "pipelines").mkdir(parents=True, exist_ok=True)
    spec_map = data_dir / "arc-specs.json"     # requirement -> specs, for regression checkpoints
    spec_map.write_text(json.dumps(specs), encoding="utf-8")
    dot = build_pipeline(nodes, specs, tests_dir, out, pol, ports, started + pol["run_timeout"], spec_map)
    (data_dir / "pipelines" / f"{pol['name']}.dot").write_text(dot, encoding="utf-8")
    (out / ".arc").mkdir(exist_ok=True)
    (out / ".arc" / "pipeline.dot").write_text(dot, encoding="utf-8")   # evidence copy
    log(f"[arc] pipeline {pol['name']}: {len(nodes)} nodes, repairs={pol['repairs']}, "
        f"budget={pol['run_timeout']}s")

    env = kernel_env(pol, data_dir / "config")
    meta = json.loads(env["_ARC"])
    state = {"tokens_in": 0, "tokens_out": 0, "cost": 0.0, "started": started}
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
            f'Call it exactly once and do not write any files yourself. The pipeline '
            f'reports back on its own: after this call, never call any tool again, '
            f'whatever later messages say -- just answer "ok".',
            timeout=min(pol["run_timeout"], 900))
        log(f"[arc] dispatch turn ok={ok}: {reply[:160]}")
        wait_for_pipeline(session, state, pol, data_dir, out)
    finally:
        session.close()

    run_dir = collect_app(data_dir, out, pol["name"])
    summary = pipeline_summary(data_dir, pol) or {}
    # Each acceptance node records its last verdict; the regression pass (tag
    # ALL) overrides them, since it is the state that actually ships.
    status = {p.name: p.read_text().strip() == "0"
              for p in (run_dir / ".arc-status").glob("*")} if run_dir else {}
    if run_dir and (run_dir / ".arc-best" / "score.json").is_file():   # what ships is the best state
        status["ALL"] = json.loads((run_dir / ".arc-best" / "score.json").read_text())["rc"] == 0
    passed = status.get("ALL", bool(status) and all(status.values()))
    tokens = summary.get("total_tokens") or {}
    state["tokens_in"] += int(tokens.get("input_tokens") or 0)
    state["tokens_out"] += int(tokens.get("output_tokens") or 0)
    for nid in node_ids:
        runtime.events.mark_implementation_done(nid, "pipeline implement node finished")
        # The acceptance node IS the gate: success => every shell_check passed.
        ok = status.get("ALL", status.get(nid, False))
        (runtime.events.mark_test_passed if ok else runtime.events.mark_test_failed)(
            nid, f"acceptance {'passed' if ok else 'failed'}")
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


def collect_app(data_dir: Path, out: Path, name: str) -> Path | None:
    """Move the built app into ARCBENCH_OUTPUT_DIR. run_pipeline gives every run
    its own dir under `pipeline-runs/<run_id>/` and fences each node's file tools
    to it, so the app is NOT in the deliverable dir and the kernel exposes no
    knob to redirect it. This collects the same bytes acceptance just verified.
    """
    runs = [r for r in sorted(data_dir.glob(f"profiles/*/data/pipeline-runs/{name}-*"),
                              key=lambda p: p.stat().st_mtime if p.exists() else 0)
            if r.is_dir()]
    if not runs:
        log("[arc] no pipeline run dir found; nothing to collect")
        return None
    # The full-suite check keeps the best state it measured; the final state
    # can be worse (a repair that broke more than it fixed, or a run cut off
    # mid-edit).
    best = runs[-1] / ".arc-best"
    source = best / "app" if (best / "app" / "frontend").is_dir() else runs[-1]
    copied = [part for part in ("frontend", "backend") if (source / part).is_dir()]
    for part in copied:
        shutil.rmtree(out / part, ignore_errors=True)
        shutil.copytree(source / part, out / part,
                        ignore=shutil.ignore_patterns("node_modules", ".git"))
    log(f"[arc] collected {copied or 'nothing'} from {runs[-1].name}"
        + (f" (best full-suite state: {json.loads((best / 'score.json').read_text())['passed']} passed)"
           if source != runs[-1] else ""))
    return runs[-1]


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


def deliver_progress(data_dir: Path, out: Path, name: str, synced: dict) -> None:
    """Copy the latest state an acceptance check passed into the output dir,
    so a run the platform kills (or that crashes) still ships working code
    rather than the bare template. The final collect replaces it."""
    for good in data_dir.glob(f"profiles/*/data/pipeline-runs/{name}-*/.arc-good"):
        stamp = (good / "stamp").read_text() if (good / "stamp").is_file() else ""
        if stamp and stamp != synced.get("stamp"):
            for part in ("frontend", "backend"):
                if (good / "app" / part).is_dir():
                    shutil.rmtree(out / part, ignore_errors=True)
                    shutil.copytree(good / "app" / part, out / part)
            synced["stamp"] = stamp


def wait_for_pipeline(session, state: dict, pol: dict, data_dir: Path, out: Path) -> None:
    """`run_pipeline` is spawn_only: the dispatch turn returns as soon as the
    pipeline is queued, so the glue waits here for the background run."""
    import queue
    deadline = state["started"] + pol["run_timeout"] + pol["final_reserve_seconds"]
    idle_limit = pol["verify_timeout"] + 120
    last_progress, last_sync, synced = time.time(), 0.0, {}
    while time.time() < deadline:
        if time.time() - last_sync > 60:
            deliver_progress(data_dir, out, pol["name"], synced)
            last_sync = time.time()
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
