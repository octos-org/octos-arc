#!/usr/bin/env python3
"""Verify decision-lock.json — the blob-SHA pin on inherited upstream records.

The competition baseline pins the CODE (`8558a3bf` + the Cargo.lock SHA-256 in
`arc-runtime-lock.json`) but, before this gate existed, nothing pinned what it
INHERITED. Two kinds of thing are at risk:

  * the decision records — spec / ADR / UPCR / protocol documents, which could
    be edited downstream or superseded upstream without anything noticing (#70);
  * the vendored documentation — the en/zh books and docs/ARCHITECTURE.md, which
    participants read to operate the runtime this tree builds, and which had no
    pin statement and no sync check at all (#215).

`decision-lock.json` inventories both by Git blob SHA; this script is the check.

A Git blob SHA is a content hash, so "same blob SHA" means "byte-identical"
with no diffing and no network.

Modes
-----
    check-decision-lock.py                 # offline verify (the CI gate)
    check-decision-lock.py --upstream      # + confirm the lock against upstream
    check-decision-lock.py --update        # regenerate the lock from upstream
    check-decision-lock.py --json          # machine-readable result

Offline verification answers "is this tree still what the lock says?":

    A. every inventoried path still exists in the tree;
    B. its committed blob SHA equals the recorded `arc_blob_sha`;
    C. any entry whose `arc_blob_sha` differs from `upstream_blob_sha` carries
       a non-empty `delta.reason` — divergence from upstream is allowed, but
       only on the record;
    D. coverage — every tree path matching a group's patterns is inventoried,
       so a newly added decision record cannot slip in unpinned;
    E. UPCR numbering — no collision or gap beyond the ones the lock
       acknowledges as inherited from upstream.

`--upstream` additionally fetches the upstream tree at the pinned commit and
confirms each `upstream_blob_sha` really is what upstream has there (a lock
edited by hand would otherwise verify against itself), then reports records
that upstream `main` has changed, deleted, or added since the pin. That report
is advisory: moving past the pin is a deliberate act, not a build failure.

Blob SHAs are read from `git ls-tree -r HEAD`, i.e. committed content. That is
the thing a lock file can meaningfully pin, and it is immune to working-tree
line-ending translation.

Exit codes: 0 = pass, 1 = drift / violation, 2 = usage or environment error.
"""

from __future__ import annotations

import argparse
import fnmatch
import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.request
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
LOCK_PATH = REPO_ROOT / "decision-lock.json"
SCHEMA_VERSION = 1
UPCR_RE = re.compile(r"UPCR_(\d{4})_(\d{3})")
API_ROOT = "https://api.github.com"


class CheckError(Exception):
    """Environment or usage failure — distinct from a drift finding."""


# ── tree access ───────────────────────────────────────────────────────


def git_tree(root: Path, ref: str = "HEAD") -> dict[str, str]:
    """Return {path: blob_sha} for every blob reachable from `ref`."""
    try:
        out = subprocess.run(
            ["git", "ls-tree", "-r", ref],
            cwd=root,
            check=True,
            capture_output=True,
            text=True,
        ).stdout
    except FileNotFoundError as exc:  # pragma: no cover - environment
        raise CheckError("git is not on PATH") from exc
    except subprocess.CalledProcessError as exc:
        raise CheckError(
            f"`git ls-tree -r {ref}` failed in {root}: {exc.stderr.strip()}"
        ) from exc

    tree: dict[str, str] = {}
    for line in out.splitlines():
        if not line:
            continue
        # "<mode> <type> <sha>\t<path>" — the classic format, portable across
        # git versions (`--format` only landed in 2.36).
        meta, _, path = line.partition("\t")
        parts = meta.split()
        if len(parts) != 3 or not path:
            continue
        _mode, kind, sha = parts
        if kind == "blob":
            tree[path] = sha
    return tree


def dirty_paths(root: Path) -> set[str]:
    """Paths with staged or unstaged changes, so mismatches can be explained.

    The lock is verified against committed content, which is the right thing
    for a lock file but confusing mid-edit: without this, a staged-but-
    uncommitted change reports as "edited downstream" when the real answer is
    "not committed yet".
    """
    try:
        out = subprocess.run(
            ["git", "status", "--porcelain", "--untracked-files=all"],
            cwd=root,
            check=True,
            capture_output=True,
            text=True,
        ).stdout
    except (FileNotFoundError, subprocess.CalledProcessError):  # pragma: no cover
        return set()

    paths: set[str] = set()
    for line in out.splitlines():
        if len(line) < 4:
            continue
        path = line[3:]
        if " -> " in path:  # rename: take the destination
            path = path.split(" -> ", 1)[1]
        paths.add(path.strip('"'))
    return paths


def github_tree(repo: str, ref: str) -> dict[str, str]:
    """Return {path: blob_sha} for a GitHub repo at `ref`, recursively."""
    url = f"{API_ROOT}/repos/{repo}/git/trees/{ref}?recursive=1"
    req = urllib.request.Request(url, headers={"Accept": "application/vnd.github+json"})
    token = os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN")
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            payload = json.load(resp)
    except urllib.error.HTTPError as exc:
        hint = ""
        if exc.code in (401, 403):
            hint = " (set GITHUB_TOKEN to raise the rate limit)"
        raise CheckError(f"GitHub API {exc.code} for {repo}@{ref}{hint}") from exc
    except urllib.error.URLError as exc:
        raise CheckError(f"cannot reach the GitHub API: {exc.reason}") from exc

    if payload.get("truncated"):
        raise CheckError(
            f"the tree listing for {repo}@{ref} was truncated by the API; "
            "cannot verify the lock against a partial tree"
        )
    return {
        item["path"]: item["sha"]
        for item in payload.get("tree", [])
        if item.get("type") == "blob"
    }


# ── lock model ────────────────────────────────────────────────────────


def load_lock(path: Path) -> dict:
    if not path.exists():
        raise CheckError(f"{path} does not exist; run with --update to create it")
    try:
        lock = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        raise CheckError(f"{path} is not valid JSON: {exc}") from exc

    version = lock.get("schema_version")
    if version != SCHEMA_VERSION:
        raise CheckError(
            f"{path} has schema_version {version!r}; this script understands "
            f"{SCHEMA_VERSION}"
        )
    for field in ("upstream", "groups", "entries"):
        if field not in lock:
            raise CheckError(f"{path} is missing the required '{field}' field")
    for field in ("repository", "commit"):
        if not lock["upstream"].get(field):
            raise CheckError(f"{path}: upstream.{field} is required")
    return lock


def patterns_of(lock: dict) -> list[tuple[str, str]]:
    """Flatten groups into (pattern, group_name) pairs."""
    pairs: list[tuple[str, str]] = []
    for name, group in lock["groups"].items():
        for pattern in group.get("patterns", []):
            pairs.append((pattern, name))
    return pairs


def matches(path: str, pairs: list[tuple[str, str]]) -> str | None:
    """Return the group a path belongs to, or None."""
    for pattern, group in pairs:
        if fnmatch.fnmatchcase(path, pattern):
            return group
    return None


# ── checks ────────────────────────────────────────────────────────────


def check_entries(
    lock: dict, tree: dict[str, str], dirty: set[str] | None = None
) -> list[str]:
    """Checks A, B and C: presence, pinned content, and recorded divergence."""
    dirty = dirty or set()
    problems: list[str] = []
    for entry in lock["entries"]:
        path = entry["path"]
        recorded = entry["arc_blob_sha"]
        upstream = entry.get("upstream_blob_sha")
        actual = tree.get(path)

        if actual is None:
            problems.append(
                f"{path}: inventoried in decision-lock.json but absent from the "
                f"tree. Restore it, or drop the entry and say why in the PR."
            )
            continue

        if actual != recorded:
            if path in dirty:
                problems.append(
                    f"{path}: has uncommitted changes. The lock is verified "
                    f"against committed content, so commit first, then re-run "
                    f"`scripts/check-decision-lock.py --update` if the change "
                    f"is intended."
                )
            else:
                problems.append(
                    f"{path}: content changed (locked {recorded[:12]}, found "
                    f"{actual[:12]}). An inherited record was edited "
                    f"downstream. If that is intended, re-run "
                    f"`scripts/check-decision-lock.py --update` and record a "
                    f"delta.reason explaining the divergence."
                )
            continue

        if upstream and recorded != upstream:
            reason = (entry.get("delta") or {}).get("reason", "").strip()
            if not reason:
                problems.append(
                    f"{path}: diverges from upstream@pin ({upstream[:12]} -> "
                    f"{recorded[:12]}) with no delta.reason. Every intentional "
                    f"divergence must be stated in the lock."
                )
    return problems


def check_coverage(lock: dict, tree: dict[str, str]) -> list[str]:
    """Check D: nothing matching a group's patterns may go uninventoried."""
    pairs = patterns_of(lock)
    inventoried = {entry["path"] for entry in lock["entries"]}
    missing = sorted(
        path
        for path in tree
        if path not in inventoried and matches(path, pairs) is not None
    )
    return [
        f"{path}: matches a decision-lock group but is not inventoried. Run "
        f"`scripts/check-decision-lock.py --update` to pin it."
        for path in missing
    ]


def upcr_numbering(tree_paths) -> tuple[dict[str, list[str]], list[str]]:
    """Return ({number: [filenames]} for collisions, [missing numbers])."""
    seen: dict[str, list[str]] = {}
    for path in tree_paths:
        match = UPCR_RE.search(path)
        if match:
            seen.setdefault(match.group(2), []).append(os.path.basename(path))
    collisions = {n: sorted(f) for n, f in seen.items() if len(f) > 1}
    numbers = sorted(int(n) for n in seen)
    gaps = [f"{n:03d}" for n in range(1, max(numbers) + 1) if n not in numbers] if numbers else []
    return collisions, gaps


def check_upcr(lock: dict, tree: dict[str, str]) -> list[str]:
    """Check E: only upstream-inherited numbering defects are tolerated."""
    known = lock.get("upcr_numbering", {})
    known_collisions = known.get("known_collisions", {})
    known_gaps = set(known.get("known_gaps", []))

    collisions, gaps = upcr_numbering(tree)
    problems: list[str] = []

    for number, files in sorted(collisions.items()):
        if number not in known_collisions:
            problems.append(
                f"UPCR-{number}: new numbering collision between "
                f"{', '.join(files)}. Renumber one of them."
            )
        elif sorted(known_collisions[number]) != files:
            problems.append(
                f"UPCR-{number}: the acknowledged collision changed members "
                f"(lock: {', '.join(sorted(known_collisions[number]))}; tree: "
                f"{', '.join(files)})."
            )
    for number in gaps:
        if number not in known_gaps:
            problems.append(
                f"UPCR-{number}: numbering gap not acknowledged in the lock. "
                f"A record may have been deleted."
            )
    return problems


def check_upstream(lock: dict) -> tuple[list[str], dict]:
    """Confirm the lock against upstream@pin, and report movement on main."""
    repo = lock["upstream"]["repository"]
    commit = lock["upstream"]["commit"]
    pinned = github_tree(repo, commit)

    problems: list[str] = []
    for entry in lock["entries"]:
        recorded = entry.get("upstream_blob_sha")
        if not recorded:
            continue
        upstream_path = entry.get("upstream_path", entry["path"])
        actual = pinned.get(upstream_path)
        if actual is None:
            problems.append(
                f"{entry['path']}: upstream_blob_sha recorded, but "
                f"{upstream_path} does not exist in {repo}@{commit[:12]}."
            )
        elif actual != recorded:
            problems.append(
                f"{entry['path']}: lock records upstream_blob_sha "
                f"{recorded[:12]} but {repo}@{commit[:12]} has {actual[:12]}. "
                f"The lock does not describe the pinned base."
            )

    head = github_tree(repo, lock["upstream"].get("drift_ref", "main"))
    pairs = patterns_of(lock)
    inventoried = {entry.get("upstream_path", entry["path"]) for entry in lock["entries"]}

    changed, deleted, absorbed = [], [], []
    for entry in lock["entries"]:
        upstream_path = entry.get("upstream_path", entry["path"])
        recorded = entry.get("upstream_blob_sha")
        if not recorded:
            continue
        current = head.get(upstream_path)
        if current is None:
            deleted.append(upstream_path)
        elif current != recorded:
            if current == entry["arc_blob_sha"]:
                # A recorded delta already brought this file up to what
                # upstream now has. Reporting it as outstanding drift would
                # keep it in the weekly issue forever with nothing to do.
                absorbed.append(upstream_path)
            else:
                changed.append(upstream_path)

    added = sorted(
        path
        for path in head
        if path not in inventoried and matches(path, pairs) is not None
    )
    report = {
        "upstream": repo,
        "pinned_commit": commit,
        "drift_ref": lock["upstream"].get("drift_ref", "main"),
        "changed_since_pin": sorted(changed),
        "deleted_since_pin": sorted(deleted),
        "added_since_pin": added,
        "absorbed_since_pin": sorted(absorbed),
    }
    return problems, report


# ── update ────────────────────────────────────────────────────────────


def update(lock: dict, tree: dict[str, str]) -> dict:
    """Rebuild the entry inventory from this tree plus upstream@pin."""
    repo = lock["upstream"]["repository"]
    commit = lock["upstream"]["commit"]
    pinned = github_tree(repo, commit)
    pairs = patterns_of(lock)
    previous = {entry["path"]: entry for entry in lock.get("entries", [])}

    entries = []
    for path in sorted(tree):
        group = matches(path, pairs)
        if group is None:
            continue
        old = previous.get(path, {})
        upstream_path = old.get("upstream_path", path)
        entry = {
            "path": path,
            "group": group,
            "arc_blob_sha": tree[path],
        }
        if upstream_path != path:
            entry["upstream_path"] = upstream_path
        if upstream_path in pinned:
            entry["upstream_blob_sha"] = pinned[upstream_path]
        if tree[path] != pinned.get(upstream_path) and old.get("delta"):
            entry["delta"] = old["delta"]
        entries.append(entry)

    collisions, gaps = upcr_numbering(tree)
    lock["entries"] = entries
    lock["upcr_numbering"] = {
        **lock.get("upcr_numbering", {}),
        "known_collisions": collisions,
        "known_gaps": gaps,
    }
    return lock


# ── main ──────────────────────────────────────────────────────────────


def run(args: argparse.Namespace) -> int:
    root = Path(args.root).resolve() if args.root else REPO_ROOT
    lock_path = Path(args.lock).resolve() if args.lock else root / "decision-lock.json"

    lock = load_lock(lock_path)
    tree = git_tree(root)

    if args.update:
        lock = update(lock, tree)
        lock_path.write_text(
            json.dumps(lock, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
        )
        print(
            f"decision-lock: wrote {len(lock['entries'])} entries to "
            f"{lock_path.relative_to(root) if lock_path.is_relative_to(root) else lock_path}"
        )
        return 0

    problems: list[str] = []
    problems += check_entries(lock, tree, dirty_paths(root))
    problems += check_coverage(lock, tree)
    problems += check_upcr(lock, tree)

    report: dict = {}
    if args.upstream:
        upstream_problems, report = check_upstream(lock)
        problems += upstream_problems

    if args.json:
        print(
            json.dumps(
                {
                    "ok": not problems,
                    "entries": len(lock["entries"]),
                    "problems": problems,
                    "drift": report,
                },
                indent=2,
            )
        )
        return 1 if problems else 0

    if problems:
        print("decision-lock: FAILED\n", file=sys.stderr)
        for problem in problems:
            print(f"  - {problem}", file=sys.stderr)
        print(
            "\nThe baseline pins the decision records and vendored documentation "
            "it inherited, by blob SHA, so a downstream edit or an upstream "
            "supersession cannot pass unnoticed (#70, #215). See ARC_BASELINE.md.",
            file=sys.stderr,
        )
        return 1

    declared = sum(1 for entry in lock["entries"] if entry.get("delta"))
    print(
        f"decision-lock: {len(lock['entries'])} inherited records match the pin "
        f"({lock['upstream']['repository']}@{lock['upstream']['commit'][:12]})"
        + (f", {declared} with a declared delta" if declared else "")
    )
    if args.upstream:
        print(
            f"decision-lock: lock confirmed against upstream@"
            f"{lock['upstream']['commit'][:12]}"
        )
        moved = (
            len(report["changed_since_pin"])
            + len(report["deleted_since_pin"])
            + len(report["added_since_pin"])
        )
        if moved:
            print(
                f"\ndecision-lock: upstream {report['drift_ref']} has moved on "
                f"({len(report['changed_since_pin'])} changed, "
                f"{len(report['deleted_since_pin'])} deleted, "
                f"{len(report['added_since_pin'])} added). Advisory only — the "
                f"baseline stays at the pin until someone moves it deliberately."
            )
            for label, key in (
                ("changed", "changed_since_pin"),
                ("deleted", "deleted_since_pin"),
                ("added", "added_since_pin"),
            ):
                for path in report[key]:
                    print(f"  {label:>7}: {path}")
        else:
            print(f"decision-lock: upstream {report['drift_ref']} matches the pin")
        if report["absorbed_since_pin"]:
            print(
                f"\ndecision-lock: {len(report['absorbed_since_pin'])} file(s) "
                f"already carry what upstream {report['drift_ref']} has, via a "
                f"recorded delta — nothing outstanding:"
            )
            for path in report["absorbed_since_pin"]:
                print(f"  absorbed: {path}")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Verify decision-lock.json against this tree and upstream."
    )
    parser.add_argument(
        "--upstream",
        action="store_true",
        help="confirm the lock against upstream@pin and report movement on main",
    )
    parser.add_argument(
        "--update",
        action="store_true",
        help="regenerate the entry inventory from this tree plus upstream@pin",
    )
    parser.add_argument("--json", action="store_true", help="machine-readable output")
    parser.add_argument("--root", help="repository root (default: this script's repo)")
    parser.add_argument("--lock", help="lock file path (default: <root>/decision-lock.json)")
    args = parser.parse_args(argv)

    try:
        return run(args)
    except CheckError as exc:
        print(f"decision-lock: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
