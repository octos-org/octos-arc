import subprocess
import tempfile
import unittest
from pathlib import Path

from acceptance import (
    failure_summaries,
    isolated_install_env,
    map_specs_to_nodes,
    playwright_version_hint,
    nodes_for_failures,
    restore_tree,
    restore_worktree,
    robustness_probe,
    workers_for_memory,
    snapshot_worktree,
    tree_digest,
    spec_node_id,
    summarize_report,
)


class SpecIdTests(unittest.TestCase):
    def test_should_extract_leading_requirement_id(self):
        self.assertEqual(spec_node_id("REQ-1.spec.ts"), "REQ-1")
        self.assertEqual(spec_node_id("REQ-1.1-user-registration.spec.ts"), "REQ-1.1")
        self.assertEqual(spec_node_id("sub/REQ-12.3.4-x.spec.ts"), "REQ-12.3.4")
        self.assertIsNone(spec_node_id("support/e2e.ts"))
        self.assertIsNone(spec_node_id("smoke.spec.ts"))


class MappingTests(unittest.TestCase):
    def test_should_match_exact_ids(self):
        mapping, aliases = map_specs_to_nodes(["REQ-1.spec.ts", "REQ-2.spec.ts"], ["REQ-1", "REQ-2"])
        self.assertEqual(mapping, {"REQ-1": ["REQ-1.spec.ts"], "REQ-2": ["REQ-2.spec.ts"], None: []})
        self.assertEqual(aliases, {})

    def test_should_map_in_order_when_spec_ids_differ_but_counts_match(self):
        specs = ["REQ-1.1-user-registration.spec.ts", "REQ-1.2-user-login.spec.ts", "support/e2e.ts"]
        mapping, aliases = map_specs_to_nodes(specs, ["REQ-1", "REQ-2"])
        self.assertEqual(mapping["REQ-1"], ["REQ-1.1-user-registration.spec.ts"])
        self.assertEqual(mapping["REQ-2"], ["REQ-1.2-user-login.spec.ts"])
        self.assertEqual(aliases, {"REQ-1.1": "REQ-1", "REQ-1.2": "REQ-2"})

    def test_should_fall_back_to_parent_prefix_and_leave_rest_unassigned(self):
        specs = ["REQ-1.1-a.spec.ts", "REQ-1.2-b.spec.ts", "REQ-9.spec.ts"]
        mapping, aliases = map_specs_to_nodes(specs, ["REQ-1", "REQ-2"])
        self.assertEqual(mapping["REQ-1"], ["REQ-1.1-a.spec.ts", "REQ-1.2-b.spec.ts"])
        self.assertEqual(mapping["REQ-2"], [])
        self.assertEqual(mapping[None], ["REQ-9.spec.ts"])
        self.assertEqual(aliases, {"REQ-1.1": "REQ-1", "REQ-1.2": "REQ-1"})

    def test_should_sort_spec_ids_numerically_when_mapping_in_order(self):
        specs = ["REQ-1.10-x.spec.ts", "REQ-1.2-y.spec.ts"]
        mapping, _ = map_specs_to_nodes(specs, ["A", "B"])
        self.assertEqual(mapping["A"], ["REQ-1.2-y.spec.ts"])
        self.assertEqual(mapping["B"], ["REQ-1.10-x.spec.ts"])


def report(*tests):
    specs = []
    for title, status, error, steps, duration in tests:
        result = {"status": status, "duration": duration, "steps": [{"title": s, "category": "pw:api"} for s in steps]}
        if error:
            result["error"] = {"message": error, "location": {"file": "/w/tests/REQ-1.spec.ts", "line": 12}}
            result["errors"] = [result["error"]]
        specs.append({"title": title, "file": "REQ-1.spec.ts", "tests": [{"status": "expected" if status == "passed" else "unexpected", "results": [result]}]})
    return {"suites": [{"title": "REQ-1.spec.ts", "specs": specs}]}


class ReportTests(unittest.TestCase):
    def test_should_count_passed_and_collect_durations(self):
        summary = summarize_report(report(("a", "passed", None, [], 800), ("b", "failed", "boom", [], 10500)))
        self.assertEqual((summary.passed, summary.total), (1, 2))
        self.assertEqual([r.title for r in summary.results if not r.ok], ["b"])
        self.assertEqual(summary.slow(3000), ["b"])

    def test_should_treat_missing_report_as_zero_of_zero(self):
        summary = summarize_report({})
        self.assertEqual((summary.passed, summary.total), (0, 0))

    def test_should_build_four_field_summary_without_ansi_and_with_last_steps(self):
        msg = "\x1b[31mError: expect(locator).toHaveText(expected)\x1b[39m\n\nLocator: getByTestId('count')\nExpected string: \"2\"\nReceived string: \"1\""
        steps = ["page.goto(/)", "locator.click", "locator.click", "expect.toHaveText"]
        summary = summarize_report(report(("REQ-1: increments", "failed", msg, steps, 5000)))
        text = failure_summaries(summary, max_steps=3)
        self.assertIn("Feature: REQ-1: increments", text)
        self.assertIn("Failed at: REQ-1.spec.ts:12", text)
        self.assertIn("Observation: Error: expect(locator).toHaveText(expected)", text)
        self.assertNotIn("\x1b", text)
        self.assertIn("Steps: locator.click -> locator.click -> expect.toHaveText", text)

    def test_should_use_call_log_lines_when_no_step_trace(self):
        msg = "Error: page.goto: net::ERR_CONNECTION_REFUSED\nCall log:\n  - navigating to \"http://x/\", waiting until \"load\"\n\nmore"
        summary = summarize_report(report(("t", "failed", msg, [], 100)))
        self.assertIn('Steps: navigating to "http://x/", waiting until "load"', failure_summaries(summary))

    def test_should_mark_timeouts_as_performance_observations(self):
        summary = summarize_report(report(("slow one", "timedOut", "Test timeout of 10000ms exceeded.", ["page.reload"], 10000)))
        text = failure_summaries(summary)
        self.assertIn("timed out", text.lower())


if __name__ == "__main__":
    unittest.main()


class WorktreeSnapshotTests(unittest.TestCase):
    def test_should_undo_test_run_mutations_but_keep_uncommitted_edits(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            run = lambda args: subprocess.run(["git", *args], cwd=root, check=False, capture_output=True,  # noqa: E731
                                              env={"GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@x", "GIT_COMMITTER_NAME": "t",
                                                   "GIT_COMMITTER_EMAIL": "t@x", "PATH": "/usr/bin:/bin:/opt/homebrew/bin"})
            run(["init", "-q"])
            (root / "backend").mkdir()
            (root / "backend" / "db.json").write_text('{"count": 0}')
            (root / "backend" / "server.js").write_text("v1")
            run(["add", "-A"]); run(["commit", "-qm", "init"])
            (root / "backend" / "server.js").write_text("v2 (repair edit, uncommitted)")
            snapshot_worktree(run)
            # the test run mutates the store and creates a new file
            (root / "backend" / "db.json").write_text('{"count": -1}')
            (root / "backend" / "uploads.json").write_text("[]")
            restore_worktree(run)
            self.assertEqual((root / "backend" / "db.json").read_text(), '{"count": 0}')
            self.assertEqual((root / "backend" / "server.js").read_text(), "v2 (repair edit, uncommitted)")
            self.assertFalse((root / "backend" / "uploads.json").exists())


class FailureGroupingTests(unittest.TestCase):
    def test_should_group_failed_tests_by_owning_node_via_spec_basename(self):
        summary = summarize_report({"suites": [
            {"title": "a", "file": "REQ-1.spec.ts", "specs": [
                {"title": "one", "file": "REQ-1.spec.ts", "tests": [{"status": "unexpected", "results": [{"status": "failed", "duration": 1,
                    "error": {"message": "x", "location": {"file": "/w/tests/REQ-1.spec.ts", "line": 3}}}]}]}]},
            {"title": "b", "file": "sub/REQ-2.spec.ts", "specs": [
                {"title": "two", "file": "sub/REQ-2.spec.ts", "tests": [{"status": "expected", "results": [{"status": "passed", "duration": 1}]}]},
                {"title": "three", "file": "sub/REQ-2.spec.ts", "tests": [{"status": "unexpected", "results": [{"status": "timedOut", "duration": 1}]}]}]},
        ]})
        grouped = nodes_for_failures(summary.results, {"REQ-1": ["REQ-1.spec.ts"], "REQ-2": ["sub/REQ-2.spec.ts"], None: []})
        self.assertEqual({k: [r.title for r in v] for k, v in grouped.items()}, {"REQ-1": ["one"], "REQ-2": ["three"]})


class PrivateInstallTests(unittest.TestCase):
    def test_should_pin_version_from_tests_package_lock_or_fallback(self):
        with tempfile.TemporaryDirectory() as tmp:
            tests = Path(tmp) / "tests"; tests.mkdir()
            self.assertEqual(playwright_version_hint(tests, fallback="1.63.0"), "1.63.0")
            (Path(tmp) / "package-lock.json").write_text(
                '{"packages": {"node_modules/@playwright/test": {"version": "1.55.1"}}}')
            self.assertEqual(playwright_version_hint(tests), "1.55.1")
            (tests / "package.json").write_text('{"devDependencies": {"@playwright/test": "^1.52.0"}}')
            self.assertEqual(playwright_version_hint(tests), "1.52.0")

    def test_should_keep_every_write_inside_the_private_root(self):
        env = isolated_install_env(Path("/private/x"))
        for key in ("npm_config_cache", "NPM_CONFIG_CACHE", "PLAYWRIGHT_BROWSERS_PATH"):
            self.assertTrue(env[key].startswith("/private/x"), key)
        self.assertIn("npmmirror", env["PLAYWRIGHT_DOWNLOAD_HOST"])


class ProtectedTreeTests(unittest.TestCase):
    def test_should_restore_changed_deleted_and_added_files(self):
        import shutil
        with tempfile.TemporaryDirectory() as tmp:
            live = Path(tmp) / "tests"; (live / "support").mkdir(parents=True)
            (live / "REQ-1.spec.ts").write_text("original"); (live / "support" / "e2e.ts").write_text("helper")
            snap = Path(tmp) / "snap"; shutil.copytree(live, snap)
            digest = tree_digest(live)
            (live / "REQ-1.spec.ts").write_text("tampered"); (live / "support" / "e2e.ts").unlink()
            (live / "playwright.config.ts").write_text("injected")
            fixed = restore_tree(live, snap, digest)
            self.assertEqual(sorted(fixed), ["REQ-1.spec.ts", "playwright.config.ts", "support/e2e.ts"])
            self.assertEqual((live / "REQ-1.spec.ts").read_text(), "original")
            self.assertEqual((live / "support" / "e2e.ts").read_text(), "helper")
            self.assertFalse((live / "playwright.config.ts").exists())
            self.assertEqual(tree_digest(live), digest)

    def test_should_report_zero_tests_as_load_error(self):
        summary = summarize_report({"suites": [], "errors": [{"message": "SyntaxError: Unexpected token"}]})
        self.assertEqual(summary.total, 0)
        self.assertEqual(summary.load_errors, ["SyntaxError: Unexpected token"])


class HelperLocationTests(unittest.TestCase):
    def test_should_attribute_failure_raised_in_helper_to_the_spec_file(self):
        rep = {"suites": [{"title": "REQ-2.3.1-x.spec.ts", "file": "REQ-2.3.1-x.spec.ts", "specs": [
            {"title": "REQ-2.3.1: trash view", "file": "REQ-2.3.1-x.spec.ts", "tests": [{"status": "unexpected", "results": [
                {"status": "failed", "duration": 900, "error": {"message": "boom", "location": {"file": "/w/tests/support/e2e.ts", "line": 48}}}]}]}]}]}
        summary = summarize_report(rep)
        grouped = nodes_for_failures(summary.results, {"REQ-2.3.1": ["REQ-2.3.1-x.spec.ts"], None: []})
        self.assertEqual(list(grouped), ["REQ-2.3.1"])
        self.assertIn("Failed at: e2e.ts:48 (called from REQ-2.3.1-x.spec.ts)", failure_summaries(summary))


class MemoryWorkersTests(unittest.TestCase):
    def test_should_scale_workers_to_container_memory(self):
        self.assertEqual(workers_for_memory(None, 4), 4)
        self.assertEqual(workers_for_memory(512 * 1024 * 1024, 4), 1)
        self.assertEqual(workers_for_memory(2 * 1024 * 1024 * 1024, 4), 2)
        self.assertEqual(workers_for_memory(8 * 1024 * 1024 * 1024, 4), 4)


class RobustnessProbeTests(unittest.TestCase):
    def test_should_pass_for_a_server_that_answers_404_and_fail_for_a_dead_port(self):
        import http.server, socket, threading
        class H(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                self.send_response(404); self.end_headers()
            def log_message(self, *a): pass
        srv = http.server.HTTPServer(("127.0.0.1", 0), H); port = srv.server_address[1]
        th = threading.Thread(target=srv.serve_forever, daemon=True); th.start()
        try:
            self.assertIsNone(robustness_probe(port, None, timeout=3))
        finally:
            srv.shutdown()
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0)); free = s.getsockname()[1]
        err = robustness_probe(free, None, timeout=2)
        self.assertIn("no HTTP response", err)


class FinalWorkersAndReapTests(unittest.TestCase):
    def test_should_pick_final_workers_by_450mib_per_worker(self):
        from acceptance import workers_for_final
        self.assertEqual(workers_for_final(2 * 1024**3, 4), 4)
        self.assertEqual(workers_for_final(512 * 1024**2, 4), 1)
        self.assertEqual(workers_for_final(None, 4), 4)

    def test_should_reap_only_node_processes_inside_app_dirs(self):
        import tempfile
        from pathlib import Path
        from acceptance import should_reap
        root = Path(tempfile.mkdtemp())
        (root / "backend").mkdir(); (root / ".octos").mkdir()
        self.assertTrue(should_reap("node", str(root / "backend"), root))
        self.assertTrue(should_reap("/usr/bin/node", str(root / "frontend" / "x"), root))
        self.assertFalse(should_reap("node", str(root / ".octos"), root))
        self.assertFalse(should_reap("node", str(root), root))
        self.assertFalse(should_reap("octos", str(root / "backend"), root))
        self.assertFalse(should_reap("node", None, root))
