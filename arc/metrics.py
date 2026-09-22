#!/usr/bin/env python3
"""Summarise one local run: tokens, cost, turns, duration, node states, grade.

usage: metrics.py <output_dir> [--json]

Numbers come straight from `.arc/octos-events.jsonl` (turn/completed
tokens_in/tokens_out; token_cost_update session_cost, summed over sessions
because each session's cost is cumulative) and `.arc/runner-events.jsonl`
(running -> completed wall time, requirement_state rows).
"""

from __future__ import annotations

import json
import sys
import time
from pathlib import Path


def _iter_jsonl(path: Path):
    if not path.is_file():
        return
    with path.open(encoding="utf-8", errors="replace") as fh:
        for line in fh:
            try:
                yield json.loads(line)
            except json.JSONDecodeError:
                continue


TURN_CUTOFF_S = 1190  # OCTOS_NODE_TIMEOUT defaults to 1200; a cut turn loses its partial work


def cost_profile(records: list[dict]) -> dict:
    """Where a run's prompt tokens went, from the proxy's own request log.

    Split by mode, because the two behave nothing alike: codegen sends one
    large request per node and gets a file back, while a tool-mode escalation
    sends many requests that are mostly re-sent context. On one bookstack run
    two of 34 nodes escalated and took 201 of the 232 requests and 7.39M of the
    7.83M prompt tokens, which a single total hides completely.

    `avg_tools_chars` is the tool-schema payload repeated on every tool-mode
    request; `reasoning_pct` catches a model spending its output budget on
    thinking; `turns_near_cutoff` counts turns that came within ten seconds of
    the node timeout, where the partial work is lost rather than returned.
    """
    def bucket(rows: list[dict]) -> dict:
        return {
            "requests": len(rows),
            "prompt_tokens": sum(int(r.get("prompt_tokens") or 0) for r in rows),
            "completion_tokens": sum(int(r.get("completion_tokens") or 0) for r in rows),
            "avg_tools_chars": round(sum(int((r.get("request") or {}).get("tools_chars") or 0)
                                         for r in rows) / len(rows)) if rows else 0,
        }

    tool_mode = [r for r in records if int((r.get("request") or {}).get("tools") or 0) > 0]
    codegen = [r for r in records if int((r.get("request") or {}).get("tools") or 0) == 0]
    out = sum(int(r.get("completion_tokens") or 0) for r in records)
    reasoning = sum(int(r.get("reasoning_tokens") or 0) for r in records)
    elapsed = [int(r.get("elapsed_ms") or 0) / 1000 for r in records]
    return {
        "codegen": bucket(codegen),
        "tool_mode": bucket(tool_mode),
        "reasoning_pct": round(100 * reasoning / out) if out else 0,
        "turns_near_cutoff": sum(1 for e in elapsed if e >= TURN_CUTOFF_S),
        "max_turn_s": round(max(elapsed)) if elapsed else 0,
    }


def summarize(output_dir: Path) -> dict:
    arc = output_dir / ".arc"
    turns = tokens_in = tokens_out = 0
    tool_calls = 0
    cost_by_session: dict[str, float] = {}
    # token_cost_update carries cumulative per-session input/output tokens; a
    # turn that hits the wall-clock cap never emits turn/completed, so these
    # are the complete count (keep-local-3: only 2 of 19 turns completed).
    in_by_session: dict[str, int] = {}
    out_by_session: dict[str, int] = {}
    sessions: set[str] = set()
    for ev in _iter_jsonl(arc / "octos-events.jsonl"):
        method, params = ev.get("method"), ev.get("params") or {}
        sid = str(params.get("session_id") or "")
        if sid:
            sessions.add(sid)
        if method == "turn/completed":
            turns += 1
            tokens_in += int(params.get("tokens_in") or 0)
            tokens_out += int(params.get("tokens_out") or 0)
        elif method == "tool/started":
            tool_calls += 1
        elif method == "progress/updated":
            meta = params.get("metadata") or {}
            if meta.get("kind") == "token_cost_update":
                tc = meta.get("token_cost") or {}
                cost = tc.get("session_cost")
                if isinstance(cost, (int, float)):
                    cost_by_session[sid] = max(cost_by_session.get(sid, 0.0), float(cost))
                in_by_session[sid] = max(in_by_session.get(sid, 0), int(tc.get("input_tokens") or 0))
                out_by_session[sid] = max(out_by_session.get(sid, 0), int(tc.get("output_tokens") or 0))
    started = completed = None
    states: dict[str, str] = {}
    for ev in _iter_jsonl(arc / "runner-events.jsonl"):
        if ev.get("type") == "runner_state":
            ts = ev.get("timestamp")
            if ev.get("state") == "running":
                started, completed = ts, None  # last run in the file wins
            elif ev.get("state") in ("completed", "failed"):
                completed = ts
        elif ev.get("type") == "requirement_state":
            states[ev["node_id"]] = f"{ev.get('phase')}/{ev.get('status')}"
    duration = None
    if started and completed:
        fmt = "%Y-%m-%d %H:%M:%S"
        duration = int(time.mktime(time.strptime(completed, fmt)) - time.mktime(time.strptime(started, fmt)))
    node_states = {}
    try:
        node_states = {k: v.get("state") for k, v in json.loads((arc / "traceability" / "node_states.json").read_text()).items()}
    except (OSError, json.JSONDecodeError, AttributeError):
        pass
    billed = {"requests": 0, "prompt_tokens": 0, "completion_tokens": 0, "cache_hit": 0}
    usage = list(_iter_jsonl(arc / "llm-usage.jsonl"))
    for rec in usage:
        billed["requests"] += int(rec.get("requests") or 1)  # kernel-session turns carry their LLM-call count
        billed["prompt_tokens"] += int(rec.get("prompt_tokens") or 0)
        billed["completion_tokens"] += int(rec.get("completion_tokens") or 0)
        billed["cache_hit"] += int(rec.get("prompt_cache_hit_tokens") or 0)
    grade = None
    grade_file = arc / "local-grade.json"
    if grade_file.is_file():
        try:
            grade = json.loads(grade_file.read_text())
        except json.JSONDecodeError:
            pass
    return {
        "output_dir": str(output_dir), "turns": turns, "sessions": len(sessions), "tool_calls": tool_calls,
        "tokens_in": tokens_in, "tokens_out": tokens_out,
        "tokens_in_all": sum(in_by_session.values()), "tokens_out_all": sum(out_by_session.values()),
        "cost": round(sum(cost_by_session.values()), 6), "duration_s": duration,
        "node_states": node_states, "last_events": states, "grade": grade, "billed": billed,
        "cost_profile": cost_profile(usage),
    }


def main(argv: list[str]) -> int:
    if not argv or argv[0].startswith("-"):
        print(__doc__)
        return 2
    data = summarize(Path(argv[0]).resolve())
    if "--json" in argv:
        print(json.dumps(data, ensure_ascii=False, indent=2))
        return 0
    g = data["grade"] or {}
    grade = f"{g.get('passed')}/{g.get('total')}" if g else "n/a"
    b = data["billed"]
    print(f"| {Path(data['output_dir']).name} | {data['turns']} | {data['tokens_in_all']} | {data['tokens_out_all']} | "
          f"{data['cost']} | {data['duration_s']} | {grade} | {data['node_states']} | "
          f"billed req={b['requests']} prompt={b['prompt_tokens']} (cache {b['cache_hit']}) completion={b['completion_tokens']} |")
    p = data["cost_profile"]
    print(f"  codegen  req={p['codegen']['requests']} prompt={p['codegen']['prompt_tokens']} "
          f"completion={p['codegen']['completion_tokens']}")
    print(f"  toolmode req={p['tool_mode']['requests']} prompt={p['tool_mode']['prompt_tokens']} "
          f"completion={p['tool_mode']['completion_tokens']} avg_tool_schema_chars={p['tool_mode']['avg_tools_chars']}")
    print(f"  reasoning={p['reasoning_pct']}% of output | max turn {p['max_turn_s']}s | "
          f"turns near the {TURN_CUTOFF_S}s cut-off: {p['turns_near_cutoff']}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
