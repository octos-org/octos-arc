#!/usr/bin/env python3
"""Split a finished cloud run's lost nodes into the two classes that need different fixes.

A single "25/32" says nothing about what to change. Two very different things produce it:

  never-passed   the node's own spec never passed in any round. A capability gap: the
                 repair turns had the evidence and still could not make it work. Levers:
                 failure evidence quality, node budget, repair rounds.
  regressed      the node passed, then a later node's change broke it. A sequencing gap.
                 Levers: regression checkpoint interval, OCTOS_ARC_CHECKPOINT_REPAIRS.

IMPORTANT about what these buckets can and cannot see, because misreading it sent me
to the wrong lever once and the mistake is easy to repeat:

`mark("test_passed"/"test_failed")` fires ONCE per acceptance_loop -- after every
in-node repair round is already over -- so a node that ran five repair rounds and a
node that ran none emit exactly the same single test event. **In-node repair rounds
are invisible here.** The extra test events that make a node read as "recovered"
come from the *later* phases that re-test an already-decided node: the regression
checkpoint (`regression specs pass locally`, `previously regressed behavior passed
its checkpoint specs`) and the final check.

So read them as:
  clean          its acceptance loop ultimately passed -- on round 0 or on round 3,
                 indistinguishable from here. NOT "passed first try": on submission A's
                 keep the adapter log shows 12 of 32 nodes ran repair rounds
                 ({round 0: 20, 1: 8, 2: 2, 3: 2}) while this bucket held 24.
  recovered      failed its own acceptance loop, then passed at a later checkpoint.
                 NOT "its own repair turns fixed it".
  never-passed   failed its own acceptance loop and no later phase revisited it.
                 It did still run up to OCTOS_REPAIR_ROUNDS (default 5, lowered to 3
                 for big trees) in-node repairs -- you just cannot see them from here.
                 To see them you need the adapter's own `[acceptance] <node> round N:`
                 lines, which only reach the cloud log when the container exits.

Concretely: comparing two runs' "passed after repair" counts compares their
*checkpoint recovery*, not their repair-turn quality. Concluding "this model's
repairs recover nothing" from a zero in that column is unsupported.

and one more that only the official grade can reveal:

  hidden interference   the node passed locally but the official pass count is lower.
                 Per-node runs cannot see cross-node interference through shared server
                 state; only a run of every spec against one server can.

Two traps this encodes, both of which produced wrong readings before it existed:

1. Parent nodes emit the same design/implement/test events as leaves, so counting every
   requirement_state inflates both the pass and the loss count (keep 3ffe9702bf15 read as
   "31 passed / 13 lost" instead of "26 passed / 6 lost"). Parents are identified by their
   messages: design "N atomic children designed", test "children not verified: ...".
2. Local node counts and official pass counts are only the same unit when the tree is one
   spec per leaf. Ticket Booking is 2 leaves and 10 specs, so differencing them reads as a
   -5 "gap" that means nothing. The comparison is skipped unless leaves == specs.

usage:
  python3 arc/postmortem.py <run_id> [<run_id> ...]

Needs a session: ARC_COOKIE_JAR (or --cookie-jar) pointing at a Netscape cookie jar with
the site cookie, same as scoreboard.py.
"""
from __future__ import annotations

import argparse
import http.cookiejar
import json
import os
import sys
import time
from pathlib import Path
import urllib.error
import urllib.request

BASE = "https://arc-bench.com/api"


def client(jar_path: str | None):
    jar = http.cookiejar.MozillaCookieJar(jar_path) if jar_path else http.cookiejar.CookieJar()
    if jar_path:
        jar.load(ignore_discard=True, ignore_expires=True)
    return urllib.request.build_opener(urllib.request.HTTPCookieProcessor(jar))


def get(opener, path: str, tries: int = 6):
    """Run logs reach hundreds of kilobytes and the connection drops mid-body often
    enough to matter; read in chunks and retry rather than lose the whole postmortem.

    5xx is retried too: the logs endpoint answers 500 intermittently on the larger runs
    and then serves the same run fine seconds later. 4xx is the caller's fault (wrong run
    id, no session) and is raised immediately instead of being retried six times.
    """
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
            time.sleep(5)
        except Exception as exc:  # noqa: BLE001 - transport flakiness, retry
            last = exc
            time.sleep(5)
    raise last if last else RuntimeError("unreachable")


def classify(events: list) -> dict:
    timeline: dict[str, list[str]] = {}
    parents: set[str] = set()
    suite_verified: set[str] = set()
    for event in events:
        if not (isinstance(event, dict) and event.get("type") == "requirement_state"):
            continue
        message = str(event.get("message") or "")
        node = event.get("node_id")
        phase = event.get("phase")
        if phase == "design" and message.endswith("atomic children designed"):
            parents.add(node)
        if phase == "test":
            if message.startswith("children not verified"):
                parents.add(node)
            if "full parallel suite" in message:
                suite_verified.add(node)
            timeline.setdefault(node, []).append(event.get("status"))
    leaves = {node: hist for node, hist in timeline.items() if node not in parents}
    out = {"clean": [], "recovered": [], "regressed": [], "never_passed": [], "history": leaves,
           "suite_verified": suite_verified}
    for node, hist in leaves.items():
        if hist[-1] == "passed":
            out["recovered" if "failed" in hist else "clean"].append(node)
        elif "passed" in hist:
            out["regressed"].append(node)
        else:
            out["never_passed"].append(node)
    return out


PROVIDER_FAILURES = (
    "insufficient_balance", "quota exhausted", "PermanentProviderError",
    "HTTP 402", "model not found", "rate limit", "Too Many Requests",
)


def provider_failures(logs: dict) -> list[str]:
    """Which provider-level failures appear anywhere in this run's log.

    Without this the "never passed" bucket is ambiguous and reads as a capability
    gap. Submission C's keep is the example: its eight never-passed nodes are
    REQ-2.5.1 and then REQ-3.2 through REQ-6.2 -- consecutive *late* requirements
    in a run that died on `insufficient_balance` at the end. Those nodes almost
    certainly never got a working model call at all, which is a very different
    finding from "the repair turns had the evidence and still could not do it".

    The classification itself only asks whether a spec ever passed, and cannot
    tell the two apart, so the honest move is to say so when the log shows the
    provider failing.
    """
    blob = json.dumps(logs, ensure_ascii=False)
    return [sign for sign in PROVIDER_FAILURES if sign.lower() in blob.lower()]


def postmortem(opener, run_id: str) -> dict:
    run = get(opener, f"/runs/{run_id}")
    logs = get(opener, f"/runs/{run_id}/logs")
    events = logs.get("visual_events") or []
    if isinstance(events, str):
        events = json.loads(events)
    c = classify(events)
    passed = run.get("passed_count") or 0
    failed = run.get("failed_count") or 0
    specs = passed + failed
    leaves = len(c["history"])
    print(f"[{run_id}] {run.get('requirement_id')} {run.get('status')}  official {passed}/{specs}"
          f"  feature {run.get('feature_implementation_rate')}%"
          f"  CNY {run.get('token_cost_usd') or 0:.4f}  {run.get('token_count')} tok"
          f"  {run.get('run_duration_seconds')}s")
    # Both labels were wrong in the same way and for the same reason -- one test event
    # per acceptance_loop, emitted after all in-node repairs. "passed first try" counted
    # every node whose loop ultimately passed, including the ones that needed three
    # repair rounds to get there; on submission A's keep the adapter log shows 12 nodes
    # ran repairs ({round 0: 20, 1: 8, 2: 2, 3: 2}) while this line read 24.
    print(f"  its loop passed (any round) {len(c['clean'])}")
    print(f"  passed at a checkpoint      {len(c['recovered'])} {c['recovered'][:6]}")
    print(f"  regressed (was passing) {len(c['regressed'])} {c['regressed']}")
    print(f"  never passed            {len(c['never_passed'])} {c['never_passed']}")
    broken = provider_failures(logs)
    if broken and c["never_passed"]:
        print(f"  !! the provider failed during this run ({', '.join(broken)}) --"
              f" 'never passed' here may mean those nodes never got a working model call,"
              f" not that they were attempted and could not be done")
    unconfirmed = [n for n in c["clean"] + c["recovered"] if n not in c["suite_verified"]]
    if unconfirmed:
        print(f"  passed its own run but never confirmed by the full suite: "
              f"{len(unconfirmed)} {unconfirmed[:8]}")
    for node in c["regressed"][:5]:
        print(f"    regressed {node}: {' -> '.join(c['history'][node])}")
    for node in c["never_passed"][:5]:
        print(f"    never     {node}: {' -> '.join(c['history'][node])}")
    local = len(c["clean"]) + len(c["recovered"])
    if specs and leaves == specs:
        gap = local - passed
        print(f"  local {local} vs official {passed}  ({leaves} leaves = {specs} specs, same unit)"
              f"  -> hidden interference {gap}")
        if gap > 0:
            print("    >0 means nodes that pass alone break when every spec runs together,"
                  " and nothing caught it during the run")
    elif specs:
        print(f"  local {local} nodes vs official {passed}/{specs} specs -- not one spec per leaf,"
              f" no gap comparison")
    return c


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("run_ids", nargs="+")
    # Default to the driver's jar. Without a cookie every call is 401, and the
    # traceback points at urllib rather than at the missing session -- which cost a
    # detour the first time this was run months after it was written.
    default_jar = os.environ.get("ARC_COOKIE_JAR") or str(Path.home() / ".arc-web-driver" / "session.jar")
    ap.add_argument("--cookie-jar", default=default_jar if Path(default_jar).exists() else None)
    args = ap.parse_args()
    opener = client(args.cookie_jar)
    for run_id in args.run_ids:
        postmortem(opener, run_id)
        print()
    return 0


if __name__ == "__main__":
    sys.exit(main())
