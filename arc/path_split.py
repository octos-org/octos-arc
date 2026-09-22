#!/usr/bin/env python3
"""Split a run's nodes by which implementation path they took, and what each path cost.

This is the instrument that found the large-task bottleneck. On cloud stackoverflow
97848d542ac8 (66 nodes, 66/66, 7.5 h) it showed:

    codegen  23 nodes (35%)  median interval  20 s   total  45.8 min
    tool     43 nodes (65%)  median interval 392 s   total 402.9 min

i.e. 65% of the nodes took 90% of the wall clock, 19.6x the median each. The cause was
`codegen_context_fits` gating on the *output* budget, so an app outgrowing 90000
characters pushed every later node into tool mode. After the fix (30bd4f70), bookstack
48739278f1e0 ran 34/34 nodes with tool mode at 0%.

How the path is read: the `design` event's message. A codegen node reports "design folded
into the implementation turn (no JSON file)"; a tool-mode node reports "design JSON
written inline to ...". Parent (non-atomic) nodes report "N atomic children designed" and
are skipped.

Two measurement traps, both of which produced a wrong reading before this tool existed:

1. **Do not use the design→test interval as "time inside the node".** The `design` event is
   emitted *after* the model call that folded the design in, so that interval excludes the
   model call entirely. It reads as a 3 second median and suggests 86% of the time is spent
   outside nodes, which is nonsense. The only usable signal is the interval between
   consecutive nodes' first test verdicts.
2. **Do not read a partial run as final.** At node 44 of 66 stackoverflow read as 55% tool
   mode / 8.8x; the finished run was 65% / 19.6x. Quote the finished numbers.

usage:
  python3 arc/path_split.py <run_id> [<run_id> ...] [--cookie-jar PATH]
"""
from __future__ import annotations

import argparse
import http.cookiejar
import json
import os
import sys
import time
import urllib.error
import urllib.request
from datetime import datetime

BASE = "https://arc-bench.com/api"


def client(jar_path: str | None):
    jar = http.cookiejar.MozillaCookieJar(jar_path) if jar_path else http.cookiejar.CookieJar()
    if jar_path:
        jar.load(ignore_discard=True, ignore_expires=True)
    return urllib.request.build_opener(urllib.request.HTTPCookieProcessor(jar))


def get(opener, path: str, tries: int = 6):
    """Chunked read with retries; 5xx is retried (the logs endpoint answers 500
    intermittently on the larger runs), 4xx is raised at once."""
    last: Exception | None = None
    for _ in range(tries):
        try:
            with opener.open(BASE + path, timeout=300) as resp:
                chunks = []
                while True:
                    chunk = resp.read(65536)
                    if not chunk:
                        break
                    chunks.append(chunk)
                return json.loads(b"".join(chunks))
        except urllib.error.HTTPError as exc:
            if exc.code < 500:
                raise
            last = exc
            time.sleep(6)
        except Exception as exc:  # noqa: BLE001 - transport flakiness
            last = exc
            time.sleep(6)
    raise last if last else RuntimeError("unreachable")


def stamp(text):
    try:
        return datetime.strptime(text, "%Y-%m-%d %H:%M:%S")
    except Exception:  # noqa: BLE001
        return None


def split(events: list) -> dict:
    path_of: dict[str, str] = {}
    firsts: list[tuple[str, datetime]] = []
    seen: set[str] = set()
    for event in events:
        if not (isinstance(event, dict) and event.get("type") == "requirement_state"):
            continue
        message = str(event.get("message") or "")
        node = event.get("node_id")
        phase = event.get("phase")
        if phase == "design":
            if "atomic children designed" in message:
                continue                      # parent node, not a leaf
            path_of[node] = "tool" if "design JSON written" in message else "codegen"
        if phase == "test" and not message.startswith("children not verified"):
            if node not in seen:
                seen.add(node)
                when = stamp(event.get("timestamp"))
                if when:
                    firsts.append((node, when))
    gaps: dict[str, list[float]] = {"codegen": [], "tool": []}
    for i in range(1, len(firsts)):
        # Interval between consecutive first verdicts -- see trap 1 in the module docstring.
        gaps[path_of.get(firsts[i][0], "codegen")].append(
            (firsts[i][1] - firsts[i - 1][1]).total_seconds())
    return {"path_of": path_of, "gaps": gaps}


def report(opener, run_id: str) -> None:
    run = get(opener, f"/runs/{run_id}")
    logs = get(opener, f"/runs/{run_id}/logs")
    events = logs.get("visual_events") or []
    if isinstance(events, str):
        events = json.loads(events)
    s = split(events)
    counts = {"codegen": 0, "tool": 0}
    for path in s["path_of"].values():
        counts[path] += 1
    leaves = counts["codegen"] + counts["tool"]
    passed = run.get("passed_count") or 0
    failed = run.get("failed_count") or 0
    tail = "" if run.get("status") in ("PASSED", "FAILED") else "  (still running -- partial)"
    print(f"[{run_id}] {run.get('requirement_id')} {run.get('status')} {passed}/{passed + failed}"
          f"  CNY {run.get('token_cost_usd') or 0:.4f}  {run.get('token_count')} tok"
          f"  {run.get('run_duration_seconds')}s{tail}")
    if not leaves:
        print("  no leaf nodes with a decided path yet")
        return
    print(f"  leaves {leaves}: codegen {counts['codegen']}, tool {counts['tool']}"
          f" ({100 * counts['tool'] / leaves:.0f}% tool)")
    for path in ("codegen", "tool"):
        v = sorted(s["gaps"][path])
        if v:
            print(f"    {path:<8} {len(v):>3} intervals  median {v[len(v) // 2]:>5.0f}s"
                  f"  total {sum(v) / 60:>6.1f} min")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("run_ids", nargs="+")
    ap.add_argument("--cookie-jar", default=os.environ.get("ARC_COOKIE_JAR"))
    args = ap.parse_args()
    opener = client(args.cookie_jar)
    for run_id in args.run_ids:
        report(opener, run_id)
        print()
    return 0


if __name__ == "__main__":
    sys.exit(main())
