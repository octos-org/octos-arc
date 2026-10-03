#!/usr/bin/env python3
"""ARC-Bench adapter — glue only: read env/paths, emit this task's pipeline
for the kernel, name it on the first turn, collect events into the 7 tables.
Policy lives in arc-policy.toml and prompts/."""
from __future__ import annotations

import argparse, functools, hashlib, json, os, re, shlex, shutil, subprocess, sys, tempfile, time, tomllib
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
    "repairs": ("repair_rounds", "OCTOS_REPAIR_ROUNDS", 2),
    # Wall clock one requirement may spend repairing after its first failed check.
    "repair_window": ("repair_window_seconds", "OCTOS_ARC_REPAIR_WINDOW", 1800),
    "node_timeout": ("node_timeout_seconds", "OCTOS_NODE_TIMEOUT", 900),
    "verify_timeout": ("verify_timeout_seconds", "OCTOS_ARC_VERIFY_TIMEOUT", 1800),
    "max_iterations": ("max_iterations", "OCTOS_MAX_ITERATIONS", 40),
    "run_timeout": ("run_timeout_seconds", "OCTOS_TIME_BUDGET", 3600),
    "node_budget": ("node_budget_seconds", "OCTOS_NODE_TIME_BUDGET", 200),
    "min_node_seconds": ("min_node_seconds", "OCTOS_ARC_MIN_NODE_SECONDS", 120),
    "final_reserve_seconds": ("final_reserve_seconds", "OCTOS_ARC_FINAL_RESERVE", 600),
    "final_repairs": ("final_repair_rounds", "OCTOS_ARC_FINAL_REPAIRS", 2),
    "tools": ("node_tools", "OCTOS_ARC_NODE_TOOLS", "read_file,write_file,edit_file,glob,grep,list_dir"),
    "reasoning": ("reasoning_effort", "OCTOS_ARC_REASONING", "none"),
    # 0 = trust the kernel's model catalog; set it for an endpoint with a smaller window.
    "context_window": ("context_window", "OCTOS_ARC_CONTEXT_WINDOW", 0),
    # One non-streaming LLM request's ceiling (platform proxies reject SSE).
    "llm_timeout": ("llm_timeout_seconds", "OCTOS_ARC_LLM_TIMEOUT", 900),
    # Output cap of one worker LLM call; unset, a runaway reply outlasts any timeout.
    "node_max_output_tokens": ("node_max_output_tokens", "OCTOS_ARC_NODE_MAX_TOKENS", 32768),
    "group_requirements": ("group_requirements", "OCTOS_ARC_GROUP_REQUIREMENTS", 1), "group_max_chars": ("group_max_chars", "OCTOS_ARC_GROUP_MAX_CHARS", 6000),  # see arc-policy.toml
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


def group_nodes(nodes: list[dict], max_chars: int, max_members: int) -> list[list[dict]]:
    # Cluster independent siblings (deps met by an earlier CLOSED group) -- see arc-policy.toml.
    groups, current, chars, closed = [], [], 0, set()
    for node in nodes:
        deps, n = {str(d) for d in (node.get("dependencies") or [])}, len(describe(node))
        if current and deps <= closed and len(current) < max_members and chars + n <= max_chars:
            current, chars = current + [node], chars + n; continue
        if current:
            groups.append(current); closed.update(str(x["id"]) for x in current)
        current, chars = [node], n
    return groups + [current] if current else groups


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


def build_pipeline(nodes, out, pol, ports, deadline) -> str:
    """seed -> (implement -> acceptance) per requirement in dependency order ->
    a final smoke check with its own fix loop.

    Acceptance is requirement-text-driven only: verify_node.py builds the app,
    boots it and smokes GET /. The agent never sees the evaluation's test
    files -- no evaluation-test probing, no spec mapping, no helpers.

    Loop semantics (all enforced by the kernel's DAG scheduler):
    * a failing acceptance node fires its back-edge to the implement node (the
      repair round) until verify_node.py prints STOP (attempts/window/deadline);
    * forward edges fire on pass OR fail: one stuck requirement never prunes
      the rest of the build;
    * implement nodes are continue_on_error: a timed-out turn still hands what
      it wrote to acceptance.

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
        # The validator's CWD is the pipeline run dir, the only place file
        # writes can land. ShellCheckHandler runs it via `sh -c`: quote every
        # path, or a space in a dir name splits into "file not found" forever.
        return dot_quote(" ".join(shlex.quote(str(a)) for a in
                                  [sys.executable, BUNDLE_DIR / "verify_node.py", *args]))

    window = f'context_window="{pol["context_window"]}", ' if pol["context_window"] else ""
    # The gateway section of config.json never reaches the profile runtime,
    # so worker reasoning and output caps ride on each node.
    window += (f'reasoning_effort="{pol["reasoning"]}", '
               f'max_output_tokens="{pol["node_max_output_tokens"]}", ')

    def impl_node(name, label, prompt, timeout_secs=None) -> str:
        return (f'    {name} [handler="codergen", label="{dot_quote(label)}", {window}'
                f'tools="{pol["tools"]}", max_iterations="{pol["max_iterations"]}", '
                f'max_retries="0", continue_on_error="true", timeout_secs="{timeout_secs or pol["node_timeout"]}", '
                f'prompt="{dot_quote(prompt)}"]')

    fail = 'outcome.status == \\"fail\\"'
    settled = f'outcome.status == \\"pass\\" || {fail}'
    # A codergen node that ends Fail (e.g. out of iterations) must still hand
    # what it wrote to acceptance; an unconditional edge would prune it.
    anyway = f'{settled} || outcome.status == \\"error\\"'
    # `retry` in the condition is what makes these legal back-edges.
    repair = (f'{fail} && !outcome.contains(\\"{STOP}\\") '
              f'&& context.retry_budget != \\"exhausted\\"')
    # The adapter owns the budget: the graph carries run_timeout + reserve.
    lines = [f'digraph {pol["name"]} {{',
             f'    graph [default_timeout_secs="{pol["run_timeout"] + pol["final_reserve_seconds"]}"]',
             '    start [handler="noop", label="Start"]',
             f'    seed [handler="shell_check", label="seed workspace", timeout_secs="120", '
             f'prompt="{verify("--seed", out)}"]',
             "    start -> seed"]
    prev, prev_cond = "seed", None
    tmpl = read("pipeline-implement")
    groups = group_nodes(nodes, pol["group_max_chars"] if pol["group_requirements"] else 0, 3)
    total = len(groups); impl_timeout = min(pol["node_timeout"], max(pol["min_node_seconds"], (pol["run_timeout"] - pol["final_reserve_seconds"]) // total)) if total else pol["node_timeout"]  # fair share per impl node

    def node_body(nid, node):
        return (tmpl.replace("{node_id}", nid)
                    .replace("{description}", untemplate(describe(node)))
                    .replace("{spec}", "(no public example for this requirement)")
                    .replace("{port}", str(ports[0]))
                    .replace("{ports}", ports_clause))

    for index, members in enumerate(groups, 1):
        ids = [str(n["id"]) for n in members]; tag = "+".join(ids)
        impl, check = f"impl_{sanitize(tag)}", f"check_{sanitize(tag)}"
        body = node_body(ids[0], members[0]) if len(ids) == 1 else "Implement ALL together, then stop:\n\n" + "\n\n".join(node_body(i, n) for i, n in zip(ids, members))
        lines.append(impl_node(impl, tag, body, impl_timeout)); reserve = (total - index) * impl_timeout + pol["final_reserve_seconds"]
        lines.append(
            f'    {check} [handler="shell_check", label="verify {dot_quote(tag)}", '
            f'timeout_secs="{pol["verify_timeout"]}", prompt="{verify(ports[0], "--tag", tag, "--attempts", pol["repairs"] + 1, "--deadline", int(deadline - reserve), "--repair-window", pol["repair_window"], *(["--e2e", f"checks/{ids[0]}.mjs"] if len(ids) == 1 else ["--e2e-list", ",".join(f"checks/{i}.mjs" for i in ids)]))}"]')
        lines.append(f'    {prev} -> {impl}' + (f' [condition="{prev_cond}"]' if prev_cond else ""))
        lines.append(f'    {impl} -> {check} [condition="{anyway}"]')
        lines.append(f'    {check} -> {impl} [condition="{repair}"]')
        prev, prev_cond = check, settled
    # Budget layering: the chain above buys one implementation per requirement
    # (or small sibling group) first (repairs=2 caps quick retries); everything
    # left over goes to the final loop below, final_repairs times. (Per-req
    # sweep nodes would need 2 graph nodes each; the kernel caps graphs at 40.)
    # Final pass: the finished app must still build, boot and serve GET /.
    # A later requirement can break an earlier one; this is where that shows.
    if total > 1:
        lines += [
            f'    check_all [handler="shell_check", label="verify all", '
            f'timeout_secs="{pol["verify_timeout"]}", prompt="{verify(ports[0], "--tag", "ALL", "--attempts", pol["final_repairs"] + 1, "--deadline", int(deadline - pol["final_reserve_seconds"] // 2), "--e2e-dir", "checks")}"]',
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
    # The profile runtime never reads config.json's gateway section;
    # reasoning/caps/timeouts ride on the graph nodes and the env below.
    config = {
        "provider": provider, "model": model,
        "sandbox": {"allow_network": True},
        "memory": {"refresh": {"enabled": False}},
    }
    if provider not in ("openai", "anthropic") and base_url:
        config["base_url"] = base_url
    config_dir.mkdir(parents=True, exist_ok=True)
    (config_dir / "config.json").write_text(json.dumps(config, indent=2), encoding="utf-8")
    env["OCTOS_CONFIG_DIR"] = str(config_dir)
    env["OCTOS_STDIO_SOLO_TOOLS"] = "run_pipeline"
    env["OCTOS_PIPELINE_ALLOW"] = pol["name"]
    env["OCTOS_PIPELINE_NODE_CONTINUATIONS"] = "0"
    env["OCTOS_PIPELINE_TIMEOUT_MAX_SECS"] = str(pol["run_timeout"] + pol["final_reserve_seconds"])
    # Fixed, not a default: a model-supplied timeout_secs would win otherwise.
    env["OCTOS_PIPELINE_TIMEOUT_SECS"] = env["OCTOS_PIPELINE_TIMEOUT_MAX_SECS"]
    env["OCTOS_PIPELINE_DAG"] = "1"      # the DAG scheduler: retries + critique feedback
    env.setdefault("OCTOS_DISABLE_STREAMING", "1")   # platform proxies reject SSE
    env["OCTOS_LLM_TIMEOUT_SECS"] = str(pol["llm_timeout"])
    # Ride out refused / reset connections (1+2+...+60s); timeouts never retry.
    env["OCTOS_LLM_MAX_RETRIES"] = "8"
    env["OCTOS_STDIO_REASONING_EFFORT"] = pol["reasoning"]
    env.setdefault("OCTOS_DANGER_FULL_ACCESS", "1")
    env.setdefault("npm_config_registry", "https://registry.npmjs.org")
    env["_ARC"] = json.dumps({"provider": provider, "model": model, "key_env": key_env,
                              "base_url": base_url})
    return env


#: The container ships no octos, and it must be OUR kernel: K4's policy-driven
#: tool surface and the `shell_check` DOT spelling are kernel changes the stock
#: release lacks; without them `run_pipeline` never appears. Published as a
#: Linux x86_64 bundle; override with OCTOS_RELEASE_URL.
OCTOS_RELEASE_URL = (
    "https://github.com/octos-org/octos-arc/releases/download/v2.0.3-rc.11-arc.17/"
    "octos-bundle-x86_64-unknown-linux-gnu.tar.gz"
)


def _octos_url() -> str:
    return os.environ.get("OCTOS_RELEASE_URL", OCTOS_RELEASE_URL)


def _runtime_lock() -> dict | None:
    """The pinned runtime release (url + sha256) we are allowed to execute."""
    override = os.environ.get("OCTOS_LOCK")
    if override:
        path = Path(override)
        if not path.is_file():
            # An explicit lock path pins this environment; never fall back silently.
            raise RuntimeError(f"OCTOS_LOCK={override} not found; refusing to fall back to a different lock")
        return json.loads(path.read_text(encoding="utf-8"))
    for path in (BUNDLE_DIR / "arc-runtime-lock.json",
                 BUNDLE_DIR.parent / "arc-runtime-lock.json"):
        try:
            if path.is_file():
                return json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            pass
    return None


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _verify_against_lock(tarball: Path, url: str) -> None:
    """Verify downloads before executing them; no downgrade path (ARC_BASELINE)."""
    lock = _runtime_lock()
    if lock is None:
        raise RuntimeError("arc-runtime-lock.json not found; refusing to run an unverified engine download")
    release = lock.get("runtime_release") or {}
    pinned_url = release.get("url")
    if pinned_url and url != pinned_url:
        raise RuntimeError(f"octos release url {url} != pinned {pinned_url}; refusing to run it")
    expected = release.get("archive_sha256")
    if not expected:
        raise RuntimeError("runtime lock has no archive_sha256; refusing to run an unverified engine download")
    got = _sha256(tarball)
    if got != expected:
        raise RuntimeError(f"downloaded octos bundle sha256 {got} != pinned {expected}; refusing to run it")


def _cached_octos(cache_dir: Path) -> str | None:
    """The cached binary, only if hash-pinned and from the current URL."""
    try:
        if (cache_dir / "octos").is_file() and (cache_dir / "source-url.txt").read_text() == _octos_url():
            expected = (_runtime_lock() or {}).get("runtime_release", {}).get("binary_sha256")
            if expected and _sha256(cache_dir / "octos") != expected:
                log("[octos] cached binary hash != arc-runtime-lock.json; re-downloading")
                return None
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
    """Only the pinned official release URL + sha256 verify; 12 resumable attempts."""
    import tarfile, urllib.request
    cache_dir.mkdir(parents=True, exist_ok=True)
    tarball, url = cache_dir / "octos-bundle.tar.gz", _octos_url()
    if _cached_octos(cache_dir) is None:
        tarball.unlink(missing_ok=True)     # an older URL's archive is stale
    for attempt in range(1, 13):
        if _tarball_ok(tarball):
            break
        log(f"[octos] download attempt {attempt} ({url}) ...")
        if shutil.which("curl"):
            cmd = ["curl", "-fsSL", "--http1.1", "-C", "-", "--connect-timeout", "30",
                   "--speed-limit", "10240", "--speed-time", "60", "--retry", "2",
                   "-o", str(tarball), url]
            try:
                subprocess.run(cmd, check=False, timeout=600)
            except subprocess.TimeoutExpired:
                log(f"[octos] attempt {attempt} stalled 600s; retrying")
        else:
            try:
                urllib.request.urlretrieve(url, tarball)
            except Exception as exc:  # noqa: BLE001
                log(f"[octos] download error: {exc}")
    if not _tarball_ok(tarball):
        raise RuntimeError(f"failed to download our octos release after 12 attempts: {url}")
    _verify_against_lock(tarball, url)
    with tarfile.open(tarball) as tf:
        for member in ("octos", "octos-sandbox"):
            try:
                tf.extract(member, cache_dir, filter="data")
            except KeyError:
                pass
    for name in ("octos", "octos-sandbox"):
        if (cache_dir / name).is_file():
            (cache_dir / name).chmod(0o755)
    binary = cache_dir / "octos"
    expected_bin = (_runtime_lock() or {}).get("runtime_release", {}).get("binary_sha256")
    if expected_bin and _sha256(binary) != expected_bin:
        binary.unlink(missing_ok=True)
        (cache_dir / "octos-sandbox").unlink(missing_ok=True)
        raise RuntimeError("extracted octos binary hash != arc-runtime-lock.json; deleted, refusing to run it")
    (cache_dir / "source-url.txt").write_text(url)
    return str(cache_dir / "octos")


def find_octos() -> str:
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
    ports = [args.web_port]

    # SHORT temp path, never under the deliverable (control socket paths are
    # length-limited). Budget grows with the tree; all checks share one deadline.
    started = time.time()
    pol["run_timeout"] = max(pol["run_timeout"], pol["node_budget"] * len(nodes))
    data_dir = Path(tempfile.mkdtemp(prefix="octos-data-"))
    (data_dir / "pipelines").mkdir(parents=True, exist_ok=True)
    dot = build_pipeline(nodes, out, pol, ports, started + pol["run_timeout"])
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
        ask = (f'Call the run_pipeline tool now with pipeline="{pol["name"]}" and '
               f'input="Build the application described by requirements {", ".join(node_ids)}". '
               f'Call it exactly once and do not write any files yourself. The pipeline '
               f'reports back on its own: after this call, never call any tool again, '
               f'whatever later messages say -- just answer "ok".')
        # The turn's success says nothing about the tool call: on a 125-id task
        # glm-5.3-flash once answered a bare "ok" and the run idled. A started
        # run leaves its dir; without one, ask again.
        started_run = lambda: any(data_dir.glob(f"profiles/*/data/pipeline-runs/{pol['name']}-*"))  # noqa: E731
        for attempt in range(3):
            ok, reply = session.run_turn(ask if attempt == 0 else
                                         f'You did not call run_pipeline; nothing is running. {ask}',
                                         timeout=min(pol["run_timeout"], 900))
            log(f"[arc] dispatch turn {attempt + 1} ok={ok}: {reply[:160]}")
            for _ in range(30):
                if started_run():
                    break
                time.sleep(1)
            if started_run():
                break
        wait_for_pipeline(session, state, pol, data_dir, out)
    finally:
        session.close()

    run_dir = collect_app(data_dir, out, pol["name"])
    # Delivery gate: never ship an app that dies at boot (grader-style soak);
    # on failure roll back to the last accepted state and gate that too.
    gate = [sys.executable, str(BUNDLE_DIR / "verify_node.py"), str(ports[0]), "--soak", "30"]
    good = run_dir / ".arc-good" / "app" if run_dir else None
    booted = False
    for _ in range(2):
        booted = subprocess.run(gate, cwd=out, capture_output=True, text=True, timeout=480).returncode == 0
        if booted or not (good and (good / "frontend").is_dir()):
            break
        log("[arc] deliverable failed the boot gate; rolling back to the last accepted state")
        for part in ("frontend", "backend"):
            if (good / part).is_dir():
                shutil.rmtree(out / part, ignore_errors=True)
                shutil.copytree(good / part, out / part)
    (out / ".arc-server.log").unlink(missing_ok=True)
    log(f"[arc] boot gate: {'ok' if booted else 'FAILED (shipped best available)'}")
    summary = pipeline_summary(data_dir, pol) or {}
    # Each acceptance node records its last verdict; the final pass (tag
    # ALL) overrides them, since it is the state that actually ships.
    status = {p.name: p.read_text().strip() == "0"
              for p in (run_dir / ".arc-status").glob("*")} if run_dir else {}
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
    """Move the built app into ARCBENCH_OUTPUT_DIR from the pipeline run dir."""
    runs = [r for r in sorted(data_dir.glob(f"profiles/*/data/pipeline-runs/{name}-*"),
                              key=lambda p: p.stat().st_mtime if p.exists() else 0)
            if r.is_dir()]
    if not runs:
        log("[arc] no pipeline run dir found; nothing to collect")
        return None
    source = runs[-1]
    copied = [part for part in ("frontend", "backend") if (source / part).is_dir()]
    for part in copied:
        shutil.rmtree(out / part, ignore_errors=True)
        shutil.copytree(source / part, out / part,
                        ignore=shutil.ignore_patterns("node_modules", ".git"))
    log(f"[arc] collected {copied or 'nothing'} from {runs[-1].name}")
    return runs[-1]


NODE_RE = re.compile(r"Pipeline '[^']*' running: (\S+)")


def record(method: str, params: dict, state: dict) -> None:
    """Token/cost accounting rides the kernel's own events."""
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
    """`.octos/runs/<run_id>/summary.json`: the run's completion signal."""
    for path in data_dir.glob(f"profiles/*/data/.octos/runs/{pol['name']}-*/summary.json"):
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        if data.get("graph_id") == pol["name"]:
            return data
    return None


def deliver_progress(data_dir: Path, out: Path, name: str, synced: dict) -> None:
    """Copy the latest accepted state into the output dir (killed-run delivery)."""
    for good in data_dir.glob(f"profiles/*/data/pipeline-runs/{name}-*/.arc-good"):
        stamp = (good / "stamp").read_text() if (good / "stamp").is_file() else ""
        if stamp and stamp != synced.get("stamp"):
            for part in ("frontend", "backend"):
                if (good / "app" / part).is_dir():
                    shutil.rmtree(out / part, ignore_errors=True)
                    shutil.copytree(good / "app" / part, out / part)
            synced["stamp"] = stamp


def wait_for_pipeline(session, state: dict, pol: dict, data_dir: Path, out: Path) -> None:
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
        session.on_event(frame.get("method", ""), frame.get("params") or {})
        if frame.get("method") == "tool/progress":
            last_progress = time.time()
    log("[arc] run budget exhausted; keeping whatever the pipeline produced")


if __name__ == "__main__":
    sys.exit(main())
