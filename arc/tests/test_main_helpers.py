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
        self.assertIn("do not output them", text)
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
        self.assertIn("mkdirSync('dist',{recursive:true})", fe["scripts"]["build"])
        # the build script must run and emit both register.html and extensionless register
        import shutil, subprocess
        node = shutil.which("node") or "/opt/homebrew/opt/node@24/bin/node"
        (root / "frontend/src").mkdir(parents=True)
        (root / "frontend/src/index.html").write_text("i"); (root / "frontend/src/register.html").write_text("r")
        cmd = fe["scripts"]["build"][len("node -e "):].strip('"').replace('\\"', '"')
        subprocess.run([node, "-e", cmd], cwd=root / "frontend", check=True)
        self.assertEqual((root / "frontend/dist/register").read_text(), "r")
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
