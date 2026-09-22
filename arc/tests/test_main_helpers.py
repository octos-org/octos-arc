import os
import unittest

from main import OctosDriver, describe_node, folder_descendants, inline_sources, inline_spec_text, unchanged_node_ids
import main as m


def node(node_id, description, deps=()):
    return {"id": node_id, "type": "ATOMIC", "name": node_id, "description": description,
            "dependencies": list(deps), "scenarios": [{"name": "s", "steps": [{"keyword": "GIVEN", "content": "x"}]}]}


class EvolutionDiffTests(unittest.TestCase):
    def test_should_keep_nodes_whose_content_matches_previous_requirement_table(self):
        current = [node("REQ-1", "same"), node("REQ-2", "changed"), node("REQ-3", "new", ["REQ-1"])]
        previous = {
            "REQ-1": {"req_id": "REQ-1", "id": "REQ-1", "name": "REQ-1", "description": "same", "dependencies": [],
                      "scenarios": [{"name": "s", "steps": [{"keyword": "GIVEN", "content": "x"}]}]},
            "REQ-2": {"req_id": "REQ-2", "id": "REQ-2", "name": "REQ-2", "description": "old", "dependencies": [],
                      "scenarios": [{"name": "s", "steps": [{"keyword": "GIVEN", "content": "x"}]}]},
        }
        self.assertEqual(unchanged_node_ids(current, previous), {"REQ-1"})

    def test_should_treat_everything_as_changed_without_previous_table(self):
        self.assertEqual(unchanged_node_ids([node("REQ-1", "a")], {}), set())


class DescribeNodeTests(unittest.TestCase):
    def test_should_render_scenarios_and_dependencies(self):
        text = describe_node(node("REQ-2", "desc", ["REQ-1"]))
        self.assertIn("ID: REQ-2", text)
        self.assertIn("GIVEN x", text)
        self.assertIn("Depends on: REQ-1", text)


if __name__ == "__main__":
    unittest.main()


class TransientTests(unittest.TestCase):
    def test_should_not_retry_own_turn_timeouts(self):
        self.assertFalse(OctosDriver._transient("octos turn timed out"))
        self.assertFalse(OctosDriver._transient("octos timed out after 900s"))

    def test_should_not_retry_account_errors_or_numbers_in_request_ids(self):
        for text in (
            'HTTP 402 insufficient_balance request id 503429401',
            'HTTP 401 authentication failed',
            'HTTP 403 forbidden: request timed out',
            'provider quota exhausted rate limit',
            'bad input request id 502',
        ):
            self.assertFalse(OctosDriver._transient(text), text)

    def test_should_abort_flow_before_falling_back_to_another_generation(self):
        import argparse
        from pathlib import Path
        from types import SimpleNamespace
        flow = m.Flow(argparse.Namespace(web_port=3000), Path("."), Path("."))
        flow.driver = SimpleNamespace(run=lambda *args: (False, "HTTP 402 insufficient_balance"))
        with self.assertRaises(m.PermanentProviderError):
            flow.turn("implement", 60, "node implement")

    def test_should_retry_provider_errors(self):
        self.assertTrue(OctosDriver._transient("HTTP 503 Service Temporarily Unavailable"))
        self.assertTrue(OctosDriver._transient("failed to send streaming request"))


class FolderDescendantTests(unittest.TestCase):
    def test_should_map_every_folder_to_its_atomic_leaves(self):
        tree = {"id": "ROOT", "type": "FOLDER", "children": [
            {"id": "F-1", "type": "FOLDER", "children": [node("REQ-1", "a"), node("REQ-2", "b")]},
            node("REQ-3", "c")]}
        self.assertEqual(folder_descendants(tree), {"F-1": ["REQ-1", "REQ-2"], "ROOT": ["REQ-1", "REQ-2", "REQ-3"]})


class SetupPlaywrightTests(unittest.TestCase):
    """Regression for cloud run d116ad5e3aa0: the private-install branch of
    setup_playwright must unpack (root, env_extra) and expose cleanup."""

    def test_should_use_private_install_tuple_and_clean_it_up(self):
        import argparse, tempfile
        from pathlib import Path
        import main as m
        with tempfile.TemporaryDirectory() as tmp:
            tests = Path(tmp) / "tests"; tests.mkdir(); (tests / "REQ-1.spec.ts").write_text("x")
            fake_root = Path(tmp) / "pw"; (fake_root / "node_modules" / "@playwright" / "test").mkdir(parents=True)
            flow = m.Flow(argparse.Namespace(web_port=3000), Path(tmp) / "out", Path(tmp) / "req")
            flow.tests_dir = tests
            calls = {}
            def fake_ensure(install_root, log, timeout=540, version="1.63.0"):
                calls["version"] = version
                return fake_root, {"PLAYWRIGHT_BROWSERS_PATH": str(install_root / "browsers")}
            saved = (m.find_playwright_root, m.find_playwright_by_search, m.ensure_playwright)
            m.find_playwright_root = lambda cands: fake_root if cands == [fake_root] else None
            m.find_playwright_by_search = lambda log: None
            m.ensure_playwright = fake_ensure
            try:
                flow.setup_playwright()
            finally:
                m.find_playwright_root, m.find_playwright_by_search, m.ensure_playwright = saved
            self.assertEqual(calls["version"], "1.63.0")
            self.assertIsNotNone(flow.runner)
            self.assertEqual(flow.runner.root, fake_root)
            self.assertIn("PLAYWRIGHT_BROWSERS_PATH", flow.runner.env_extra)
            private = flow.private_playwright
            self.assertTrue(private.exists())
            flow.cleanup_playwright()
            self.assertFalse(private.exists())


class InlineSpecTests(unittest.TestCase):
    def test_should_quote_files_within_budget_and_bail_when_too_big(self):
        import tempfile
        from pathlib import Path
        with tempfile.TemporaryDirectory() as tmp:
            tests = Path(tmp); (tests / "support").mkdir()
            (tests / "REQ-1.spec.ts").write_text("spec body"); (tests / "support" / "e2e.ts").write_text("helper")
            text = inline_spec_text(tests, ["REQ-1.spec.ts", "support/e2e.ts"], 1000)
            self.assertIn("--- REQ-1.spec.ts ---\nspec body", text)
            self.assertIn("--- support/e2e.ts ---\nhelper", text)
            self.assertEqual(inline_spec_text(tests, ["REQ-1.spec.ts", "support/e2e.ts"], 10), "")


class InlineSourcesTests(unittest.TestCase):
    def test_should_quote_small_files_and_omit_those_over_budget(self):
        import tempfile
        from pathlib import Path
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp); (root / "backend").mkdir(); (root / "frontend" / "src").mkdir(parents=True)
            (root / "backend" / "server.js").write_text("x" * 100); (root / "frontend" / "src" / "index.html").write_text("<p>hi</p>")
            (root / "frontend" / "node_modules").mkdir(); (root / "frontend" / "node_modules" / "a.js").write_text("no")
            text = inline_sources(root, max_chars=50)
            self.assertIn("--- frontend/src/index.html ---\n<p>hi</p>", text)
            self.assertIn("backend/server.js --- (omitted, 100 chars", text)
            self.assertNotIn("node_modules", text)


class FailureNormalizationTests(unittest.TestCase):
    def test_should_treat_digests_differing_only_in_numbers_as_identical(self):
        import re
        a = "Observation: TIMED OUT after 4136 ms ... Expected: \"2\" Received: \"\""
        b = "Observation: TIMED OUT after 4144 ms ... Expected: \"2\" Received: \"\""
        self.assertEqual(re.sub(r"\d+", "#", a), re.sub(r"\d+", "#", b))


class CodegenPromptTests(unittest.TestCase):
    def test_should_format_without_placeholder_errors_and_keep_build_command(self):
        import main as m
        text = m.CODEGEN_PROMPT.format(node_id="REQ-1", description="S", spec="T", port=3000, ports=" P", size_rule="R")
        self.assertIn("update manifests when required", text)
        self.assertIn("REQ-1", text)


class AlreadyPassingProbeTests(unittest.TestCase):
    def test_should_mark_only_fully_passing_nodes_as_unchanged(self):
        import argparse
        from pathlib import Path
        from types import SimpleNamespace
        flow = m.Flow(argparse.Namespace(web_port=1), Path("."), Path("."))
        flow.spec_map = {"REQ-1": ["a.spec.ts"], "REQ-2": ["b.spec.ts"], "REQ-3": [], None: []}
        results = {"a.spec.ts": SimpleNamespace(error=None, total=2, passed=2, all_passed=True),
                   "b.spec.ts": SimpleNamespace(error=None, total=2, passed=1, all_passed=False)}
        flow.run_specs = lambda specs, **kw: results[specs[0]]
        self.assertEqual(flow.already_passing_nodes(["REQ-1", "REQ-2", "REQ-3"]), {"REQ-1"})


class CodegenManifestTests(unittest.TestCase):
    def test_should_write_missing_manifests_once(self):
        import json, tempfile
        from pathlib import Path
        root = Path(tempfile.mkdtemp())
        self.assertEqual(m.write_codegen_manifests(root), ["frontend/package.json", "backend/package.json"])
        self.assertEqual(m.write_codegen_manifests(root), [])
        fe = json.loads((root / "frontend/package.json").read_text())
        # the build preserves HTML filenames without creating route aliases
        import shutil, subprocess
        node = shutil.which("node") or "/opt/homebrew/opt/node@24/bin/node"
        (root / "frontend/src").mkdir(parents=True)
        (root / "frontend/src/index.html").write_text("i"); (root / "frontend/src/register.html").write_text("r")
        cmd = fe["scripts"]["build"][len("node -e "):].strip('"').replace('\\"', '"')
        subprocess.run([node, "-e", cmd], cwd=root / "frontend", check=True)
        self.assertFalse((root / "frontend/dist/register").exists())
        self.assertTrue((root / "frontend/dist/register.html").is_file())
        self.assertFalse((root / "frontend/dist/index").exists())
        be = json.loads((root / "backend/package.json").read_text())
        self.assertEqual(be["scripts"]["start"], "node server.js")
        self.assertEqual(be["type"], "commonjs")


class ExtraPortsBoundTests(unittest.TestCase):
    def test_should_report_unbound_spec_ports_in_grader_like_mode(self):
        import tempfile
        from pathlib import Path
        from acceptance import AppServer
        srv = AppServer(Path(tempfile.mkdtemp()), 3100, lambda s: None, grader_like=True, extra_ports=[3301])
        srv.port = 3100
        err = srv.extra_ports_bound(wait_seconds=0.3)
        self.assertIn("3301", err)
        self.assertIn("ERR_CONNECTION_REFUSED", err)
        srv.extra_ports = []
        self.assertIsNone(srv.extra_ports_bound(wait_seconds=0.1))


class SnapshotSourcesTests(unittest.TestCase):
    def test_should_copy_sources_but_not_node_modules(self):
        import argparse, tempfile
        from pathlib import Path
        root = Path(tempfile.mkdtemp())
        (root / "frontend/src").mkdir(parents=True); (root / "backend/node_modules/x").mkdir(parents=True)
        (root / "frontend/src/index.html").write_text("<p>")
        (root / "backend/server.js").write_text("x")
        (root / "backend/node_modules/x/i.js").write_text("y")
        flow = m.Flow(argparse.Namespace(web_port=1), root, root)
        dest = flow.snapshot_sources("REQ-1", 0)
        self.assertTrue((dest / "frontend/src/index.html").is_file())
        self.assertTrue((dest / "backend/server.js").is_file())
        self.assertFalse((dest / "backend/node_modules").exists())


class DiscardTemplateTests(unittest.TestCase):
    def test_should_move_app_dirs_aside_and_clear_has_app(self):
        import argparse, tempfile
        from pathlib import Path
        root = Path(tempfile.mkdtemp())
        (root / "frontend").mkdir(); (root / "backend").mkdir()
        (root / "frontend/package.json").write_text("{}"); (root / "backend/package.json").write_text("{}")
        flow = m.Flow(argparse.Namespace(web_port=1), root, root)
        self.assertTrue(flow.has_app())
        dest = flow.discard_template()
        self.assertFalse(flow.has_app())
        self.assertTrue((dest / "frontend/package.json").is_file())
        self.assertTrue((dest / "backend/package.json").is_file())


class CodegenReasoningTests(unittest.TestCase):
    def test_should_drop_reasoning_for_small_specs_only(self):
        import argparse, os
        from pathlib import Path
        flow = m.Flow(argparse.Namespace(web_port=1), Path("."), Path("."))
        self.assertEqual(flow.codegen_reasoning(1200), "none")
        self.assertIsNone(flow.codegen_reasoning(14000))
        self.assertIsNone(flow.codegen_reasoning(0))
        os.environ["OCTOS_ARC_REASONING"] = "low"
        try:
            self.assertIsNone(flow.codegen_reasoning(1200))
        finally:
            del os.environ["OCTOS_ARC_REASONING"]


class DryRunDriverTests(unittest.TestCase):
    def test_should_return_parseable_file_blocks_for_codegen_prompts(self):
        from codegen import parse_file_blocks
        d = m.DryRunDriver()
        ok, text = d.run("Requirement ...\n<<<FILE relative/path>>>\ncontents\n<<<END FILE>>>", 10)
        self.assertTrue(ok)
        files = parse_file_blocks(text)
        self.assertEqual(sorted(files), ["backend/server.js", "frontend/src/index.html"])
        ok, text = d.run("Implement the node with tools.", 10)
        self.assertTrue(ok); self.assertIn("dry run", text)
        self.assertEqual(d.turns, 2)


class TinyTierTests(unittest.TestCase):
    def test_should_compact_spec_to_its_statements(self):
        spec = """import { test, expect } from '@playwright/test';

test('REQ-1: roll a dice', async ({ page }) => {
  await page.goto('/');
  const roll = page.getByRole('button', { name: 'Roll' });
  await expect(roll).toBeVisible();
});
"""
        out = m.compact_spec_lines(spec)
        self.assertEqual(out.splitlines()[0], "test: REQ-1: roll a dice")
        self.assertIn("page.goto('/');", out)
        self.assertNotIn("await", out); self.assertNotIn("import", out); self.assertNotIn("});", out.splitlines())

    def test_should_gate_tiny_mode_by_spec_size(self):
        import argparse, os
        from pathlib import Path
        flow = m.Flow(argparse.Namespace(web_port=1), Path("."), Path("."))
        self.assertTrue(flow.tiny_mode(600)); self.assertFalse(flow.tiny_mode(1500)); self.assertFalse(flow.tiny_mode(0))
        os.environ["OCTOS_ARC_TINY"] = "0"
        try:
            self.assertFalse(flow.tiny_mode(600))
        finally:
            del os.environ["OCTOS_ARC_TINY"]

    def test_should_pass_unabridged_requirements_and_spec_to_tiny_generation(self):
        import argparse
        import tempfile
        from pathlib import Path
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            flow = m.Flow(argparse.Namespace(web_port=3000), root, root)
            flow.tests_dir = root
            flow.runner = object()
            spec = "test('example', async () => {\n  await check();\n});"
            flow.spec_bodies = lambda _: spec
            captured = []
            flow.codegen_turn = lambda prompt, *args, **kwargs: (captured.append(prompt) or False, "")
            flow.tiny_turn("item", ["example.spec.ts"], 20, {"description": "Allow arbitrary search terms and style the result list"})
            self.assertIn("Allow arbitrary search terms", captured[0])
            self.assertIn(spec, captured[0])

    def test_tiny_server_should_format_and_parse(self):
        import shutil, subprocess, tempfile
        from pathlib import Path
        js = m.TINY_SERVER_JS.format(port=3000, extra_ports="[3301]")
        self.assertIn("listen(process.env.PORT || 3000)", js); self.assertIn("[3301]", js)
        node = shutil.which("node")
        if node:
            p = Path(tempfile.mkdtemp()) / "server.js"; p.write_text(js)
            self.assertEqual(subprocess.run([node, "--check", str(p)], capture_output=True).returncode, 0)

    def test_should_strip_code_fences(self):
        self.assertEqual(m.strip_code_fences("```html\n<html></html>\n```"), "<html></html>")
        self.assertEqual(m.strip_code_fences("<html></html>"), "<html></html>")


class ProbeTests(unittest.TestCase):
    def test_minimal_probe_body_disables_thinking_and_caps_output(self):
        import json
        body = json.loads(m.minimal_probe_body("deepseek-v4-flash"))
        self.assertEqual(body["max_tokens"], 1)
        self.assertEqual(body["thinking"], {"type": "disabled"})
        self.assertNotIn("reasoning_effort", body)

    def test_any_non_5xx_means_endpoint_up(self):
        for code in (200, 204, 401, 403, 404, 405, 429):
            self.assertTrue(m.endpoint_is_up(code))
        for code in (500, 502, 503, 504):
            self.assertFalse(m.endpoint_is_up(code))
class CostGuardTests(unittest.TestCase):
    def test_should_wind_down_on_token_or_turn_limit(self):
        import argparse, os
        from pathlib import Path
        from types import SimpleNamespace
        os.environ["OCTOS_ARC_MAX_TOTAL_TOKENS"] = "1000"; os.environ["OCTOS_ARC_MAX_TURNS"] = "3"
        try:
            flow = m.Flow(argparse.Namespace(web_port=1), Path("."), Path("."))
        finally:
            del os.environ["OCTOS_ARC_MAX_TOTAL_TOKENS"]; del os.environ["OCTOS_ARC_MAX_TURNS"]
        flow.llm_proxy = SimpleNamespace(total_tokens=999)
        self.assertFalse(flow.wound_down())
        flow.llm_proxy.total_tokens = 1000
        self.assertTrue(flow.wound_down())
        flow.llm_proxy.total_tokens = 0; flow.turn_count = 3
        self.assertTrue(flow.wound_down())

    def test_should_stay_unset_until_the_tree_is_known_and_never_trip_a_normal_run(self):
        import argparse
        from pathlib import Path
        from types import SimpleNamespace
        flow = m.Flow(argparse.Namespace(web_port=1), Path("."), Path("."))
        self.assertEqual((flow.max_total_tokens, flow.max_turns), (-1, -1))
        flow.llm_proxy = SimpleNamespace(total_tokens=10**9); flow.turn_count = 10**6
        self.assertFalse(flow.wound_down())  # -1 = not derived yet -> inactive
        # keep-sized tree: calibrated run (26M tokens, 35 turns) is far below the derived limits
        flow.max_total_tokens = max(6_000_000, 2_500_000 * 32); flow.max_turns = max(24, 4 * 32)
        flow.llm_proxy.total_tokens = 26_000_000; flow.turn_count = 35
        self.assertFalse(flow.wound_down())
        flow.llm_proxy.total_tokens = 80_000_000
        self.assertTrue(flow.wound_down())

    def test_should_honor_absolute_ceiling(self):
        import argparse
        from pathlib import Path
        from types import SimpleNamespace
        flow = m.Flow(argparse.Namespace(web_port=1), Path("."), Path("."))
        flow.max_total_tokens, flow.max_turns, flow.max_total_tokens_abs = 0, 0, 75_000_000
        flow.llm_proxy = SimpleNamespace(total_tokens=74_999_999); flow.turn_count = 999
        self.assertFalse(flow.wound_down())
        flow.llm_proxy.total_tokens = 75_000_000
        self.assertTrue(flow.wound_down())


class ProbePolicyTests(unittest.TestCase):
    def test_all_specs_tiny_requires_every_spec_node_small(self):
        import argparse, tempfile
        from pathlib import Path
        root = Path(tempfile.mkdtemp())
        (root / "a.spec.ts").write_text("x" * 400); (root / "b.spec.ts").write_text("y" * 4000)
        flow = m.Flow(argparse.Namespace(web_port=1), root, root)
        flow.tests_dir = root
        flow.spec_map = {"REQ-1": ["a.spec.ts"], "REQ-2": ["b.spec.ts"], "REQ-3": [], None: []}
        self.assertFalse(flow.all_specs_tiny(["REQ-1", "REQ-2", "REQ-3"]))
        self.assertTrue(flow.all_specs_tiny(["REQ-1", "REQ-3"]))
        self.assertFalse(flow.all_specs_tiny(["REQ-3"]))

    def test_looks_like_markup_accepts_fragments(self):
        self.assertTrue(m.looks_like_markup('<div data-testid="count">0</div><button>Increment</button><script>1</script>'))
        self.assertTrue(m.looks_like_markup("<!DOCTYPE html><html></html>"))
        self.assertFalse(m.looks_like_markup("dry run: no model call; nothing written."))


class RelevantSourcesTests(unittest.TestCase):
    def test_should_quote_backend_first_then_pages_by_spec_overlap_within_budget(self):
        import tempfile
        from pathlib import Path
        root = Path(tempfile.mkdtemp())
        (root / "frontend/src").mkdir(parents=True); (root / "backend").mkdir()
        (root / "backend/server.js").write_text("const http = require('http'); // router")
        (root / "frontend/src/index.html").write_text("<a href='/notes'>Notes</a>" + "x" * 300)
        (root / "frontend/src/notes.html").write_text("<h1>Notes</h1><button>New note</button><ul data-testid='note-list'></ul>" + "y" * 300)
        (root / "frontend/src/settings.html").write_text("<h1>Settings</h1>" + "z" * 300)
        spec = "await page.goto('/notes'); await page.getByRole('button', { name: 'New note' }).click(); await expect(page.getByTestId('note-list')).toBeVisible();"
        out = m.relevant_sources(root, spec, max_chars=800)
        self.assertLess(out.index("backend/server.js"), out.index("frontend/src/notes.html"))
        self.assertIn("--- frontend/src/notes.html ---", out)
        self.assertIn("settings.html", out)  # listed as omitted
        self.assertNotIn("--- frontend/src/settings.html ---", out)

    def test_should_include_json_without_displacing_code_or_overflowing_first_file(self):
        import tempfile
        from pathlib import Path
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            (root / 'backend/data').mkdir(parents=True)
            (root / 'frontend').mkdir()
            (root / 'backend/server.js').write_text('server')
            (root / 'frontend/index.html').write_text('page')
            (root / 'backend/data/state.json').write_text('{"count":17}')
            full = m.relevant_sources(root, 'count', 100)
            self.assertIn('--- backend/data/state.json ---', full)
            self.assertIn('"count":17', full)
            tight = m.relevant_sources(root, 'count', 10)
            self.assertIn('--- backend/server.js ---', tight)
            self.assertIn('--- frontend/index.html ---', tight)
            self.assertNotIn('--- backend/data/state.json ---', tight)
            (root / 'backend/server.js').write_text('x' * 101)
            tight = m.relevant_sources(root, 'count', 100)
            self.assertNotIn('--- backend/server.js ---', tight)
            self.assertIn('--- backend/data/state.json ---', tight)

    def test_codegen_requires_existing_sources_to_fit(self):
        """The gate itself, pinned against its own budget rather than the default."""
        from pathlib import Path
        from unittest.mock import patch
        import argparse, tempfile
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            flow = m.Flow(argparse.Namespace(web_port=1), root, root)
            with patch.dict("os.environ", {"OCTOS_ARC_CODEGEN_SOURCE_FIT_CHARS": "90000"}):
                self.assertTrue(flow.codegen_context_fits("x" * 12000))
                (root / "backend").mkdir()
                (root / "frontend").mkdir()
                (root / "backend/server.js").write_text("b" * 26000)
                (root / "frontend/index.html").write_text("p" * 55000)
                self.assertFalse(flow.codegen_context_fits("x" * 12000))
                (root / "frontend/index.html").write_text("p" * 50000)
                self.assertTrue(flow.codegen_context_fits("x" * 12000))

    def test_an_app_past_the_budget_falls_back_to_tool_mode(self):
        """An app whose sources no longer fit must NOT take the single-request path.

        This gate was briefly raised to 400000 to escape tool mode (it is 65% of
        stackoverflow 97848d542ac8's nodes and 90% of its wall clock, 19.6x the median per
        node). The raise was submitted as 95da0dc11e93 and reverted on 2026-09-18, because
        it bought that speed with correctness:

            run                        path            result   regressed  never-passed
            stackoverflow 97848d542ac8 65% tool mode   66/66    0          0
            stackoverflow 34ca94da0075 100% codegen    mid-run  9          7
            12306         99196f2e802b 100% codegen    mid-run  29         0

        The tool-mode run finished with zero regressions; the codegen-only runs regressed
        9 and 29 nodes, and 12306's losses were entirely regressions -- not one node it
        could not build, only nodes it built and then broke. A node seeing part of a shared
        file and asked to return it complete drops the handlers it never saw, and those
        belong to other nodes' specs. So an app past the budget has to use tool mode, and
        this test pins that.
        """
        from pathlib import Path
        import argparse, tempfile
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            flow = m.Flow(argparse.Namespace(web_port=1), root, root)
            (root / "backend").mkdir(); (root / "frontend").mkdir()
            (root / "backend/server.js").write_text("b" * 26000)
            (root / "frontend/index.html").write_text("p" * 55000)   # 81000 total
            self.assertEqual(flow.codegen_source_fit_chars(), flow.codegen_context_chars())
            self.assertFalse(flow.codegen_context_fits("x" * 12000))
            # A small app still takes the fast path -- the revert is not "always tool mode".
            (root / "frontend/index.html").write_text("p" * 200)
            self.assertTrue(flow.codegen_context_fits("x" * 1000))

    def test_the_fit_gate_stays_overridable(self):
        """The knob survives the revert: a gate sized by the files a node will actually
        rewrite (rather than the whole app) should recover most of the speed without
        breaking the invariant, and that experiment needs to be runnable."""
        from pathlib import Path
        import argparse, os
        flow = m.Flow(argparse.Namespace(web_port=1), Path("."), Path("."))
        os.environ["OCTOS_ARC_CODEGEN_SOURCE_FIT_CHARS"] = "400000"
        try:
            self.assertEqual(flow.codegen_source_fit_chars(), 400000)
        finally:
            del os.environ["OCTOS_ARC_CODEGEN_SOURCE_FIT_CHARS"]

    def test_an_oversized_spec_still_falls_back(self):
        """The spec-size half of the gate is unchanged: a spec at 60% of the output
        budget cannot be answered as one complete-file reply whatever the source budget."""
        from pathlib import Path
        import argparse
        flow = m.Flow(argparse.Namespace(web_port=1), Path("."), Path("."))
        self.assertFalse(flow.codegen_context_fits("x" * int(flow.codegen_context_chars() * 0.6)))

    def test_codegen_applies_to_big_trees_unless_capped(self):
        import argparse, os
        from pathlib import Path
        from types import SimpleNamespace
        flow = m.Flow(argparse.Namespace(web_port=1), Path("."), Path("."))
        flow.llm_proxy = SimpleNamespace(); flow.n_nodes = 32
        self.assertTrue(flow.codegen_mode())
        os.environ["OCTOS_ARC_CODEGEN_MAX_NODES"] = "2"
        try:
            self.assertFalse(flow.codegen_mode())
        finally:
            del os.environ["OCTOS_ARC_CODEGEN_MAX_NODES"]
        self.assertTrue(flow.codegen_context_fits("x" * 20000)); self.assertFalse(flow.codegen_context_fits("x" * 60000))


class ProtectedSpecRestoreTests(unittest.TestCase):
    """The platform grades with the official spec files, so an edit the model
    sneaks past the hook has to be undone before the next acceptance run --
    otherwise the harness measures a suite the grader will never run. The
    behaviour existed; nothing failed when it was removed."""

    def _flow(self):
        import argparse, tempfile
        from pathlib import Path
        root = Path(tempfile.mkdtemp())
        tests = root / "tests"; reqs = root / "requirements"
        tests.mkdir(); reqs.mkdir()
        (tests / "REQ-1.spec.ts").write_text("expect(page).toHaveText('real requirement');\n")
        (reqs / "requirements.md").write_text("the real requirement\n")
        flow = m.Flow(argparse.Namespace(web_port=1), root, reqs)
        flow.tests_dir = tests
        flow.snapshot_protected()
        return flow, tests, reqs

    def test_should_undo_an_edit_to_an_official_spec(self):
        flow, tests, _ = self._flow()
        spec = tests / "REQ-1.spec.ts"
        spec.write_text("expect(true).toBe(true);\n")          # model weakens the test
        fixed = flow.restore_protected()
        self.assertIn("REQ-1.spec.ts", " ".join(fixed))
        self.assertIn("real requirement", spec.read_text())     # ground truth is back

    def test_should_delete_a_spec_the_model_added(self):
        flow, tests, _ = self._flow()
        (tests / "REQ-EXTRA.spec.ts").write_text("test('freebie', () => {});\n")
        flow.restore_protected()
        self.assertFalse((tests / "REQ-EXTRA.spec.ts").exists())

    def test_should_restore_a_spec_the_model_deleted(self):
        flow, tests, _ = self._flow()
        (tests / "REQ-1.spec.ts").unlink()
        flow.restore_protected()
        self.assertTrue((tests / "REQ-1.spec.ts").exists())
        self.assertIn("real requirement", (tests / "REQ-1.spec.ts").read_text())

    def test_should_protect_the_requirements_too(self):
        flow, _, reqs = self._flow()
        (reqs / "requirements.md").write_text("whatever I feel like\n")
        flow.restore_protected()
        self.assertIn("the real requirement", (reqs / "requirements.md").read_text())

    def test_should_leave_an_untouched_tree_alone(self):
        flow, _, _ = self._flow()
        self.assertEqual(flow.restore_protected(), [])


class FinalSuiteBestRoundTests(unittest.TestCase):
    """L17 port: the final suite delivers the best round. Simulated with stubbed test runs."""
    def _flow(self, rounds_results):
        import argparse, tempfile
        from pathlib import Path
        from types import SimpleNamespace
        from acceptance import RunSummary, TestOutcome
        root = Path(tempfile.mkdtemp()); (root / "t").mkdir()
        for n in ("REQ-1", "REQ-2"):
            (root / "t" / f"{n}.spec.ts").write_text("x")
        flow = m.Flow(argparse.Namespace(web_port=1), root, root)
        flow.tests_dir = root / "t"; flow.spec_map = {"REQ-1": ["REQ-1.spec.ts"], "REQ-2": ["REQ-2.spec.ts"], None: []}
        flow.runner = SimpleNamespace(root=root, work_dir=root / "prepared"); flow.test_verdict = {"REQ-1": False}
        flow.heads = iter(["sha0", "sha1", "sha2"]); flow.restored = []; flow.commits = []
        it = iter(rounds_results)
        def run_specs(specs, workers=None, grader_like=False):
            passed = next(it)
            results = [TestOutcome(title=f"{n} t", ok=i < passed, status="passed" if i < passed else "failed", duration_ms=1,
                                   file=f"{n}.spec.ts") for i, n in enumerate(["REQ-1", "REQ-2"])]
            return RunSummary(passed=passed, total=2, results=results)
        flow.run_specs = run_specs
        flow.head = lambda: getattr(flow, "_head", "sha0")
        flow.commit = lambda msg: (flow.commits.append(msg), setattr(flow, "_head", f"sha{len(flow.commits)}"))[1] or True
        flow.restore_app = lambda sha: flow.restored.append(sha)
        flow.turn = lambda *a, **k: (True, "repaired")
        flow.record_tests = lambda *a, **k: None
        flow.remaining = lambda: 10_000
        flow.wound_down = lambda: False
        flow.sources_text = lambda: ""; flow.corrections_text = lambda: ""
        return flow

    def test_should_keep_original_requirements_in_full_suite_repairs(self):
        from unittest.mock import Mock, patch
        flow = self._flow([1, 2])
        flow.requirement_nodes = {
            'REQ-1': {'id':'REQ-1', 'description':'Preserve initial state beyond visible assertions'},
            'REQ-2': {'id':'REQ-2', 'description':'Save the full text without truncation'}}
        flow.turn = Mock(return_value=(True, 'repaired'))
        with patch.dict('os.environ', {'OCTOS_FINAL_REPAIR_ROUNDS':'1'}):
            flow.final_acceptance()
        prompt = flow.turn.call_args.args[0]
        self.assertIn('Preserve initial state beyond visible assertions', prompt)
        self.assertIn('Save the full text without truncation', prompt)

    def test_should_restore_best_state_after_regressing_repairs(self):
        import os
        os.environ["OCTOS_FINAL_REPAIR_ROUNDS"] = "2"
        try:
            flow = self._flow([1, 0, 0])
            flow.final_acceptance()
        finally:
            del os.environ["OCTOS_FINAL_REPAIR_ROUNDS"]
        # round 0 (1/2) is best at sha0; repairs regress to 0/2 twice (identical failures stop) -> restore sha0
        self.assertEqual(flow.restored, ["sha0"])
        self.assertTrue(flow.test_verdict["REQ-1"]); self.assertFalse(flow.test_verdict["REQ-2"])

    def test_should_never_deliver_a_later_pass_that_is_worse(self):
        """`final_acceptance_passes` says a repeat starts from a state at least as
        good as the one before it. Cloud 6e82a7ff571c bears it out -- pass 1 ended
        29/32 and pass 2 opened 29/32 -- but nothing pinned it. If the restore at
        the end of a pass ever went away, a later pass could hand back worse code
        than an earlier one already had."""
        import argparse, os, tempfile
        from pathlib import Path
        from types import SimpleNamespace
        from unittest.mock import patch
        from acceptance import RunSummary, TestOutcome
        root = Path(tempfile.mkdtemp()); (root / "t").mkdir()
        names = ["REQ-1", "REQ-2", "REQ-3"]
        for n in names:
            (root / "t" / f"{n}.spec.ts").write_text("x")
        flow = m.Flow(argparse.Namespace(web_port=1), root, root)
        flow.tests_dir = root / "t"
        flow.spec_map = {n: [f"{n}.spec.ts"] for n in names}; flow.spec_map[None] = []
        flow.runner = SimpleNamespace(root=root, work_dir=root / "w", timeout_ms=10000)
        flow.mem_limit = 2 * 1024 ** 3
        flow.test_verdict = {n: False for n in names}
        flow.requirement_nodes = {}; flow.pending_corrections = []
        flow.min_repair_seconds = 10
        flow.remaining = lambda: 100_000; flow.time_up = lambda: False
        flow.wound_down = lambda: False
        flow.sources_text = lambda: ""; flow.corrections_text = lambda: ""
        flow.last_repair_diff = lambda *a, **k: ""
        flow.driver = None
        heads = iter([f"sha{i}" for i in range(50)])
        flow.head = lambda: next(heads)
        flow.commit = lambda msg: True
        flow.turn = lambda *a, **k: (True, "done")
        flow.record_tests = lambda *a, **k: None
        recorded = []
        flow.record_full_suite = lambda summary, grouped: recorded.append(summary.passed)
        flow.restore_app = lambda sha: None
        # pass 1 peaks at 2/3; pass 2 only ever measures worse than that peak
        scores = iter([1, 2, 1, 1, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1])
        def run_specs(specs, workers=None, grader_like=False):
            k = next(scores, 1)
            rows = [TestOutcome(title=n, ok=(i < k), status="passed" if i < k else "timedOut",
                                duration_ms=1, file=f"{n}.spec.ts", message="boom")
                    for i, n in enumerate(names)]
            su = RunSummary(passed=k, total=3, results=rows); su.stores_written = []
            return su
        flow.run_specs = run_specs
        with patch.dict(os.environ, {"OCTOS_FINAL_SUITE_PASSES": "2", "OCTOS_FINAL_REPAIR_ROUNDS": "2"}):
            flow.final_acceptance_passes()
        self.assertTrue(recorded, "no full-suite round was recorded")
        self.assertEqual(recorded[-1], max(recorded),
                         f"delivered {recorded[-1]} after having reached {max(recorded)}: {recorded}")

    def test_should_not_restore_on_a_single_round_behind_the_best(self):
        """One round behind can be a flaky spec. The node loop waits for two and
        so does this one; restoring on every dip would chase a lucky round."""
        import os
        os.environ["OCTOS_FINAL_REPAIR_ROUNDS"] = "2"
        try:
            flow = self._flow([1, 0, 2])      # dip once, then beat the best
            flow.final_acceptance()
        finally:
            del os.environ["OCTOS_FINAL_REPAIR_ROUNDS"]
        self.assertEqual(flow.restored, [])   # the dip alone must not roll back

    def test_should_roll_back_without_losing_the_flakiness_evidence(self):
        """The first attempt at this rolled back AND rebuilt the evidence from the
        best round, where the flaky specs were passing, so `intermittent_note`
        stopped naming them. Rolling back must leave the evidence alone."""
        import argparse, tempfile
        from pathlib import Path
        from types import SimpleNamespace
        from unittest.mock import patch
        from acceptance import RunSummary, TestOutcome
        root = Path(tempfile.mkdtemp()); (root / "t").mkdir()
        names = ["REQ-1", "REQ-2", "REQ-3"]
        for n in names:
            (root / "t" / f"{n}.spec.ts").write_text("x")
        flow = m.Flow(argparse.Namespace(web_port=1), root, root)
        flow.tests_dir = root / "t"
        flow.spec_map = {n: [f"{n}.spec.ts"] for n in names}; flow.spec_map[None] = []
        flow.runner = SimpleNamespace(root=root, work_dir=root / "w", timeout_ms=10000)
        flow.mem_limit = 2 * 1024 ** 3
        flow.test_verdict = {n: True for n in names}
        flow.requirement_nodes = {}; flow.pending_corrections = []
        flow.restored = []
        flow.restore_app = lambda sha: flow.restored.append(sha)
        flow.head = lambda: "shaBEST"; flow.commit = lambda msg: True
        flow.record_tests = lambda *a, **k: None; flow.record_full_suite = lambda *a, **k: None
        flow.remaining = lambda: 10_000; flow.wound_down = lambda: False
        flow.sources_text = lambda: ""; flow.corrections_text = lambda: ""
        flow.last_repair_diff = lambda *a, **k: ""
        tree = '- generic [ref=e1]:\n  - button "Pin"'
        seq = iter([[True, False, False], [False, False, False], [False, False, False]])
        def run_specs(specs, workers=None, grader_like=False):
            oks = next(seq)
            rows = [TestOutcome(title=n, ok=ok, status="passed" if ok else "timedOut", duration_ms=1,
                                file=f"{n}.spec.ts", message="boom", rendered_page="" if ok else tree)
                    for n, ok in zip(names, oks)]
            s = RunSummary(passed=sum(oks), total=3, results=rows); s.stores_written = []
            return s
        flow.run_specs = run_specs
        prompts = []
        flow.turn = lambda p, t, l, **k: (prompts.append(p), (True, "done"))[1]
        with patch.dict("os.environ", {"OCTOS_FINAL_REPAIR_ROUNDS": "2"}):
            flow.final_acceptance()
        self.assertEqual(flow.restored, ["shaBEST"])
        self.assertIn("already passed in an earlier round", prompts[-1])   # REQ-1 flaked
        self.assertIn("Page at failure", prompts[-1])

    def test_should_tell_the_model_when_it_rolled_the_app_back(self):
        # Cloud 6e82a7ff571c: 27/32, repair cut at the 1200s timeout, 17/32 next.
        # Silently swapping the code under the model leaves it repairing a tree
        # it has not been told about.
        import os
        os.environ["OCTOS_FINAL_REPAIR_ROUNDS"] = "2"
        try:
            flow = self._flow([1, 0, 0])
            flow.final_acceptance()
        finally:
            del os.environ["OCTOS_FINAL_REPAIR_ROUNDS"]
        self.assertEqual(flow.restored, ["sha0"])
        self.assertTrue(any("restored frontend/ and backend/ to the best state" in c
                            for c in flow.pending_corrections))

    def test_should_continue_when_same_test_reaches_a_new_failed_operation(self):
        from unittest.mock import patch
        flow = self._flow([1, 1, 2])
        original = flow.run_specs
        observations = iter(["waiting for button", "waiting for dialog", ""])
        calls = []
        def run_specs(*args, **kwargs):
            summary = original(*args, **kwargs)
            message = next(observations)
            for result in summary.results:
                if not result.ok:
                    result.message = message
            calls.append(message)
            return summary
        flow.run_specs = run_specs
        with patch.dict("os.environ", {"OCTOS_FINAL_REPAIR_ROUNDS": "2"}):
            flow.final_acceptance()
        self.assertEqual(len(calls), 3)
        self.assertTrue(all(flow.test_verdict.values()))

    def test_should_change_approach_once_before_giving_up_on_identical_rounds(self):
        from unittest.mock import patch
        # Cloud 3ffe9702bf15 stalled here: two identical rounds ended the run with
        # most of its budget unspent. One repeat means the repair missed the cause.
        flow = self._flow([1, 1, 2])
        flow.pending_corrections = []
        with patch.dict("os.environ", {"OCTOS_FINAL_REPAIR_ROUNDS": "3"}):
            flow.final_acceptance()
        self.assertTrue(any('Repeated attempts produced the same observed failure' in c
                            for c in flow.pending_corrections))
        self.assertTrue(all(flow.test_verdict.values()))  # the changed approach fixed it

    def test_should_stop_when_a_changed_approach_still_reproduces_the_failure(self):
        from unittest.mock import patch
        flow = self._flow([1, 1, 1, 2])  # a fourth round exists but must not run
        flow.pending_corrections = []
        calls = []
        original = flow.run_specs
        flow.run_specs = lambda *a, **k: (calls.append(1), original(*a, **k))[1]
        with patch.dict("os.environ", {"OCTOS_FINAL_REPAIR_ROUNDS": "3"}):
            flow.final_acceptance()
        self.assertEqual(len(calls), 3)

    def test_should_repeat_the_full_suite_pass_while_budget_remains(self):
        from unittest.mock import patch
        flow = self._flow([1])
        flow.pending_corrections = []
        flow.min_repair_seconds = 300
        flow.driver = None
        flow.time_up = lambda: False
        calls = []
        flow.final_acceptance = lambda: calls.append(dict(flow.test_verdict))
        with patch.dict("os.environ", {"OCTOS_FINAL_SUITE_PASSES": "3"}):
            flow.final_acceptance_passes()
        self.assertEqual(len(calls), 3)  # REQ-1 stays False, so every pass runs

    def test_should_stop_repeating_once_the_full_suite_is_green(self):
        from unittest.mock import patch
        flow = self._flow([1])
        flow.pending_corrections = []
        flow.min_repair_seconds = 300
        flow.driver = None
        flow.time_up = lambda: False
        calls = []
        def pass_everything():
            calls.append(1)
            flow.test_verdict = {"REQ-1": True, "REQ-2": True}
        flow.final_acceptance = pass_everything
        with patch.dict("os.environ", {"OCTOS_FINAL_SUITE_PASSES": "3"}):
            flow.final_acceptance_passes()
        self.assertEqual(len(calls), 1)

    def test_should_not_start_a_pass_it_cannot_finish(self):
        from unittest.mock import patch
        flow = self._flow([1])
        flow.min_repair_seconds = 300
        flow.driver = None
        flow.time_up = lambda: False
        flow.remaining = lambda: 600  # under the 900 s a pass needs
        calls = []
        flow.final_acceptance = lambda: calls.append(1)
        with patch.dict("os.environ", {"OCTOS_FINAL_SUITE_PASSES": "3"}):
            flow.final_acceptance_passes()
        self.assertEqual(calls, [])

    def test_should_not_restore_when_last_round_is_best(self):
        import os
        os.environ["OCTOS_FINAL_REPAIR_ROUNDS"] = "1"
        try:
            flow = self._flow([0, 1])
            flow.final_acceptance()
        finally:
            del os.environ["OCTOS_FINAL_REPAIR_ROUNDS"]
        self.assertEqual(flow.restored, [])
        self.assertTrue(flow.test_verdict["REQ-1"])

class ProbeInvalidationTests(unittest.TestCase):
    def test_failed_model_turn_discards_pre_generation_verdicts(self):
        from unittest.mock import Mock
        from acceptance import RunSummary
        from pathlib import Path
        flow = object.__new__(m.Flow)
        flow.probe_summaries = {'unchanged': RunSummary(passed=1, total=1)}
        flow.protected_prefixes = lambda: []
        flow.output_dir = Path('/tmp/unused-application')
        flow.turn_count = 0
        flow.guard_enabled = False
        flow.restore_protected = lambda: []
        flow.driver = Mock()
        flow.driver.run.return_value = (False, 'partial implementation failed')
        flow.turn('modify shared component', 60, 'changed implement', expect_verification=False)
        self.assertEqual(flow.probe_summaries, {})


class SmokeShellContractTests(unittest.TestCase):
    @unittest.skipUnless(__import__("os").name == "posix", "POSIX shell contract")
    def test_should_return_with_live_server_without_inheriting_capture_pipes(self):
        import os
        import re
        import signal
        import subprocess
        import tempfile
        from pathlib import Path

        text = m.PORT_RULES.format(smoke=43219, port=3000)
        command = re.search(r"```sh\n(.*?)\n```", text, re.S)
        self.assertIsNotNone(command, "Supply an executable background-server example")
        self.assertEqual(m.PORT_RULES, (Path(m.__file__).parent / "prompts/port-rules.md").read_text())
        with tempfile.TemporaryDirectory(prefix="smoke shell '") as directory:
            root = Path(directory)
            (root / "backend").mkdir()
            bin_dir = root / "bin"
            bin_dir.mkdir()
            npm = bin_dir / "npm"
            # A genuinely long-running child, without a timed sleep or network dependency.
            npm.write_text("#!/bin/sh\nexec python3 -c 'import signal; signal.pause()'\n")
            npm.chmod(0o755)
            process = subprocess.Popen(["/bin/sh", "-c", command.group(1)], cwd=root,
                                       env={**os.environ, "PATH": str(bin_dir) + os.pathsep + os.environ["PATH"]},
                                       stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
            try:
                stdout, stderr = process.communicate(timeout=3)
                self.assertEqual(process.returncode, 0, stderr.decode())
                server_pid = int(stdout.strip())
                os.kill(server_pid, 0)
            finally:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.communicate()


class PostflightOwnershipTests(unittest.TestCase):
    def test_should_only_signal_descendants_or_processes_inside_this_workspace(self):
        from pathlib import Path
        from types import SimpleNamespace
        from unittest.mock import patch
        root = Path('/private/tmp/sweep-app')
        ps = """PID PPID RSS ELAPSED ARGS
90 1 100 00:01 node platform-runner.js
100 90 100 00:01 python main.py
110 100 100 00:01 sh wrapper
120 110 100 00:01 node server.js
130 120 100 00:01 chromium --headless
200 1 100 00:01 node foreign.js /private/tmp/sweep-app/input.txt
300 1 100 00:01 node server.js
400 1 100 00:01 node server.js
"""
        cwds = {200: '/elsewhere', 300: str(root/'backend'), 400: str(root)+'-other'}
        with patch.object(m.os, 'getpid', return_value=100), \
             patch.object(m.os, 'getppid', return_value=90), \
             patch.object(m.subprocess, 'run', return_value=SimpleNamespace(stdout=ps)), \
             patch.object(m, 'process_cwd', side_effect=lambda pid: cwds.get(pid), create=True), \
             patch.object(m.os, 'kill') as kill, \
             patch.object(m.time, 'sleep'), patch.object(m, 'log'):
            m._reap_stray_processes('test', root)
        self.assertEqual({call.args[0] for call in kill.call_args_list}, {120, 130, 300})

    def test_should_leave_foreign_listener_during_generation(self):
        from pathlib import Path
        from types import SimpleNamespace
        from unittest.mock import Mock, patch
        stop = Mock()
        stop.is_set.side_effect = [False, True]
        with patch.object(m.subprocess, 'run', return_value=SimpleNamespace(stdout='123\n')), \
             patch.object(m.os, 'readlink', return_value='/private/tmp/app-other/backend'), \
             patch.object(m.os, 'kill') as kill, patch.object(m, 'log'):
            m._port_watchdog(3000, Path('/private/tmp/app'), stop)
        kill.assert_not_called()


class RuntimeCacheProvenanceTests(unittest.TestCase):
    def test_should_reuse_only_a_cache_from_the_requested_url(self):
        from pathlib import Path
        from tempfile import TemporaryDirectory
        from unittest.mock import patch
        with TemporaryDirectory() as folder:
            cache = Path(folder)
            (cache/'octos').write_text('old executable')
            url = 'https://example.invalid/runtime/new.tar.gz'
            for marker, expected in [(None, 'downloaded'), ('old-url', 'downloaded'), (url, str(cache/'octos'))]:
                provenance = cache/'source-url.txt'
                if marker is not None:
                    provenance.write_text(marker)
                elif provenance.exists():
                    provenance.unlink()
                with patch.dict(m.os.environ, {'OCTOS_CACHE_DIR': folder, 'OCTOS_RELEASE_URL': url}, clear=True), \
                     patch.object(m, 'BUNDLE_DIR', cache/'bundle'), \
                     patch.object(m.shutil, 'which', return_value=None), \
                     patch.object(m, '_download_octos', return_value='downloaded') as download:
                    self.assertEqual(m.find_octos(), expected)
                    self.assertEqual(download.call_count, int(expected=='downloaded'))

    def test_should_replace_a_stale_archive_before_recording_its_new_source(self):
        import io, tarfile
        from pathlib import Path
        from tempfile import TemporaryDirectory
        from unittest.mock import patch
        with TemporaryDirectory() as folder:
            cache = Path(folder)
            def archive(content):
                with tarfile.open(cache/'octos-bundle.tar.gz', 'w:gz') as tar:
                    member=tarfile.TarInfo('octos');member.size=len(content)
                    tar.addfile(member, io.BytesIO(content))
            archive(b'old version')
            (cache/'source-url.txt').write_text('old-url')
            url='https://example.invalid/runtime/new.tar.gz'
            with patch.dict(m.os.environ, {'OCTOS_RELEASE_URL': url}), \
                 patch.object(m.shutil, 'which', return_value='/usr/bin/curl'), \
                 patch.object(m.subprocess, 'run', side_effect=lambda *a, **kw: archive(b'new version')) as download, \
                 patch.object(m, 'log'):
                binary=m._download_octos(cache)
            self.assertEqual(Path(binary).read_bytes(), b'new version')
            self.assertEqual(download.call_count, 1)
            self.assertEqual((cache/'source-url.txt').read_text(), url)

class FailedGenerationAcceptanceTests(unittest.TestCase):
    def test_should_verify_existing_app_after_generation_returns_no_files(self):
        self.check_existing_app(True, True)

    def test_should_keep_failure_when_no_app_can_be_verified(self):
        self.check_existing_app(False, False)

    def test_should_retain_failed_acceptance_instead_of_trusting_the_model(self):
        self.check_existing_app(True, True, verdict=False)

    def test_should_keep_failure_without_a_test_runner(self):
        self.check_existing_app(True, False, runner=False)

    def test_should_keep_failure_without_specs(self):
        self.check_existing_app(True, False, specs=False)

    def test_should_forward_corrections_to_codegen_and_skip_tiny(self):
        self.check_existing_app(True, True, correction='Restore the previously verified navigation behavior')

    def check_existing_app(self, has_app, should_verify, verdict=True, runner=True, specs=True, correction=None):
        import tempfile
        from pathlib import Path
        from unittest.mock import Mock
        with tempfile.TemporaryDirectory() as directory:
            flow = Mock(spec=m.Flow)
            flow.output_dir = Path(directory)
            flow.req_dir = Path(directory)
            flow.spec_map = {'feature': ['feature.spec.ts'] if specs else []}
            flow.node_budget_cap = 300
            flow.remaining.return_value = 600
            flow.design_enabled = False
            flow.evolution = False
            flow.has_app.return_value = has_app
            flow.codegen_mode.return_value = False
            flow.turn.return_value = (False, 'reply contained no file blocks')
            flow.node_timeout = 300
            flow.implement_fraction = .7
            flow.smoke_port = 3001
            flow.web_port = 3000
            flow.runner = Mock() if runner else None
            flow.impl_failed = []
            flow.pending_corrections = []
            flow.test_verdict = {}
            flow.acceptance_loop.return_value = verdict
            for method in ['ancestors_text', 'tests_prompt_for', 'perf_text', 'ui_contract', 'verify_text', 'corrections_text']:
                getattr(flow, method).return_value = ''
            if correction:
                flow.pending_corrections = [correction]
                flow.corrections_text.side_effect = lambda: m.Flow.corrections_text(flow)
                flow.codegen_mode.return_value = True
                flow.tiny_mode.return_value = True
                flow.tiny_turn.return_value = False
                flow.spec_bodies.return_value = 'a generic acceptance spec'
                flow.codegen_context_fits.return_value = True
                flow.codegen_reasoning.return_value = 'none'
                flow.codegen_context_chars.return_value = 20000
                flow.codegen_quote_chars.return_value = 20000   # 拆分后仍与上面同值，保持这个用例原来的行为
                flow.codegen_ports_clause.return_value = ''
                flow.codegen_turn.return_value = (True, 'generated')
            flow.runtime = Mock()
            flow.runtime.traceability.list_interfaces.return_value = []
            m.Flow.node_cycle(flow, node('feature', 'Existing capability'), [], 1, 1)
            if correction:
                sent = flow.codegen_turn.call_args.args[0]
                self.assertEqual(sent.count(correction), 1)
                self.assertEqual(flow.pending_corrections, [])
                flow.tiny_turn.assert_not_called()
            self.assertEqual(flow.acceptance_loop.called, should_verify)
            if should_verify:
                self.assertEqual(flow.test_verdict['feature'], verdict)
                self.assertEqual(flow.impl_failed, [])
            else:
                self.assertEqual(flow.impl_failed, ['feature'])


class ToolFreeExecutionTests(unittest.TestCase):
    def test_should_drop_cached_sessions_at_both_mode_boundaries(self):
        from unittest.mock import Mock
        driver = object.__new__(m.OctosDriver)
        driver.tools_disabled = False
        old = Mock(); new = Mock(); driver._session = old
        with driver.without_tools():
            self.assertTrue(driver.tools_disabled)
            old.close.assert_called_once()
            driver._session = new
        self.assertFalse(driver.tools_disabled)
        new.close.assert_called_once()
        self.assertIsNone(driver._session)

    def test_should_not_fall_back_to_unrestricted_chat(self):
        from unittest.mock import Mock, patch
        driver = object.__new__(m.OctosDriver)
        driver.tools_disabled = True
        driver._get_session = Mock(side_effect=RuntimeError('stdio unavailable'))
        driver.close = Mock()
        with patch('main.run_octos') as chat:
            ok, text = driver._run_stdio('generate', 60)
        self.assertFalse(ok)
        chat.assert_not_called()

    def test_should_restore_tool_and_proxy_state_after_generation_exception(self):
        from unittest.mock import Mock
        flow = object.__new__(m.Flow)
        flow.llm_proxy = Mock(mode='low')
        flow.driver = object.__new__(m.OctosDriver)
        flow.driver.tools_disabled = False; flow.driver._session = None
        flow.codegen_reasoning = lambda _: None
        def fail(*args, **kwargs):
            self.assertTrue(flow.driver.tools_disabled)
            raise RuntimeError('provider failed')
        flow.turn = fail
        with self.assertRaisesRegex(RuntimeError, 'provider failed'):
            flow.codegen_turn('generate', 60, 'node implement')
        self.assertFalse(flow.driver.tools_disabled)
        self.assertFalse(flow.llm_proxy.no_tools)

    def test_should_install_deny_all_before_profile_runtime_is_loaded(self):
        from unittest.mock import Mock
        from octos_stdio import OctosStdioSession
        session = Mock(spec=OctosStdioSession)
        session._send.side_effect = [{'profile_id': 'profile'}, {}]
        OctosStdioSession.bootstrap_profile(session, 'openai', 'fake', None, None, tools_disabled=True)
        session._patch_profile_config.assert_called_once_with({'tool_policy': {'deny': ['*']}})

    def test_should_use_restricted_stdio_even_when_chat_was_selected(self):
        from unittest.mock import Mock, patch
        driver = object.__new__(m.OctosDriver)
        driver.tools_disabled = True; driver.mode = 'chat'; driver.session_scope = 'run'
        driver._run_stdio = Mock(return_value=(True, 'generated'))
        with patch('main.run_octos') as chat:
            self.assertEqual(driver.run('generate', 60), (True, 'generated'))
        driver._run_stdio.assert_called_once()
        self.assertEqual(driver._run_stdio.call_args.args[0], 'generate')
        self.assertTrue(0 < driver._run_stdio.call_args.args[1] <= 60)
        chat.assert_not_called()


class RepairSourceBudgetTests(unittest.TestCase):
    def test_repair_uses_spare_context_for_omitted_source(self):
        import argparse, tempfile
        from pathlib import Path
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "frontend").mkdir()
            page = "unique page content " + "x" * 45000
            (root / "frontend/index.html").write_text(page)
            (root / "tests").mkdir()
            (root / "tests/feature.spec.ts").write_text("acceptance")
            flow = m.Flow(argparse.Namespace(web_port=3000), root, root)
            flow.tests_dir = root / "tests"
            flow.spec_map = {"feature": ["feature.spec.ts"]}
            prompt = "failure evidence\n" + flow.sources_text() + "preserve behavior"
            full = flow.codegen_repair_prompt("feature", prompt)
            self.assertIsNotNone(full)
            self.assertIn(page, full)
            self.assertIn("preserve behavior", full)
            self.assertNotIn("(omitted,", full)
            (root / "frontend/index.html").write_text("x" * 100000)
            prompt = "failure evidence\n" + flow.sources_text()
            self.assertIsNone(flow.codegen_repair_prompt("feature", prompt))


class RetryDeadlineTests(unittest.TestCase):
    def test_retries_share_remaining_time_and_skip_unaffordable_backoff(self):
        from unittest.mock import patch
        for duration, expected_calls, expected_elapsed in [(60, [60], 40), (100, [100, 30], 100)]:
            now = [0.0]
            calls = []
            driver = object.__new__(OctosDriver)
            driver.mode = 'stdio'; driver.tools_disabled = False; driver.session_scope = 'run'
            driver.close = lambda: None
            driver._run_with_heartbeat = lambda fn: fn()
            def attempt(prompt, timeout):
                calls.append(timeout)
                now[0] += min(40, timeout)
                return False, 'HTTP 503 temporarily unavailable'
            driver._run_stdio = attempt
            with patch('main.time.monotonic', side_effect=lambda: now[0]), patch('main.time.sleep', side_effect=lambda seconds: now.__setitem__(0, now[0] + seconds)):
                ok, text = driver.run('generic request', duration)
            self.assertFalse(ok)
            self.assertEqual(calls, expected_calls)
            self.assertEqual(now[0], expected_elapsed)


class ContractPromptParityTests(unittest.TestCase):
    """main.py inlines the contract prompts and arc/prompts/ ships the same text
    to the Rust engine; an edit to one copy silently leaves the other behind."""

    CONTRACTS = {
        "UI_CONTRACT_CORE": "ui-contract-core.md",
        "UI_CONTRACT_DATA": "ui-contract-data.md",
        "UI_CONTRACT_SESSION": "ui-contract-session.md",
        "ARCHITECTURE_CONTRACT": "architecture-contract.md",
    }

    def test_should_ship_the_same_contract_text_in_both_copies(self):
        from pathlib import Path
        prompts = Path(m.__file__).parent / "prompts"
        for constant, filename in self.CONTRACTS.items():
            with self.subTest(constant):
                self.assertEqual(getattr(m, constant).strip(),
                                 (prompts / filename).read_text().strip())

    def test_should_require_per_item_controls_to_name_their_item(self):
        # Cloud 746c81a2b5aa: every note card rendered `button "More options"`,
        # so a name-based lookup clicked whichever card was hovered last.
        contract = m.UI_CONTRACT_CORE
        self.assertIn("repeated once per item", contract)
        self.assertIn("accessible name", contract)


class KilledSuiteRetryTests(unittest.TestCase):
    """How much memory a suite needs is not known before running it. A kill used
    to end the repair loop outright (cloud 29c840566f36)."""

    def _flow(self, outcomes):
        import argparse, tempfile
        from pathlib import Path
        from types import SimpleNamespace
        from acceptance import RunSummary, TestOutcome
        root = Path(tempfile.mkdtemp()); (root / "t").mkdir()
        for n in ("REQ-1", "REQ-2"):
            (root / "t" / f"{n}.spec.ts").write_text("x")
        flow = m.Flow(argparse.Namespace(web_port=1), root, root)
        flow.tests_dir = root / "t"
        flow.spec_map = {"REQ-1": ["REQ-1.spec.ts"], "REQ-2": ["REQ-2.spec.ts"], None: []}
        flow.runner = SimpleNamespace(root=root, work_dir=root / "prepared")
        flow.test_verdict = {"REQ-1": False}
        flow.mem_limit = 4 * 1024 * 1024 * 1024  # 4 GiB -> four workers
        flow.commits = []
        self.seen = []
        it = iter(outcomes)
        def run_specs(specs, workers=None, grader_like=False):
            self.seen.append(workers)
            passed = next(it)
            if passed is None:
                return RunSummary(error="playwright was killed (rc=-9)", killed=True)
            results = [TestOutcome(title=n, ok=i < passed, status="passed" if i < passed else "failed",
                                   duration_ms=1, file=f"{n}.spec.ts")
                       for i, n in enumerate(["REQ-1", "REQ-2"])]
            return RunSummary(passed=passed, total=2, results=results)
        flow.run_specs = run_specs
        flow.head = lambda: "sha0"
        flow.commit = lambda msg: flow.commits.append(msg) or True
        flow.restore_app = lambda sha: None
        flow.turn = lambda *a, **k: (True, "repaired")
        flow.record_tests = lambda *a, **k: None
        flow.remaining = lambda: 10_000
        flow.wound_down = lambda: False
        flow.sources_text = lambda: ""; flow.corrections_text = lambda: ""
        flow.pending_corrections = []
        return flow

    def test_should_halve_the_workers_after_a_kill_instead_of_giving_up(self):
        from unittest.mock import patch
        flow = self._flow([None, None, 2])  # killed at 4 and at 2, runs at 1
        with patch.dict("os.environ", {"OCTOS_FINAL_REPAIR_ROUNDS": "1"}):
            flow.final_acceptance()
        self.assertEqual(self.seen, [4, 2, 1])
        self.assertTrue(all(flow.test_verdict.values()))

    def test_should_stop_when_even_one_worker_is_killed(self):
        from unittest.mock import patch
        flow = self._flow([None, None, None])
        with patch.dict("os.environ", {"OCTOS_FINAL_REPAIR_ROUNDS": "1"}):
            flow.final_acceptance()
        self.assertEqual(self.seen, [4, 2, 1])
        self.assertFalse(flow.test_verdict["REQ-1"])  # per-node verdicts survive


class PostflightReapTests(unittest.TestCase):
    """Cloud 746c81a2b5aa: the harness measured 30/32, then the grader's four
    Chromium workers were killed four tests in and every test scored as skipped.
    Servers a model turn left running are memory the run can still give back."""

    def _flow(self):
        import argparse, tempfile
        from pathlib import Path
        flow = m.Flow(argparse.Namespace(web_port=1), Path(tempfile.mkdtemp()), Path('.'))
        flow.driver = None
        return flow

    def test_should_reap_leftover_servers_when_handing_the_box_back(self):
        from unittest.mock import patch
        flow = self._flow()
        with patch.object(m, 'reap_workspace_processes', return_value=3) as reap, \
             patch.object(flow, 'cleanup_playwright') as playwright, \
             patch.object(flow, 'stop_llm_proxy') as proxy:
            flow.postflight()
        reap.assert_called_once_with(flow.output_dir, m.log)
        playwright.assert_called_once_with()
        proxy.assert_called_once_with()

    def test_should_close_the_driver_before_reaping(self):
        from unittest.mock import MagicMock, patch
        flow = self._flow()
        order = []
        flow.driver = MagicMock()
        flow.driver.close.side_effect = lambda: order.append('driver')
        with patch.object(m, 'reap_workspace_processes', side_effect=lambda *a: order.append('reap') or 0), \
             patch.object(flow, 'cleanup_playwright'), patch.object(flow, 'stop_llm_proxy'):
            flow.postflight()
        self.assertEqual(order, ['driver', 'reap'])

    def test_should_be_what_the_run_teardown_calls(self):
        import inspect
        teardown = inspect.getsource(m.Flow.run).rsplit('finally:', 1)[-1]
        self.assertIn('self.postflight()', teardown)


class InterferenceNoteTests(unittest.TestCase):
    """Cloud 746c81a2b5aa lost REQ-5.1 to shared server state: it passed alone
    and failed in the suite, and the evidence for that looks like any other
    failure."""

    def test_should_point_at_the_files_the_run_changed(self):
        note = m.Flow.interference_note({"REQ-5.1": [], None: []}, {"REQ-5.1"},
                                        ["backend/data/notes.json"])
        self.assertIn("backend/data/notes.json", note)
        self.assertNotIn("changed on disk", m.Flow.interference_note({"REQ-5.1": []}, {"REQ-5.1"}, []))

    def test_should_name_only_the_behaviours_that_passed_alone(self):
        note = m.Flow.interference_note({"REQ-5.1": [], "REQ-2.7.4": [], None: []},
                                        {"REQ-5.1", "REQ-3.2"})
        self.assertIn("REQ-5.1", note)
        self.assertNotIn("REQ-2.7.4", note)  # never passed alone: an ordinary defect
        self.assertNotIn("REQ-3.2", note)    # passed alone and still passes
        self.assertIn("check the behaviour still works on its own", note)

    def test_should_say_nothing_when_no_failure_ever_passed_alone(self):
        self.assertEqual(m.Flow.interference_note({"REQ-2.7.4": []}, {"REQ-3.2"}), "")
        self.assertEqual(m.Flow.interference_note({}, {"REQ-3.2"}), "")

    def test_should_reach_the_repair_prompt_for_a_suite_only_failure(self):
        import argparse, tempfile
        from pathlib import Path
        from types import SimpleNamespace
        from unittest.mock import Mock, patch
        from acceptance import RunSummary, TestOutcome
        root = Path(tempfile.mkdtemp()); (root / "t").mkdir()
        for n in ("REQ-1", "REQ-2"):
            (root / "t" / f"{n}.spec.ts").write_text("x")
        flow = m.Flow(argparse.Namespace(web_port=1), root, root)
        flow.tests_dir = root / "t"
        flow.spec_map = {"REQ-1": ["REQ-1.spec.ts"], "REQ-2": ["REQ-2.spec.ts"], None: []}
        flow.runner = SimpleNamespace(root=root, work_dir=root / "prepared")
        flow.test_verdict = {"REQ-1": True, "REQ-2": True}  # both passed on their own
        flow.requirement_nodes = {}
        flow.run_specs = lambda specs, workers=None, grader_like=False: RunSummary(
            passed=1, total=2, results=[
                TestOutcome(title="REQ-1", ok=True, status="passed", duration_ms=1, file="REQ-1.spec.ts"),
                TestOutcome(title="REQ-2", ok=False, status="failed", duration_ms=1, file="REQ-2.spec.ts",
                            message="stale shared counter")])
        flow.head = lambda: "sha0"
        flow.commit = lambda msg: True
        flow.restore_app = lambda sha: None
        flow.record_tests = lambda *a, **k: None
        flow.remaining = lambda: 10_000
        flow.wound_down = lambda: False
        flow.sources_text = lambda: ""; flow.corrections_text = lambda: ""
        flow.pending_corrections = []
        flow.turn = Mock(return_value=(True, "repaired"))
        with patch.dict("os.environ", {"OCTOS_FINAL_REPAIR_ROUNDS": "1"}):
            flow.final_acceptance()
        prompt = flow.turn.call_args.args[0]
        self.assertIn("passed their own node run earlier", prompt)
        self.assertIn("REQ-2", prompt)


class RepairRequestBudgetTests(unittest.TestCase):
    """Cloud 746c81a2b5aa and 3ffe9702bf15 hit a fixed 10-request cap on 17% and
    23% of their repair turns — the last repair of every node that stayed broken
    among them — while using 7% of the token budget and 7% of the time."""

    def _budget(self, label, nodes):
        import argparse
        from pathlib import Path
        from types import SimpleNamespace
        flow = m.Flow(argparse.Namespace(web_port=1), Path('.'), Path('.'))
        flow.n_nodes = nodes
        seen = {}
        flow.llm_proxy = SimpleNamespace(mode='x', phase='', turn_budget=0, turn_requests=0,
                                         begin_turn=lambda b: seen.setdefault('budget', b))
        flow.driver = SimpleNamespace(run=lambda *a, **k: (True, 'done'))
        flow.probe_summaries = {}
        flow.turn_count = 0
        flow.protected_prefixes = lambda: []
        flow.restore_protected = lambda: []
        flow.pending_corrections = []
        flow.guard_enabled = True
        flow.turn(prompt='p', timeout=60, label=label)
        return seen['budget']

    def test_should_give_a_repair_the_same_room_as_the_turn_that_wrote_the_code(self):
        self.assertEqual(self._budget('REQ-1 repair 1/3', nodes=32),
                         self._budget('REQ-1 implement', nodes=32))

    def test_should_not_cap_a_repair_below_ten_on_a_large_tree(self):
        self.assertEqual(self._budget('REQ-1 repair 3/3', nodes=32), 0)  # 0 = uncapped

    def test_should_keep_a_cap_on_a_small_task(self):
        self.assertEqual(self._budget('REQ-1 repair 1/3', nodes=1), 20)

    def test_should_still_honour_an_explicit_override(self):
        from unittest.mock import patch
        with patch.dict('os.environ', {'OCTOS_ARC_REPAIR_REQUESTS': '4'}):
            self.assertEqual(self._budget('REQ-1 repair 1/3', nodes=32), 4)


class SlowTestThresholdTests(unittest.TestCase):
    """In cloud 3ffe9702bf15 the grader passed twenty-five tests; three of them
    ran 3.4-3.9 s and the flat 3000 ms threshold handed all three to the model as
    work to optimise."""

    GRADED_PASSING_MS = [2500, 2999, 3401, 3804, 3893, 2106, 2101, 1844, 1026, 805, 2395,
                         1285, 1095, 2500, 906, 1107, 1403, 2091, 1890, 922, 1512, 1891,
                         1081, 1189, 1412]

    def _flow(self, timeout_ms=10000):
        import argparse
        from pathlib import Path
        from types import SimpleNamespace
        flow = m.Flow(argparse.Namespace(web_port=1), Path('.'), Path('.'))
        flow.runner = SimpleNamespace(timeout_ms=timeout_ms)
        return flow

    def test_should_follow_the_budget_a_test_is_graded_against(self):
        self.assertEqual(self._flow(timeout_ms=10000).slow_test_ms(), 5000)
        self.assertEqual(self._flow(timeout_ms=6000).slow_test_ms(), 3000)

    def test_should_not_flag_tests_the_grader_passed_comfortably(self):
        threshold = self._flow().slow_test_ms()
        flagged = [ms for ms in self.GRADED_PASSING_MS if ms >= threshold]
        self.assertEqual(flagged, [])
        # the old flat threshold flagged three of them
        self.assertEqual(len([ms for ms in self.GRADED_PASSING_MS if ms >= 3000]), 3)

    def test_should_still_honour_an_explicit_override(self):
        from unittest.mock import patch
        with patch.dict('os.environ', {'OCTOS_ARC_SLOW_MS': '1500'}):
            self.assertEqual(self._flow().slow_test_ms(), 1500)

    def test_should_keep_a_floor_for_a_tiny_timeout(self):
        self.assertEqual(self._flow(timeout_ms=500).slow_test_ms(), 1000)


class CheckpointEvidenceTests(unittest.TestCase):
    """A regression checkpoint queues its evidence as a correction for the next
    node's turn. With a page snapshot per failure that evidence runs past
    14000 characters for two regressions, and the old head-only slice at 8000
    dropped the later ones and the source context with them."""

    def _summary(self, failures):
        from acceptance import RunSummary, TestOutcome
        tree = "\n".join(f"- generic [ref=e{n}]: row {n}" for n in range(300))
        return RunSummary(passed=0, total=failures, results=[
            TestOutcome(title=f"REQ-{i}: behaviour {i}", ok=False, status="timedOut", duration_ms=1,
                        file=f"REQ-{i}.spec.ts", message="TimeoutError: locator.click", rendered_page=tree)
            for i in range(failures)])

    def _correction(self, failures):
        import argparse, tempfile
        from pathlib import Path
        from types import SimpleNamespace
        from unittest.mock import patch
        root = Path(tempfile.mkdtemp()); (root / "t").mkdir()
        flow = m.Flow(argparse.Namespace(web_port=1), root, root)
        flow.tests_dir = root / "t"
        flow.runner = SimpleNamespace(root=root, work_dir=root / "prepared", timeout_ms=10000)
        flow.spec_map = {f"REQ-{i}": [f"REQ-{i}.spec.ts"] for i in range(failures)}
        flow.spec_map[None] = []
        flow.test_verdict = {f"REQ-{i}": True for i in range(failures)}
        flow.pending_corrections = []
        flow.min_repair_seconds = 300
        flow.remaining = lambda: 10_000
        flow.run_specs = lambda specs, workers=None, grader_like=False: self._summary(failures)
        flow.mark = lambda *a, **k: None
        flow.repair_regressions = lambda *a, **k: None  # covered by CheckpointRepairTests
        with patch.dict("os.environ", {"OCTOS_ARC_REGRESSION_CHECKPOINT": "2"}):
            flow.regression_checkpoint(2, 8)
        return flow.pending_corrections[-1]

    def test_should_describe_every_regression_it_reports(self):
        correction = self._correction(3)
        for i in range(3):
            self.assertIn(f"REQ-{i}: behaviour {i}", correction)

    def test_should_stay_within_its_budget(self):
        self.assertLessEqual(len(self._correction(3)), 12000 + 400)

    def test_should_keep_both_ends_when_it_has_to_cut(self):
        # Enough regressions that the blocks alone overrun the budget. Four no
        # longer do: snapshots now stop at the budget instead of taking 800
        # characters each however many failures there are, so the evidence for a
        # handful of regressions fits without the outer cut.
        from unittest.mock import patch
        with patch.dict("os.environ", {"OCTOS_ARC_CHECKPOINT_EVIDENCE": "2000"}):
            correction = self._correction(12)
        self.assertIn("REQ-0: behaviour 0", correction)
        self.assertIn("elided", correction)


class CheckpointRepairTests(unittest.TestCase):
    """Cloud e767e871a6c6: checkpoint 8 reported REQ-2.3.1/2/3 broken, checkpoint
    16 reported the same three plus four more, and no recovery was recorded in
    between. Queueing the evidence for the next node's turn fixes nothing — that
    turn has its own node, and its acceptance covers only its own spec."""

    def _flow(self, rounds_results):
        import argparse, tempfile
        from pathlib import Path
        from types import SimpleNamespace
        from unittest.mock import Mock
        from acceptance import RunSummary, TestOutcome
        root = Path(tempfile.mkdtemp()); (root / "t").mkdir()
        names = ["REQ-1", "REQ-2"]
        for n in names:
            (root / "t" / f"{n}.spec.ts").write_text("x")
        flow = m.Flow(argparse.Namespace(web_port=1), root, root)
        flow.tests_dir = root / "t"
        flow.spec_map = {n: [f"{n}.spec.ts"] for n in names}; flow.spec_map[None] = []
        flow.runner = SimpleNamespace(root=root, work_dir=root / "prepared", timeout_ms=10000)
        flow.test_verdict = {n: True for n in names}
        flow.requirement_nodes = {}
        flow.pending_corrections = []
        flow.min_repair_seconds = 300
        flow.remaining = lambda: 10_000
        flow.wound_down = lambda: False
        flow.mark = lambda *a, **k: None
        flow.commit = lambda msg: True
        flow.sources_text = lambda: ""; flow.corrections_text = lambda: ""
        flow.record_tests = lambda *a, **k: None
        flow.turn = Mock(return_value=(True, "repaired"))
        it = iter(rounds_results)
        def run_specs(specs, workers=None, grader_like=False):
            passed = next(it)
            return RunSummary(passed=passed, total=2, results=[
                TestOutcome(title=n, ok=i < passed, status="passed" if i < passed else "failed",
                            duration_ms=1, file=f"{n}.spec.ts") for i, n in enumerate(names)])
        flow.run_specs = run_specs
        return flow

    def test_should_repair_a_regression_before_the_next_node(self):
        from unittest.mock import patch
        flow = self._flow([1, 2])  # checkpoint finds one broken, the repair fixes it
        with patch.dict("os.environ", {"OCTOS_ARC_REGRESSION_CHECKPOINT": "2"}):
            flow.regression_checkpoint(2, 8)
        self.assertEqual(flow.turn.call_count, 1)
        self.assertIn("checkpoint 2 repair", flow.turn.call_args.args[2])
        self.assertTrue(all(flow.test_verdict.values()))

    def test_should_not_repair_when_nothing_regressed(self):
        from unittest.mock import patch
        flow = self._flow([2])
        with patch.dict("os.environ", {"OCTOS_ARC_REGRESSION_CHECKPOINT": "2"}):
            flow.regression_checkpoint(2, 8)
        flow.turn.assert_not_called()

    def test_should_leave_the_verdict_false_when_no_repair_round_takes(self):
        from unittest.mock import patch
        flow = self._flow([1, 1, 1])
        with patch.dict("os.environ", {"OCTOS_ARC_REGRESSION_CHECKPOINT": "2"}):
            flow.regression_checkpoint(2, 8)
        self.assertEqual(flow.turn.call_count, 2)
        self.assertFalse(flow.test_verdict["REQ-2"])

    def test_should_try_a_second_round_when_the_first_does_not_take(self):
        """Cloud keep 4e18c76637ae: one round per checkpoint cleared two of five
        regressions; REQ-2.2 and REQ-2.4 stayed broken for 97 minutes and three
        checkpoints until the final suite caught them. On a 125-node tree the final
        suite has no budget left to be that backstop."""
        from unittest.mock import patch
        flow = self._flow([1, 1, 2])   # checkpoint finds one broken, round 1 misses, round 2 fixes
        with patch.dict("os.environ", {"OCTOS_ARC_REGRESSION_CHECKPOINT": "2"}):
            flow.regression_checkpoint(2, 8)
        self.assertEqual(flow.turn.call_count, 2)
        self.assertTrue(all(flow.test_verdict.values()))

    def test_should_stop_at_one_round_when_told_to(self):
        from unittest.mock import patch
        flow = self._flow([1, 1])
        with patch.dict("os.environ", {"OCTOS_ARC_REGRESSION_CHECKPOINT": "2",
                                       "OCTOS_ARC_CHECKPOINT_REPAIRS": "1"}):
            flow.regression_checkpoint(2, 8)
        self.assertEqual(flow.turn.call_count, 1)

    def test_should_skip_the_repair_when_the_budget_is_gone(self):
        from unittest.mock import patch
        flow = self._flow([1])
        flow.remaining = lambda: 10
        with patch.dict("os.environ", {"OCTOS_ARC_REGRESSION_CHECKPOINT": "2"}):
            flow.regression_checkpoint(2, 8)
        flow.turn.assert_not_called()


class InlineSourceBudgetTests(unittest.TestCase):
    """Cloud e767e871a6c6 carried a 49 KB frontend/src/index.html and a 22 KB
    backend/server.js. Quoting smallest first inside a 40000-character budget
    spent it on seed data, lockfiles and the secondary pages, and omitted the one
    file holding the whole UI — where nearly every failure lives."""

    def _app(self):
        import tempfile
        from pathlib import Path
        root = Path(tempfile.mkdtemp())
        (root / "frontend" / "src").mkdir(parents=True)
        (root / "backend" / "data").mkdir(parents=True)
        (root / "frontend" / "src" / "index.html").write_text("<!--ui-->" + "u" * 49000)
        (root / "frontend" / "src" / "about.html").write_text("<!--about-->" + "a" * 5000)
        (root / "frontend" / "package.json").write_text('{"name":"f"}')
        (root / "backend" / "server.js").write_text("//server\n" + "s" * 22000)
        (root / "backend" / "data" / "notes.json").write_text("[" + '"n",' * 1200 + '"n"]')
        (root / "backend" / "package-lock.json").write_text('{"lock":true}')
        return root

    def test_should_quote_the_biggest_source_before_the_peripheral_files(self):
        root = self._app()
        text = m.inline_sources(root, 90000)
        self.assertIn("--- frontend/src/index.html ---\n", text)
        self.assertIn("--- backend/server.js ---\n", text)
        self.assertLessEqual(len(text), 90000 + 2000)

    def _oversized_app(self):
        """A hundred-node task grows one dominant UI file past the whole budget."""
        import tempfile
        from pathlib import Path
        root = Path(tempfile.mkdtemp())
        for rel, n in (("frontend/src/index.html", 220_000), ("backend/server.js", 60_000),
                       ("frontend/src/app.css", 18_000), ("backend/data.json", 9_000)):
            p = root / rel
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text("x" * n)
        return root

    def test_should_quote_part_of_a_file_too_large_to_fit_whole(self):
        # Dropping it left the prompt quoting the stylesheet and the seed data
        # and not the file every repair edits.
        text = m.inline_sources(self._oversized_app(), 90000)
        self.assertIn("frontend/src/index.html --- (too large to quote whole, 220000 chars", text)
        self.assertIn("read it for the rest", text)
        self.assertIn("characters elided", text)          # cannot read as the whole file

    def test_should_share_the_budget_between_oversized_files(self):
        text = m.inline_sources(self._oversized_app(), 90000)
        omitted = " ".join(l for l in text.splitlines() if "(omitted," in l)
        self.assertNotIn("index.html", omitted)
        self.assertNotIn("server.js", omitted)            # one file cannot take it all
        self.assertLessEqual(len(text), 90000 + 2000)

    def test_should_refuse_a_tool_free_repair_when_the_source_is_only_clipped(self):
        """A codegen repair has no tools and must re-emit the file whole, so a
        clipped view is as disqualifying as an omitted one."""
        import argparse, tempfile
        from pathlib import Path
        root = Path(tempfile.mkdtemp())
        (root / "frontend").mkdir(); (root / "tests").mkdir()
        (root / "frontend" / "index.html").write_text("x" * 400_000)
        (root / "tests" / "feature.spec.ts").write_text("acceptance")
        flow = m.Flow(argparse.Namespace(web_port=3000), root, root)
        flow.tests_dir = root / "tests"; flow.spec_map = {"feature": ["feature.spec.ts"]}
        sources = flow.sources_text()
        self.assertIn("too large to quote whole", sources)   # clipped, not omitted
        self.assertIsNone(flow.codegen_repair_prompt("feature", "evidence\n" + sources))

    def test_should_still_omit_a_file_when_the_room_left_is_too_small_to_help(self):
        # 25 chars of a 100-char file teaches nothing; say it was omitted instead.
        import tempfile
        from pathlib import Path
        root = Path(tempfile.mkdtemp())
        (root / "backend").mkdir(); (root / "frontend" / "src").mkdir(parents=True)
        (root / "backend" / "server.js").write_text("x" * 100)
        (root / "frontend" / "src" / "index.html").write_text("<p>hi</p>")
        text = m.inline_sources(root, max_chars=50)
        self.assertIn("backend/server.js --- (omitted, 100 chars", text)
        self.assertNotIn("too large to quote whole", text)

    def test_should_omit_the_least_important_file_when_it_cannot_fit_everything(self):
        root = self._app()
        omitted = [l for l in m.inline_sources(root, 80000).splitlines() if "(omitted," in l]
        self.assertTrue(omitted)
        self.assertNotIn("index.html", " ".join(omitted))

    def test_should_let_the_suite_repair_timeout_move_without_the_node_timeout(self):
        """A repair answering every failing node at once is a different size of
        job from one about a single node. Measured 2026-09-16: 207 node-phase
        turns, none reached the 1200s cap; both of keep's full-suite repairs
        were cut at it."""
        import argparse, tempfile
        from pathlib import Path
        from unittest.mock import patch
        root = Path(tempfile.mkdtemp())
        flow = m.Flow(argparse.Namespace(web_port=1), root, root)
        self.assertEqual(flow.suite_repair_timeout(), flow.node_timeout)
        with patch.dict("os.environ", {"OCTOS_SUITE_REPAIR_TIMEOUT": "2400"}):
            self.assertEqual(flow.suite_repair_timeout(), 2400)
            self.assertEqual(flow.node_timeout, 1200)      # node turns unmoved

    def test_should_let_the_inline_budget_move_without_the_codegen_budget(self):
        """They default to one number but bound different things: how much source
        a tool-using turn is shown, versus how much a tool-free turn is asked to
        re-emit. Raising one must not drag the other along."""
        import argparse, tempfile
        from pathlib import Path
        from unittest.mock import patch
        root = Path(tempfile.mkdtemp())
        flow = m.Flow(argparse.Namespace(web_port=1), root, root)
        self.assertEqual(flow.inline_source_chars(), flow.codegen_context_chars())
        with patch.dict("os.environ", {"OCTOS_ARC_INLINE_SOURCE_CHARS": "250000"}):
            self.assertEqual(flow.inline_source_chars(), 250000)
            self.assertEqual(flow.codegen_context_chars(), 90000)   # output budget unmoved

    def test_should_give_a_repair_the_budget_the_codegen_turn_gets(self):
        import argparse
        from pathlib import Path
        flow = m.Flow(argparse.Namespace(web_port=1), self._app(), Path('.'))
        self.assertIn("--- frontend/src/index.html ---\n", flow.sources_text())
        self.assertEqual(int(os.environ.get("OCTOS_ARC_INLINE_SOURCE_CHARS",
                                            str(flow.codegen_context_chars()))),
                         flow.codegen_context_chars())

    def test_should_still_honour_an_explicit_override(self):
        import argparse
        from pathlib import Path
        from unittest.mock import patch
        flow = m.Flow(argparse.Namespace(web_port=1), self._app(), Path('.'))
        with patch.dict('os.environ', {'OCTOS_ARC_INLINE_SOURCE_CHARS': '0'}):
            self.assertEqual(flow.sources_text(), "")


class IntermittentNoteTests(unittest.TestCase):
    """The same 32 specs over one unchanged app gave 25, 24, 25 — REQ-2.7.1
    failed once and passed twice. A repair told only that it failed looks for a
    missing feature when what it has is a race."""

    def test_should_name_only_what_already_passed_this_pass(self):
        note = m.Flow.intermittent_note({"REQ-2.7.1": [], "REQ-4.2": [], None: []},
                                        {"REQ-2.7.1", "REQ-3.2"})
        self.assertIn("REQ-2.7.1", note)
        self.assertNotIn("REQ-4.2", note)   # never passed: a real defect
        self.assertNotIn("REQ-3.2", note)   # passed and still passes
        self.assertIn("settles", note)

    def test_should_say_nothing_on_the_first_round(self):
        self.assertEqual(m.Flow.intermittent_note({"REQ-4.2": []}, set()), "")

    def test_should_reach_the_repair_prompt_after_a_flaky_round(self):
        import argparse, tempfile
        from pathlib import Path
        from types import SimpleNamespace
        from unittest.mock import Mock, patch
        from acceptance import RunSummary, TestOutcome
        root = Path(tempfile.mkdtemp()); (root / "t").mkdir()
        for n in ("REQ-1", "REQ-2"):
            (root / "t" / f"{n}.spec.ts").write_text("x")
        flow = m.Flow(argparse.Namespace(web_port=1), root, root)
        flow.tests_dir = root / "t"
        flow.spec_map = {"REQ-1": ["REQ-1.spec.ts"], "REQ-2": ["REQ-2.spec.ts"], None: []}
        flow.runner = SimpleNamespace(root=root, work_dir=root / "prepared", timeout_ms=10000)
        flow.test_verdict = {}
        flow.requirement_nodes = {}
        # REQ-1 stays broken so the pass keeps going; REQ-2 passes, then flakes.
        outcomes = iter([(False, True), (False, False), (False, False)])
        def run_specs(specs, workers=None, grader_like=False):
            ok1, ok2 = next(outcomes)
            rows = [TestOutcome(title="REQ-1", ok=ok1, status="passed" if ok1 else "failed",
                                duration_ms=1, file="REQ-1.spec.ts"),
                    TestOutcome(title="REQ-2", ok=ok2, status="passed" if ok2 else "failed",
                                duration_ms=1, file="REQ-2.spec.ts", message="TimeoutError")]
            return RunSummary(passed=sum(1 for r in rows if r.ok), total=2, results=rows)
        flow.run_specs = run_specs
        flow.head = lambda: "sha0"; flow.commit = lambda msg: True
        flow.restore_app = lambda sha: None; flow.record_tests = lambda *a, **k: None
        flow.remaining = lambda: 10_000; flow.wound_down = lambda: False
        flow.sources_text = lambda: ""; flow.corrections_text = lambda: ""
        flow.pending_corrections = []
        flow.turn = Mock(return_value=(True, "repaired"))
        with patch.dict("os.environ", {"OCTOS_FINAL_REPAIR_ROUNDS": "2"}):
            flow.final_acceptance()
        prompt = flow.turn.call_args.args[0]
        self.assertIn("already passed in an earlier round", prompt)
        self.assertIn("REQ-2", prompt)


class VerificationDemandTests(unittest.TestCase):
    """`verify_text` has the proxy drop the shell tools in minimal mode. The
    guard then asked a turn that cannot run a command why it had not run one."""

    def _monitor_for(self, dropped):
        import argparse
        from pathlib import Path
        from types import SimpleNamespace
        from unittest.mock import patch
        flow = m.Flow(argparse.Namespace(web_port=1), Path('.'), Path('.'))
        flow.n_nodes = 32
        flow.llm_proxy = SimpleNamespace(mode='x', phase='', turn_budget=0, turn_requests=0,
                                         extra_drop_tools=dropped, begin_turn=lambda b: None)
        flow.driver = SimpleNamespace(run=lambda *a, **k: (True, 'done'))
        flow.probe_summaries = {}; flow.turn_count = 0
        flow.protected_prefixes = lambda: []
        flow.restore_protected = lambda: []
        flow.pending_corrections = []; flow.guard_enabled = True
        seen = {}
        real = m.TurnMonitor
        with patch.object(m, 'TurnMonitor', lambda *a, **k: seen.setdefault('m', real(*a, **k))):
            flow.turn(prompt='p', timeout=60, label='REQ-1 implement')
        return seen['m']

    def test_should_not_demand_verification_without_a_shell(self):
        self.assertFalse(self._monitor_for({'bash', 'shell'}).expect_verification)

    def test_should_still_demand_it_when_the_shell_is_there(self):
        self.assertTrue(self._monitor_for(set()).expect_verification)


class WorkerParityNoteTests(unittest.TestCase):
    """A memory limit can force the count below the grader's, and then the run
    the repair is shown is not the run that scores the app. (An earlier version
    of this note blamed the count for changing which tests fail; more repeats
    showed those two nodes swap at a fixed count too — that is flakiness, and
    #175 covers it.)"""

    def test_should_say_nothing_when_it_matches_the_grader(self):
        self.assertEqual(m.Flow.worker_parity_note(4), "")
        self.assertEqual(m.Flow.worker_parity_note(8), "")

    def test_should_warn_when_memory_forced_the_count_down(self):
        note = m.Flow.worker_parity_note(1)
        self.assertIn("1 worker(s)", note)
        self.assertIn("grading runs 4", note)
        self.assertIn("not the run that scores the app", note)

    def test_should_reach_the_repair_prompt(self):
        import argparse, tempfile
        from pathlib import Path
        from types import SimpleNamespace
        from unittest.mock import Mock, patch
        from acceptance import RunSummary, TestOutcome
        root = Path(tempfile.mkdtemp()); (root / "t").mkdir()
        for n in ("REQ-1", "REQ-2"):
            (root / "t" / f"{n}.spec.ts").write_text("x")
        flow = m.Flow(argparse.Namespace(web_port=1), root, root)
        flow.tests_dir = root / "t"
        flow.spec_map = {"REQ-1": ["REQ-1.spec.ts"], "REQ-2": ["REQ-2.spec.ts"], None: []}
        flow.runner = SimpleNamespace(root=root, work_dir=root / "prepared", timeout_ms=10000)
        flow.mem_limit = 512 * 1024 * 1024  # forces one worker
        flow.test_verdict = {}; flow.requirement_nodes = {}
        flow.run_specs = lambda specs, workers=None, grader_like=False: RunSummary(
            passed=1, total=2, results=[
                TestOutcome(title="REQ-1", ok=True, status="passed", duration_ms=1, file="REQ-1.spec.ts"),
                TestOutcome(title="REQ-2", ok=False, status="failed", duration_ms=1,
                            file="REQ-2.spec.ts", message="boom")])
        flow.head = lambda: "sha0"; flow.commit = lambda msg: True
        flow.restore_app = lambda sha: None; flow.record_tests = lambda *a, **k: None
        flow.remaining = lambda: 10_000; flow.wound_down = lambda: False
        flow.sources_text = lambda: ""; flow.corrections_text = lambda: ""
        flow.pending_corrections = []
        flow.turn = Mock(return_value=(True, "repaired"))
        with patch.dict("os.environ", {"OCTOS_FINAL_REPAIR_ROUNDS": "1"}):
            flow.final_acceptance()
        self.assertIn("grading runs 4", flow.turn.call_args.args[0])


class LastRepairDiffTests(unittest.TestCase):
    """Cloud e767e871a6c6 ran four full-suite rounds over eleven failures that
    never budged. "You changed these files and nothing moved" is a different
    instruction from "it failed again"."""

    def _flow(self, stdout):
        import argparse
        from pathlib import Path
        from types import SimpleNamespace
        flow = m.Flow(argparse.Namespace(web_port=1), Path('.'), Path('.'))
        self.args = []
        flow.runtime = SimpleNamespace(git=SimpleNamespace(
            run=lambda a, check=True: (self.args.append(a),
                                       SimpleNamespace(stdout=stdout))[1]))
        return flow

    def test_should_name_what_the_last_repair_touched(self):
        note = self._flow(" frontend/src/index.html | 12 ++++---\n 1 file changed\n").last_repair_diff()
        self.assertIn("frontend/src/index.html", note)
        self.assertIn("did not move", note)

    def test_should_say_plainly_when_nothing_was_written(self):
        note = self._flow("").last_repair_diff()
        self.assertIn("left frontend/ and backend/ unchanged", note)
        self.assertIn("Make an edit this time", note)

    def test_should_compare_the_last_commit_against_the_one_before(self):
        self._flow("x").last_repair_diff()
        self.assertEqual(self.args[0][:4], ["diff", "HEAD~1", "HEAD", "--stat"])


    def test_should_stay_bounded(self):
        big = "\n".join(f" file{i}.js | {i} +++" for i in range(400))
        self.assertLessEqual(len(self._flow(big).last_repair_diff()), 1200)

    def test_should_stay_silent_rather_than_break_the_loop(self):
        import argparse
        from pathlib import Path
        from types import SimpleNamespace
        flow = m.Flow(argparse.Namespace(web_port=1), Path('.'), Path('.'))
        self.assertEqual(flow.last_repair_diff(), "")          # no runtime at all
        def boom(*a, **k):
            raise OSError("git is gone")
        flow.runtime = SimpleNamespace(git=SimpleNamespace(run=boom))
        self.assertEqual(flow.last_repair_diff(), "")


class UnfinishedRepairNoteTests(unittest.TestCase):
    """Cloud e767e871a6c6 spent all three full-suite repair rounds investigating
    and never reached an edit, finishing at 20/32; each round said where it had
    got to and each message was thrown away, so the next round re-read the same
    file. That run was capped at ten requests per repair and the cap has since
    been lifted, but a turn can still end mid-plan on the per-turn timeout."""

    BUDGET_GONE = ("pin/unpin helpers click the pin button only after hover which should work, but "
                   "REQ-2.8.x failures indicate the pin toggle state/assertions need checking. Made no "
                   "file edits this turn due to budget exhaustion before changes could be applied.")
    NEXT_STEP = ("No files were modified in this turn because the investigation consumed the budget. "
                 "Next step is to apply those edits to `frontend/src/index.html`, rebuild, and re-run "
                 "the acceptance suite.")

    def _flow(self):
        import argparse
        from pathlib import Path
        return m.Flow(argparse.Namespace(web_port=1), Path('.'), Path('.'))

    def test_should_carry_the_unfinished_next_step_forward(self):
        note = self._flow().unfinished_repair_note(self.NEXT_STEP)
        self.assertIn("Next step is to apply those edits", note)
        self.assertIn("frontend/src/index.html", note)

    def test_should_say_the_work_is_already_done_not_a_fresh_idea(self):
        note = self._flow().unfinished_repair_note(self.BUDGET_GONE)
        self.assertIn("continue from it rather than reading the same files again", note)
        self.assertIn("budget", note)

    def test_should_keep_the_tail_where_the_conclusion_sits(self):
        body = "read a file\n" * 400 + "Next step is to apply those edits."
        note = self._flow().unfinished_repair_note(body, max_chars=200)
        self.assertIn("Next step is to apply those edits.", note)
        self.assertLess(len(note), 900)
        self.assertIn("…", note)

    def test_should_stay_silent_when_the_turn_said_nothing(self):
        for empty in ("", "   ", None):
            self.assertEqual(self._flow().unfinished_repair_note(empty), "")

    def test_should_not_clip_a_message_that_already_fits(self):
        note = self._flow().unfinished_repair_note(self.NEXT_STEP, max_chars=700)
        self.assertNotIn("…", note)
        self.assertIn(self.NEXT_STEP, note)

    def test_should_reach_the_next_rounds_repair_prompt(self):
        """The helper is worthless unless final_acceptance actually threads it."""
        import argparse, tempfile
        from pathlib import Path
        from types import SimpleNamespace
        from unittest.mock import patch
        from acceptance import RunSummary, TestOutcome
        root = Path(tempfile.mkdtemp()); (root / "t").mkdir()
        (root / "t" / "REQ-1.spec.ts").write_text("x")
        flow = m.Flow(argparse.Namespace(web_port=1), root, root)
        flow.tests_dir = root / "t"
        flow.spec_map = {"REQ-1": ["REQ-1.spec.ts"], "REQ-2": ["REQ-2.spec.ts"], None: []}
        flow.runner = SimpleNamespace(root=root, work_dir=root / "w", timeout_ms=10000)
        flow.test_verdict = {}; flow.requirement_nodes = {}; flow.pending_corrections = []
        seen = []
        def run_specs(specs, workers=None, grader_like=False):
            seen.append(1)  # a different failure each round, so no escalation fires
            return RunSummary(passed=0, total=1, results=[
                TestOutcome(title="REQ-1", ok=False, status="timedOut", duration_ms=1,
                            file="REQ-1.spec.ts", message=f"boom {len(seen)}")])
        flow.run_specs = run_specs
        flow.head = lambda: "sha"; flow.commit = lambda msg: True
        flow.restore_app = lambda sha: None; flow.record_tests = lambda *a, **k: None
        flow.record_full_suite = lambda *a, **k: None
        flow.remaining = lambda: 10_000; flow.wound_down = lambda: False
        flow.sources_text = lambda: ""; flow.corrections_text = lambda: ""
        flow.last_repair_diff = lambda *a, **k: ""
        prompts = []
        def turn(prompt, timeout, label, **kw):
            prompts.append(prompt)
            return True, self.NEXT_STEP
        flow.turn = turn
        with patch.dict("os.environ", {"OCTOS_FINAL_REPAIR_ROUNDS": "2"}):
            flow.final_acceptance()
        self.assertGreaterEqual(len(prompts), 2)
        self.assertNotIn("Next step is to apply", prompts[0])   # nothing to carry yet
        self.assertIn("Next step is to apply those edits", prompts[1])

    def _repeat_run(self, committed):
        """Two rounds with identical failures; `committed` is what commit() reports."""
        import argparse, tempfile
        from pathlib import Path
        from types import SimpleNamespace
        from unittest.mock import patch
        from acceptance import RunSummary, TestOutcome
        root = Path(tempfile.mkdtemp()); (root / "t").mkdir()
        (root / "t" / "REQ-1.spec.ts").write_text("x")
        flow = m.Flow(argparse.Namespace(web_port=1), root, root)
        flow.tests_dir = root / "t"
        flow.spec_map = {"REQ-1": ["REQ-1.spec.ts"], "REQ-2": ["REQ-2.spec.ts"], None: []}
        flow.runner = SimpleNamespace(root=root, work_dir=root / "w", timeout_ms=10000)
        flow.test_verdict = {}; flow.requirement_nodes = {}; flow.pending_corrections = []
        flow.run_specs = lambda specs, workers=None, grader_like=False: RunSummary(
            passed=0, total=1, results=[TestOutcome(title="REQ-1", ok=False, status="timedOut",
                                                    duration_ms=1, file="REQ-1.spec.ts", message="boom")])
        flow.head = lambda: "sha"
        flow.commit = lambda msg: committed if msg.startswith("fix:") else True
        flow.restore_app = lambda sha: None; flow.record_tests = lambda *a, **k: None
        flow.record_full_suite = lambda *a, **k: None
        flow.remaining = lambda: 10_000; flow.wound_down = lambda: False
        flow.sources_text = lambda: ""; flow.corrections_text = lambda: ""
        flow.last_repair_diff = lambda *a, **k: ""
        prompts = []
        def turn(prompt, timeout, label, **kw):
            prompts.append(prompt)
            return True, self.NEXT_STEP
        flow.turn = turn
        with patch.dict("os.environ", {"OCTOS_FINAL_REPAIR_ROUNDS": "2"}):
            flow.final_acceptance()
        return prompts, flow

    def test_should_keep_carrying_when_the_last_repair_wrote_nothing(self):
        """Nothing was edited, so there is no approach to change -- only a plan to finish."""
        prompts, _ = self._repeat_run(committed=False)
        self.assertIn("Next step is to apply those edits", prompts[1])

    def test_should_stop_carrying_once_an_edit_failed_to_move_anything(self):
        """It edited and nothing moved: the same prompt already says change the cause."""
        prompts, flow = self._repeat_run(committed=True)
        self.assertNotIn("Next step is to apply those edits", prompts[1])
        # The escalation is queued as a correction; corrections_text() renders it.
        self.assertTrue(any("Recheck the assumptions" in c for c in flow.pending_corrections))



class FailureConsoleTests(unittest.TestCase):
    """What the run log shows about a failure is the only record left once the
    run ends, so it has to carry the evidence, not just the headline."""

    def test_should_keep_the_error_body_under_an_observation(self):
        """`Observation:` is followed by the build/start log tail on its own
        lines. The console filter matched line prefixes only, so a bookstack
        node that died with `npm start exited early (rc=1):` logged exactly
        that and dropped every line of the Node traceback that said why."""
        failures = (
            "- Feature: app startup\n"
            "  Failed at: build/start\n"
            "  Observation: backend `npm start` exited early (rc=1):\n"
            "SyntaxError: Unexpected token '}'\n"
            "    at wrapSafe (node:internal/modules/cjs/loader:1281:20)\n"
            "  Steps: npm run build -> npm start")
        shown = "\n".join(m.failure_console_lines(failures))
        self.assertIn("Failed at: build/start", shown)
        self.assertIn("exited early (rc=1)", shown)
        self.assertIn("SyntaxError: Unexpected token", shown)
        self.assertNotIn("Steps: npm run build", shown)

    def test_should_collapse_and_bound_a_long_observation(self):
        failures = "  Observation: boom\n" + "\n".join(f"frame {i}" for i in range(400))
        shown = m.failure_console_lines(failures)
        self.assertEqual(len(shown), 1)
        # clip_ends keeps max_chars of text and adds its own elision marker.
        self.assertLessEqual(len(shown[0]), 800 + 60)
        self.assertIn("characters elided", shown[0])
        self.assertIn("boom", shown[0])

    def test_should_still_show_a_single_line_observation(self):
        shown = "\n".join(m.failure_console_lines("  Observation: locator not found\n  Steps: click"))
        self.assertIn("locator not found", shown)


class CodegenOverflowMessageTests(unittest.TestCase):
    """Crossing the codegen context budget is the most expensive event in a
    run: every node after it repairs through tool mode, which on bookstack cost
    about 19 requests per node instead of one. The log has to name the side
    that actually overflowed."""

    def test_should_report_the_source_total_that_caused_the_overflow(self):
        """The old line quoted the spec size -- `exceeds one-request allowance
        (10112 spec chars)` -- while the limit is 90000 and the spec was well
        under it. The app's own sources were what had grown past the budget, so
        the number printed sent the reader after the wrong thing."""
        import tempfile
        from pathlib import Path as P
        with tempfile.TemporaryDirectory() as tmp:
            out = P(tmp)
            (out / "backend").mkdir()
            (out / "backend" / "server.js").write_text("x" * 5000, encoding="utf-8")
            (out / "frontend" / "src").mkdir(parents=True)
            (out / "frontend" / "src" / "index.html").write_text("y" * 3000, encoding="utf-8")
            self.assertEqual(m.app_source_chars(out), 8000)

    def test_should_be_zero_before_the_app_exists(self):
        import tempfile
        from pathlib import Path as P
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(m.app_source_chars(P(tmp)), 0)
class UnseenRewriteGuardTests(unittest.TestCase):
    """A codegen turn must not rewrite a file it was never shown.

    It is asked to return every file it changes, complete. When the source budget omitted
    a file, what it returns for that file is written from nothing, and writing that
    deletes behaviour other requirements depend on. Cloud 12306 99196f2e802b lost 29
    nodes to regressions and not one to a spec it could not build; the prompt had told it
    those files were "unchanged unless the requirement needs them", which invites exactly
    that. The prompt now forbids it and this guard enforces it.
    """

    def _app(self, root):
        (root / "backend").mkdir()
        (root / "frontend").mkdir()
        (root / "backend/server.js").write_text("b" * 9000, encoding="utf-8")
        (root / "frontend/a.html").write_text("a" * 9000, encoding="utf-8")
        (root / "frontend/b.html").write_text("c" * 9000, encoding="utf-8")

    def test_quoted_set_matches_what_the_prompt_quotes(self):
        import tempfile
        from pathlib import Path
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self._app(root)
            budget = 12000            # room for one file, not three
            text = m.relevant_sources(root, "a.html", budget)
            quoted = m.quoted_source_paths(root, "a.html", budget)
            for rel in quoted:
                self.assertIn(f"--- {rel} ---", text)
            for rel in ("backend/server.js", "frontend/a.html", "frontend/b.html"):
                if rel not in quoted:
                    self.assertNotIn(f"--- {rel} ---", text)
            self.assertTrue(0 < len(quoted) < 3)

    def test_omitted_files_are_forbidden_not_invited(self):
        import tempfile
        from pathlib import Path
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self._app(root)
            text = m.relevant_sources(root, "a.html", 12000)
            self.assertIn("do not return these", text)
            self.assertNotIn("unchanged unless the requirement needs them", text)

    def test_refuses_a_rewrite_of_an_unquoted_existing_file(self):
        import argparse, tempfile
        from pathlib import Path
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self._app(root)
            flow = m.Flow(argparse.Namespace(web_port=1), root, root)
            flow.pending_corrections = []
            flow.codegen_quoted = {"frontend/a.html"}
            kept = flow.drop_unseen_rewrites(
                {"frontend/a.html": "new a", "frontend/b.html": "clobbered"}, "node")
            self.assertEqual(set(kept), {"frontend/a.html"})
            self.assertTrue(any("frontend/b.html" in c for c in flow.pending_corrections))

    def test_allows_a_brand_new_file(self):
        import argparse, tempfile
        from pathlib import Path
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self._app(root)
            flow = m.Flow(argparse.Namespace(web_port=1), root, root)
            flow.pending_corrections = []
            flow.codegen_quoted = {"frontend/a.html"}
            kept = flow.drop_unseen_rewrites({"frontend/new.html": "brand new"}, "node")
            self.assertEqual(set(kept), {"frontend/new.html"})
            self.assertEqual(flow.pending_corrections, [])

    def test_does_nothing_when_the_turn_quoted_no_sources(self):
        """tiny tier, tool mode, skeleton turn: no quoted set, so no basis to refuse."""
        import argparse, tempfile
        from pathlib import Path
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self._app(root)
            flow = m.Flow(argparse.Namespace(web_port=1), root, root)
            flow.codegen_quoted = None
            files = {"frontend/b.html": "whatever"}
            self.assertEqual(flow.drop_unseen_rewrites(files, "node"), files)


class DryRunDriverParityTests(unittest.TestCase):
    """The dry-run driver has to answer every call the real driver answers.

    #140 gave the real driver `without_tools()` and called it from every codegen turn
    but did not add it to DryRunDriver, so OCTOS_ARC_DRYRUN=1 aborted at the first
    codegen node with AttributeError — the free structural-parity path from round 31 was
    broken from then until 2026-09-18. A dry run of arc-bench-web--keep now traverses
    30+ of its 32 nodes with no abort.
    """

    def test_dry_run_driver_answers_the_real_drivers_turn_surface(self):
        real = {n for n in ("run", "without_tools", "end_scope") }
        missing = [n for n in real if not hasattr(m.DryRunDriver(), n)]
        self.assertEqual(missing, [], f"DryRunDriver is missing {missing}")

    def test_without_tools_is_a_usable_context_manager(self):
        with m.DryRunDriver().without_tools():
            pass


class QuotedSetWiringTests(unittest.TestCase):
    """The guard is only as good as the set it checks against.

    Unit tests cover drop_unseen_rewrites() directly and a dry run covers that it does
    not misfire across 30 nodes, but neither covers the wiring: that the prompt-building
    path actually records which files it quoted, with the same budget the prompt used.
    If codegen_quoted were left None the guard silently does nothing; if it were computed
    with a different budget it would refuse files the model *was* shown.
    """

    def _flow_with_big_app(self, root):
        import argparse
        (root / "backend").mkdir()
        (root / "frontend").mkdir()
        # One file per 40k chars: with a 90k budget and a spec of a few hundred chars,
        # two fit and the third cannot.
        (root / "backend/server.js").write_text("b" * 40000, encoding="utf-8")
        (root / "frontend/a.html").write_text("a" * 40000, encoding="utf-8")
        (root / "frontend/z.html").write_text("z" * 40000, encoding="utf-8")
        return m.Flow(argparse.Namespace(web_port=1), root, root)

    def test_quoted_set_is_what_the_same_budget_would_quote(self):
        import tempfile
        from pathlib import Path
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            flow = self._flow_with_big_app(root)
            spec = "click the button labelled Save"
            budget = max(8000, flow.codegen_context_chars() - len(spec))
            quoted = m.quoted_source_paths(root, spec, budget)
            text = m.relevant_sources(root, spec, budget)
            self.assertTrue(quoted, "nothing quoted at a 90k budget with 120k of source")
            self.assertLess(len(quoted), 3, "all three files fit; the case under test is an omission")
            for rel in quoted:                      # everything claimed quoted really is
                self.assertIn(f"--- {rel} ---", text)
            omitted = {"backend/server.js", "frontend/a.html", "frontend/z.html"} - quoted
            for rel in omitted:                     # and everything omitted is refused by the guard
                flow.codegen_quoted = quoted
                flow.pending_corrections = []
                kept = flow.drop_unseen_rewrites({rel: "rewritten from nothing"}, "node")
                self.assertEqual(kept, {}, f"guard let through a rewrite of unquoted {rel}")

    def test_a_quoted_file_is_still_writable(self):
        """The guard must not block the file the node was given to change."""
        import tempfile
        from pathlib import Path
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            flow = self._flow_with_big_app(root)
            spec = "click the button labelled Save"
            budget = max(8000, flow.codegen_context_chars() - len(spec))
            quoted = m.quoted_source_paths(root, spec, budget)
            target = sorted(quoted)[0]
            flow.codegen_quoted = quoted
            flow.pending_corrections = []
            kept = flow.drop_unseen_rewrites({target: "legit edit"}, "node")
            self.assertEqual(set(kept), {target})
            self.assertEqual(flow.pending_corrections, [])


class RefusalFallsBackToToolModeTests(unittest.TestCase):
    """A refused rewrite must move the node to tool mode, not just drop the write.

    Refusing alone leaves the node unable to finish: it asked for a file it genuinely
    needs and got nothing, so it would keep failing its own spec. Tool mode reads and
    edits in place instead of re-emitting whole files -- which is also why it never had
    this failure mode -- so the refusal is the signal that this node belongs there.
    """

    def _flow(self, root):
        import argparse
        (root / "backend").mkdir()
        (root / "frontend").mkdir()
        (root / "frontend/seen.html").write_text("s" * 100, encoding="utf-8")
        (root / "frontend/unseen.html").write_text("u" * 100, encoding="utf-8")
        flow = m.Flow(argparse.Namespace(web_port=1), root, root)
        flow.pending_corrections = []
        flow.codegen_quoted = {"frontend/seen.html"}
        flow.codegen_blocked = False
        return flow

    def test_refusal_blocks_codegen_for_this_node(self):
        import tempfile
        from pathlib import Path
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            flow = self._flow(root)
            flow.drop_unseen_rewrites({"frontend/unseen.html": "clobber"}, "node")
            self.assertTrue(flow.codegen_blocked, "a refused node must fall back to tool mode")

    def test_an_accepted_write_leaves_the_fast_path_alone(self):
        import tempfile
        from pathlib import Path
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            flow = self._flow(root)
            flow.drop_unseen_rewrites({"frontend/seen.html": "legit"}, "node")
            self.assertFalse(flow.codegen_blocked)

    def test_a_new_file_leaves_the_fast_path_alone(self):
        import tempfile
        from pathlib import Path
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            flow = self._flow(root)
            flow.drop_unseen_rewrites({"frontend/brand-new.html": "new"}, "node")
            self.assertFalse(flow.codegen_blocked)


class CodegenQuoteCharsTests(unittest.TestCase):
    """The quoting budget had to become its own knob before it can be measured.

    `codegen_context_chars()` was doing four jobs at once, so the 6-run
    experiment that moved it (9,000 -> 2/2 three times, 40,000 -> 1/2 three
    times) moved the quoting budget, the inline budget, the whole-prompt ceiling
    and the spec-size test together. Splitting it changes nothing by default.
    """

    def _flow(self):
        import main
        return main.Flow.__new__(main.Flow)

    def test_should_default_to_the_context_budget(self):
        from unittest.mock import patch
        flow = self._flow()
        with patch.dict('os.environ', {}, clear=False):
            import os
            os.environ.pop('OCTOS_ARC_CODEGEN_QUOTE_CHARS', None)
            os.environ.pop('OCTOS_ARC_CODEGEN_CONTEXT_CHARS', None)
            self.assertEqual(flow.codegen_quote_chars(), flow.codegen_context_chars())
            self.assertEqual(flow.codegen_quote_chars(), 90000)

    def test_should_follow_the_context_budget_when_that_is_set(self):
        from unittest.mock import patch
        flow = self._flow()
        with patch.dict('os.environ', {'OCTOS_ARC_CODEGEN_CONTEXT_CHARS': '12000'}):
            import os
            os.environ.pop('OCTOS_ARC_CODEGEN_QUOTE_CHARS', None)
            self.assertEqual(flow.codegen_quote_chars(), 12000)

    def test_should_be_separable_from_the_context_budget(self):
        from unittest.mock import patch
        flow = self._flow()
        with patch.dict('os.environ', {'OCTOS_ARC_CODEGEN_CONTEXT_CHARS': '90000',
                                       'OCTOS_ARC_CODEGEN_QUOTE_CHARS': '9000'}):
            self.assertEqual(flow.codegen_context_chars(), 90000)
            self.assertEqual(flow.codegen_quote_chars(), 9000)
