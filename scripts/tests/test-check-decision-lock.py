#!/usr/bin/env python3
"""Regression tests for scripts/check-decision-lock.py (#70).

Each scenario builds a throwaway Git repository with its own tiny
decision-lock.json, so no state leaks between cases and the real repository is
never touched. Runs entirely offline: the two code paths that would reach
GitHub (`--update` and `--upstream`) are exercised against an injected fake
tree rather than the network.

Scenarios
---------
  Offline verification
    1. Tree matches the lock                              -> 0
    2. An inherited record edited downstream              -> 1
    3. An inherited record deleted                        -> 1
    4. A new record matching a group, uninventoried       -> 1  (coverage)
    5. Divergence from upstream WITH a delta.reason       -> 0
    6. Divergence from upstream WITHOUT a delta.reason    -> 1
    7. Divergence with a blank/whitespace delta.reason    -> 1
    8. A file outside every group pattern is ignored      -> 0

  UPCR numbering
    9. Acknowledged collision                             -> 0
   10. New collision                                      -> 1
   11. Acknowledged collision whose members changed       -> 1
   12. New numbering gap                                  -> 1

  Lock integrity
   13. Missing lock file                                  -> 2
   14. Malformed JSON                                     -> 2
   15. Unknown schema_version                             -> 2
   16. Missing required upstream.commit                   -> 2

  Network-backed paths, with the upstream tree injected
   17. --update rebuilds the inventory and then verifies  -> 0
   18. --update preserves an existing delta               -> 0
   19. --upstream rejects a lock that misstates upstream  -> 1
   20. --upstream reports changed/deleted/added on main   -> 0 (advisory)
   21. --upstream separates an absorbed delta from drift   -> reported apart

  Diagnosis quality
   22. An uncommitted edit is named as uncommitted, not drift

Usage: python3 scripts/tests/test-check-decision-lock.py
"""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import subprocess
import sys
import tempfile
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parents[1]
TARGET = REPO_ROOT / "scripts" / "check-decision-lock.py"

PASS = 0
FAIL = 0


def ok(msg: str) -> None:
    global PASS
    PASS += 1
    print(f"  OK:   {msg}")


def bad(msg: str) -> None:
    global FAIL
    FAIL += 1
    print(f"  FAIL: {msg}", file=sys.stderr)


def load_module():
    """Import the checker by path so its internals can be exercised directly.

    Bytecode writing is disabled first: importing by path would otherwise drop
    a scripts/__pycache__/ directory into the working tree, and .gitignore
    does not cover it.
    """
    sys.dont_write_bytecode = True
    spec = importlib.util.spec_from_file_location("check_decision_lock", TARGET)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


MOD = load_module()


# ── fixtures ──────────────────────────────────────────────────────────


def make_repo(tmp: Path, files: dict[str, str]) -> Path:
    """Create a git repo containing `files` and commit them."""
    root = tmp / "repo"
    root.mkdir()
    for rel, content in files.items():
        target = root / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content, encoding="utf-8")
    env = {
        "GIT_AUTHOR_NAME": "t",
        "GIT_AUTHOR_EMAIL": "t@example.com",
        "GIT_COMMITTER_NAME": "t",
        "GIT_COMMITTER_EMAIL": "t@example.com",
        "PATH": __import__("os").environ["PATH"],
    }
    subprocess.run(["git", "init", "-q"], cwd=root, check=True, env=env)
    subprocess.run(["git", "add", "-A"], cwd=root, check=True, env=env)
    subprocess.run(
        ["git", "commit", "-q", "-m", "fixture"], cwd=root, check=True, env=env
    )
    return root


def blob_sha(root: Path, rel: str) -> str:
    return subprocess.run(
        ["git", "rev-parse", f"HEAD:{rel}"],
        cwd=root,
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()


def base_lock(**overrides) -> dict:
    lock = {
        "schema_version": 1,
        "upstream": {
            "repository": "octos-org/octos",
            "commit": "0" * 40,
            "drift_ref": "main",
        },
        "groups": {
            "adr": {"patterns": ["docs/adr/*"]},
            "upcr": {"patterns": ["docs/UPCR_*.md"]},
        },
        "entries": [],
        "upcr_numbering": {"known_collisions": {}, "known_gaps": []},
    }
    lock.update(overrides)
    return lock


def write_lock(root: Path, lock: dict) -> Path:
    path = root / "decision-lock.json"
    path.write_text(json.dumps(lock, indent=2), encoding="utf-8")
    return path


def run_check(root: Path, *args: str) -> int:
    """Invoke the checker's main() in-process against `root`.

    The checker's own stdout/stderr is swallowed so the scenario list stays
    readable; a failing assertion reports the exit code, which is the
    contract under test.
    """
    with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(
        io.StringIO()
    ):
        return MOD.main(["--root", str(root), *args])


def scenario(name):
    """Decorator: run a test body inside a fresh temp dir."""

    def wrap(fn):
        def runner():
            with tempfile.TemporaryDirectory() as tmp:
                try:
                    fn(Path(tmp))
                except AssertionError as exc:
                    bad(f"{name}: {exc}")
                except Exception as exc:  # noqa: BLE001 - surface as a failure
                    bad(f"{name}: unexpected {type(exc).__name__}: {exc}")

        runner.__name__ = fn.__name__
        runner.label = name
        return runner

    return wrap


def expect(actual: int, wanted: int, what: str) -> None:
    assert actual == wanted, f"{what}: expected exit {wanted}, got {actual}"


# ── offline verification ──────────────────────────────────────────────


@scenario("clean tree matching the lock passes")
def t01(tmp):
    root = make_repo(tmp, {"docs/adr/a.md": "alpha\n"})
    sha = blob_sha(root, "docs/adr/a.md")
    write_lock(
        root,
        base_lock(
            entries=[
                {
                    "path": "docs/adr/a.md",
                    "group": "adr",
                    "arc_blob_sha": sha,
                    "upstream_blob_sha": sha,
                }
            ]
        ),
    )
    expect(run_check(root), 0, "clean tree")
    ok("clean tree matching the lock passes")


@scenario("a downstream edit to an inherited record fails")
def t02(tmp):
    root = make_repo(tmp, {"docs/adr/a.md": "alpha\n"})
    write_lock(
        root,
        base_lock(
            entries=[
                {
                    "path": "docs/adr/a.md",
                    "group": "adr",
                    "arc_blob_sha": "f" * 40,
                    "upstream_blob_sha": "f" * 40,
                }
            ]
        ),
    )
    expect(run_check(root), 1, "edited record")
    ok("a downstream edit to an inherited record fails")


@scenario("a deleted inherited record fails")
def t03(tmp):
    root = make_repo(tmp, {"docs/adr/a.md": "alpha\n"})
    sha = blob_sha(root, "docs/adr/a.md")
    write_lock(
        root,
        base_lock(
            entries=[
                {
                    "path": "docs/adr/gone.md",
                    "group": "adr",
                    "arc_blob_sha": sha,
                    "upstream_blob_sha": sha,
                }
            ]
        ),
    )
    expect(run_check(root), 1, "deleted record")
    ok("a deleted inherited record fails")


@scenario("an uninventoried record matching a group fails (coverage)")
def t04(tmp):
    root = make_repo(tmp, {"docs/adr/a.md": "alpha\n", "docs/adr/b.md": "beta\n"})
    sha = blob_sha(root, "docs/adr/a.md")
    write_lock(
        root,
        base_lock(
            entries=[
                {
                    "path": "docs/adr/a.md",
                    "group": "adr",
                    "arc_blob_sha": sha,
                    "upstream_blob_sha": sha,
                }
            ]
        ),
    )
    expect(run_check(root), 1, "uninventoried record")
    ok("an uninventoried record matching a group fails (coverage)")


@scenario("divergence from upstream with a delta.reason passes")
def t05(tmp):
    root = make_repo(tmp, {"docs/adr/a.md": "alpha edited\n"})
    sha = blob_sha(root, "docs/adr/a.md")
    write_lock(
        root,
        base_lock(
            entries=[
                {
                    "path": "docs/adr/a.md",
                    "group": "adr",
                    "arc_blob_sha": sha,
                    "upstream_blob_sha": "f" * 40,
                    "delta": {"reason": "backported security warning"},
                }
            ]
        ),
    )
    expect(run_check(root), 0, "declared delta")
    ok("divergence from upstream with a delta.reason passes")


@scenario("divergence from upstream without a delta.reason fails")
def t06(tmp):
    root = make_repo(tmp, {"docs/adr/a.md": "alpha edited\n"})
    sha = blob_sha(root, "docs/adr/a.md")
    write_lock(
        root,
        base_lock(
            entries=[
                {
                    "path": "docs/adr/a.md",
                    "group": "adr",
                    "arc_blob_sha": sha,
                    "upstream_blob_sha": "f" * 40,
                }
            ]
        ),
    )
    expect(run_check(root), 1, "undeclared delta")
    ok("divergence from upstream without a delta.reason fails")


@scenario("a blank delta.reason does not count as a declaration")
def t07(tmp):
    root = make_repo(tmp, {"docs/adr/a.md": "alpha edited\n"})
    sha = blob_sha(root, "docs/adr/a.md")
    write_lock(
        root,
        base_lock(
            entries=[
                {
                    "path": "docs/adr/a.md",
                    "group": "adr",
                    "arc_blob_sha": sha,
                    "upstream_blob_sha": "f" * 40,
                    "delta": {"reason": "   "},
                }
            ]
        ),
    )
    expect(run_check(root), 1, "blank delta reason")
    ok("a blank delta.reason does not count as a declaration")


@scenario("a file outside every group pattern is ignored")
def t08(tmp):
    root = make_repo(tmp, {"docs/adr/a.md": "alpha\n", "README.md": "unrelated\n"})
    sha = blob_sha(root, "docs/adr/a.md")
    write_lock(
        root,
        base_lock(
            entries=[
                {
                    "path": "docs/adr/a.md",
                    "group": "adr",
                    "arc_blob_sha": sha,
                    "upstream_blob_sha": sha,
                }
            ]
        ),
    )
    expect(run_check(root), 0, "unrelated file")
    ok("a file outside every group pattern is ignored")


# ── UPCR numbering ────────────────────────────────────────────────────


def upcr_repo(tmp: Path, names: list[str]) -> tuple[Path, list[dict]]:
    files = {f"docs/{n}": f"{n}\n" for n in names}
    root = make_repo(tmp, files)
    entries = [
        {
            "path": f"docs/{n}",
            "group": "upcr",
            "arc_blob_sha": blob_sha(root, f"docs/{n}"),
            "upstream_blob_sha": blob_sha(root, f"docs/{n}"),
        }
        for n in names
    ]
    return root, entries


@scenario("an acknowledged collision passes")
def t09(tmp):
    root, entries = upcr_repo(tmp, ["UPCR_2026_001_A.md", "UPCR_2026_001_B.md"])
    write_lock(
        root,
        base_lock(
            entries=entries,
            upcr_numbering={
                "known_collisions": {
                    "001": ["UPCR_2026_001_A.md", "UPCR_2026_001_B.md"]
                },
                "known_gaps": [],
            },
        ),
    )
    expect(run_check(root), 0, "acknowledged collision")
    ok("an acknowledged collision passes")


@scenario("a new collision fails")
def t10(tmp):
    root, entries = upcr_repo(tmp, ["UPCR_2026_001_A.md", "UPCR_2026_001_B.md"])
    write_lock(root, base_lock(entries=entries))
    expect(run_check(root), 1, "new collision")
    ok("a new collision fails")


@scenario("an acknowledged collision with changed members fails")
def t11(tmp):
    root, entries = upcr_repo(tmp, ["UPCR_2026_001_A.md", "UPCR_2026_001_B.md"])
    write_lock(
        root,
        base_lock(
            entries=entries,
            upcr_numbering={
                "known_collisions": {
                    "001": ["UPCR_2026_001_A.md", "UPCR_2026_001_C.md"]
                },
                "known_gaps": [],
            },
        ),
    )
    expect(run_check(root), 1, "changed collision members")
    ok("an acknowledged collision with changed members fails")


@scenario("a new numbering gap fails")
def t12(tmp):
    root, entries = upcr_repo(tmp, ["UPCR_2026_001_A.md", "UPCR_2026_003_C.md"])
    write_lock(root, base_lock(entries=entries))
    expect(run_check(root), 1, "numbering gap")
    ok("a new numbering gap fails")


# ── lock integrity ────────────────────────────────────────────────────


@scenario("a missing lock file is an environment error")
def t13(tmp):
    root = make_repo(tmp, {"docs/adr/a.md": "alpha\n"})
    expect(run_check(root), 2, "missing lock")
    ok("a missing lock file is an environment error")


@scenario("malformed JSON is an environment error")
def t14(tmp):
    root = make_repo(tmp, {"docs/adr/a.md": "alpha\n"})
    (root / "decision-lock.json").write_text("{ not json", encoding="utf-8")
    expect(run_check(root), 2, "malformed lock")
    ok("malformed JSON is an environment error")


@scenario("an unknown schema_version is an environment error")
def t15(tmp):
    root = make_repo(tmp, {"docs/adr/a.md": "alpha\n"})
    write_lock(root, base_lock(schema_version=99))
    expect(run_check(root), 2, "bad schema version")
    ok("an unknown schema_version is an environment error")


@scenario("a lock missing upstream.commit is an environment error")
def t16(tmp):
    root = make_repo(tmp, {"docs/adr/a.md": "alpha\n"})
    lock = base_lock()
    del lock["upstream"]["commit"]
    write_lock(root, lock)
    expect(run_check(root), 2, "missing upstream.commit")
    ok("a lock missing upstream.commit is an environment error")


# ── network-backed paths, upstream tree injected ──────────────────────


class FakeUpstream:
    """Stand in for github_tree() so --update / --upstream stay offline."""

    def __init__(self, by_ref: dict[str, dict[str, str]]):
        self.by_ref = by_ref
        self.original = MOD.github_tree

    def __enter__(self):
        def fake(repo: str, ref: str) -> dict[str, str]:
            if ref not in self.by_ref:
                raise MOD.CheckError(f"fake upstream has no ref {ref}")
            return self.by_ref[ref]

        MOD.github_tree = fake
        return self

    def __exit__(self, *exc):
        MOD.github_tree = self.original
        return False


@scenario("--update rebuilds the inventory, and the result verifies")
def t17(tmp):
    root = make_repo(tmp, {"docs/adr/a.md": "alpha\n", "docs/adr/b.md": "beta\n"})
    sha_a = blob_sha(root, "docs/adr/a.md")
    sha_b = blob_sha(root, "docs/adr/b.md")
    write_lock(root, base_lock())  # empty inventory
    expect(run_check(root), 1, "empty inventory should fail coverage first")

    pinned = {"docs/adr/a.md": sha_a, "docs/adr/b.md": sha_b}
    with FakeUpstream({"0" * 40: pinned, "main": pinned}):
        expect(run_check(root, "--update"), 0, "update")
    lock = json.loads((root / "decision-lock.json").read_text(encoding="utf-8"))
    assert len(lock["entries"]) == 2, f"expected 2 entries, got {len(lock['entries'])}"
    expect(run_check(root), 0, "verify after update")
    ok("--update rebuilds the inventory, and the result verifies")


@scenario("--update preserves a declared delta")
def t18(tmp):
    root = make_repo(tmp, {"docs/adr/a.md": "alpha edited\n"})
    sha = blob_sha(root, "docs/adr/a.md")
    write_lock(
        root,
        base_lock(
            entries=[
                {
                    "path": "docs/adr/a.md",
                    "group": "adr",
                    "arc_blob_sha": sha,
                    "upstream_blob_sha": "f" * 40,
                    "delta": {"reason": "backported security warning"},
                }
            ]
        ),
    )
    pinned = {"docs/adr/a.md": "f" * 40}
    with FakeUpstream({"0" * 40: pinned, "main": pinned}):
        expect(run_check(root, "--update"), 0, "update with delta")
    lock = json.loads((root / "decision-lock.json").read_text(encoding="utf-8"))
    entry = lock["entries"][0]
    assert entry.get("delta", {}).get("reason"), "delta.reason was dropped by --update"
    expect(run_check(root), 0, "verify after update keeps the delta valid")
    ok("--update preserves a declared delta")


@scenario("--upstream rejects a lock that misstates upstream")
def t19(tmp):
    root = make_repo(tmp, {"docs/adr/a.md": "alpha\n"})
    sha = blob_sha(root, "docs/adr/a.md")
    write_lock(
        root,
        base_lock(
            entries=[
                {
                    "path": "docs/adr/a.md",
                    "group": "adr",
                    "arc_blob_sha": sha,
                    # claims upstream matches, but upstream actually has 'f'*40
                    "upstream_blob_sha": sha,
                }
            ]
        ),
    )
    expect(run_check(root), 0, "offline check cannot see the lie")
    with FakeUpstream({"0" * 40: {"docs/adr/a.md": "f" * 40}, "main": {}}):
        expect(run_check(root, "--upstream"), 1, "upstream check catches it")
    ok("--upstream rejects a lock that misstates upstream")


@scenario("--upstream reports movement on main without failing")
def t20(tmp):
    root = make_repo(tmp, {"docs/adr/a.md": "alpha\n"})
    sha = blob_sha(root, "docs/adr/a.md")
    write_lock(
        root,
        base_lock(
            entries=[
                {
                    "path": "docs/adr/a.md",
                    "group": "adr",
                    "arc_blob_sha": sha,
                    "upstream_blob_sha": sha,
                }
            ]
        ),
    )
    pinned = {"docs/adr/a.md": sha}
    moved = {"docs/adr/a.md": "e" * 40, "docs/adr/new.md": "d" * 40}
    with FakeUpstream({"0" * 40: pinned, "main": moved}):
        expect(run_check(root, "--upstream"), 0, "drift is advisory")
        problems, report = MOD.check_upstream(
            json.loads((root / "decision-lock.json").read_text(encoding="utf-8"))
        )
    assert not problems, f"unexpected problems: {problems}"
    assert report["changed_since_pin"] == ["docs/adr/a.md"], report
    assert report["added_since_pin"] == ["docs/adr/new.md"], report
    ok("--upstream reports movement on main without failing")


@scenario("--upstream reports an absorbed delta separately from open drift")
def t21(tmp):
    root = make_repo(tmp, {"docs/adr/a.md": "backported\n", "docs/adr/b.md": "beta\n"})
    sha_a = blob_sha(root, "docs/adr/a.md")
    sha_b = blob_sha(root, "docs/adr/b.md")
    write_lock(
        root,
        base_lock(
            entries=[
                {
                    # arc carries a backport; upstream@pin had something older,
                    # and upstream main has since landed exactly what arc has.
                    "path": "docs/adr/a.md",
                    "group": "adr",
                    "arc_blob_sha": sha_a,
                    "upstream_blob_sha": "f" * 40,
                    "delta": {"reason": "backported security warning"},
                },
                {
                    "path": "docs/adr/b.md",
                    "group": "adr",
                    "arc_blob_sha": sha_b,
                    "upstream_blob_sha": sha_b,
                },
            ]
        ),
    )
    pinned = {"docs/adr/a.md": "f" * 40, "docs/adr/b.md": sha_b}
    # main: a.md now equals what arc already carries; b.md genuinely moved.
    moved = {"docs/adr/a.md": sha_a, "docs/adr/b.md": "e" * 40}
    with FakeUpstream({"0" * 40: pinned, "main": moved}):
        _problems, report = MOD.check_upstream(
            json.loads((root / "decision-lock.json").read_text(encoding="utf-8"))
        )
    assert report["absorbed_since_pin"] == ["docs/adr/a.md"], report
    assert report["changed_since_pin"] == ["docs/adr/b.md"], report
    ok("--upstream reports an absorbed delta separately from open drift")


@scenario("an uncommitted edit is diagnosed as uncommitted, not as drift")
def t22(tmp):
    root = make_repo(tmp, {"docs/adr/a.md": "alpha\n"})
    sha = blob_sha(root, "docs/adr/a.md")
    write_lock(
        root,
        base_lock(
            entries=[
                {
                    "path": "docs/adr/a.md",
                    "group": "adr",
                    "arc_blob_sha": sha,
                    "upstream_blob_sha": sha,
                }
            ]
        ),
    )
    expect(run_check(root), 0, "committed state passes")

    # Edit without committing: HEAD still matches the lock, the worktree does
    # not. The gate must say "uncommitted", not "edited downstream".
    (root / "docs/adr/a.md").write_text("alpha changed\n", encoding="utf-8")
    subprocess.run(["git", "add", "-A"], cwd=root, check=True)

    lock = json.loads((root / "decision-lock.json").read_text(encoding="utf-8"))
    tree = MOD.git_tree(root)
    dirty = MOD.dirty_paths(root)
    assert "docs/adr/a.md" in dirty, f"dirty detection missed the edit: {dirty}"

    # Point the lock at the new content so the committed tree now mismatches.
    lock["entries"][0]["arc_blob_sha"] = "a" * 40
    lock["entries"][0]["upstream_blob_sha"] = "a" * 40
    problems = MOD.check_entries(lock, tree, dirty)
    assert len(problems) == 1, problems
    assert "uncommitted" in problems[0], problems[0]

    # The same mismatch with a clean worktree must keep the drift wording.
    problems_clean = MOD.check_entries(lock, tree, set())
    assert len(problems_clean) == 1, problems_clean
    assert "uncommitted" not in problems_clean[0], problems_clean[0]
    assert "edited downstream" in problems_clean[0], problems_clean[0]
    ok("an uncommitted edit is diagnosed as uncommitted, not as drift")


TESTS = [
    t01, t02, t03, t04, t05, t06, t07, t08,
    t09, t10, t11, t12,
    t13, t14, t15, t16,
    t17, t18, t19, t20, t21, t22,
]


def main() -> int:
    print(f"test-check-decision-lock: {len(TESTS)} scenarios\n")
    for test in TESTS:
        test()
    print(f"\n{PASS} passed, {FAIL} failed")
    return 1 if FAIL else 0


if __name__ == "__main__":
    sys.exit(main())
