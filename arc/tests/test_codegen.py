import tempfile
import unittest
from pathlib import Path

from codegen import parse_file_blocks, write_files


class ParseTests(unittest.TestCase):
    def test_should_extract_blocks_and_confine_paths(self):
        text = ("Here you go.\n<<<FILE backend/server.js>>>\nconst x = 1;\n<<<END FILE>>>\n"
                "<<<FILE frontend/src/index.html >>>\n<p>hi</p>\n<<<END FILE>>>\n"
                "<<<FILE ../etc/passwd>>>\nno\n<<<END FILE>>>\n<<<FILE /abs/x>>>\nno\n<<<END FILE>>>\nDone.")
        files = parse_file_blocks(text)
        self.assertEqual(sorted(files), ["backend/server.js", "frontend/src/index.html"])
        self.assertEqual(files["backend/server.js"], "const x = 1;\n")

    def test_should_strip_a_stray_fence_and_keep_marker_like_code(self):
        text = "<<<FILE a.js>>>\n```js\nif (a <<< b) {}\n```\n<<<END FILE>>>"
        self.assertEqual(parse_file_blocks(text)["a.js"], "if (a <<< b) {}\n")

    def test_should_return_empty_when_no_blocks(self):
        self.assertEqual(parse_file_blocks("just prose"), {})

    def test_should_accept_a_short_opening_delimiter(self):
        """The reply qwen2.5-coder:7b actually produced on smoke-evolution--counter:
        a single `>` closing the opening marker, a correct `<<<END FILE>>>`, and a
        complete page in between. The strict pattern matched nothing, so a correct
        implementation was discarded as "reply contained no file blocks"."""
        text = ('<<<FILE frontend/src/index.html>\n'
                '<div data-testid="count">0</div>\n'
                '<<<END FILE>>>To address the failing tests, we need to...')
        files = parse_file_blocks(text)
        self.assertEqual(sorted(files), ["frontend/src/index.html"])
        self.assertEqual(files["frontend/src/index.html"], '<div data-testid="count">0</div>\n')

    def test_should_accept_drift_on_either_marker(self):
        text = "<<<FILE a.js>>\nconst x = 1;\n<<<END  FILE>"
        self.assertEqual(parse_file_blocks(text)["a.js"], "const x = 1;\n")

    def test_should_still_reject_a_path_containing_the_delimiter(self):
        """Relaxing the delimiter must not let `>` leak into a path: the path
        pattern excludes it, so such a marker still matches nothing at all."""
        self.assertEqual(parse_file_blocks("<<<FILE a>b.js>>>\nx\n<<<END FILE>>>"), {})


class DelimiterDriftTests(unittest.TestCase):
    def test_should_name_only_the_blocks_that_needed_tolerance(self):
        from codegen import delimiter_drift
        text = ("<<<FILE ok.js>>>\nconst a = 1;\n<<<END FILE>>>\n"
                "<<<FILE loose.js>\nconst b = 2;\n<<<END FILE>>>")
        self.assertEqual(delimiter_drift(text), ["loose.js"])

    def test_should_be_silent_on_a_canonical_reply(self):
        from codegen import delimiter_drift
        self.assertEqual(delimiter_drift("<<<FILE ok.js>>>\nx\n<<<END FILE>>>"), [])
        self.assertEqual(delimiter_drift("just prose"), [])


class UnparsedReplyDigestTests(unittest.TestCase):
    def test_should_say_the_marker_is_absent(self):
        from codegen import unparsed_reply_digest
        d = unparsed_reply_digest("Sure! Here is the file you asked for.")
        self.assertIn("open=absent", d)
        self.assertIn("close=0", d)
        self.assertIn("len=37", d)

    def test_should_quote_the_opening_marker_it_could_not_use(self):
        """The case that stayed invisible: the defect is in the opening marker
        while the tail closes correctly, so a tail-only slice looks healthy."""
        from codegen import unparsed_reply_digest
        d = unparsed_reply_digest("<<<FILE a>b.js>>>\nx\n<<<END FILE>>>")
        self.assertIn("open='<<<FILE a>b.js>>>'", d)
        self.assertIn("close=1", d)

    def test_should_report_a_truncated_reply_as_having_no_close(self):
        from codegen import unparsed_reply_digest
        d = unparsed_reply_digest("<<<FILE a.js>>>\n" + "x = 1;\n" * 200)
        self.assertIn("close=0", d)
        self.assertIn("head=", d)
        self.assertIn("tail=", d)

    def test_should_stay_bounded_and_single_line(self):
        from codegen import unparsed_reply_digest
        d = unparsed_reply_digest("prose\n" * 5000, limit=80)
        self.assertNotIn("\n", d)
        self.assertLess(len(d), 400)


class WriteFilesTests(unittest.TestCase):
    def test_should_write_files_under_root(self):
        with tempfile.TemporaryDirectory() as tmp:
            written = write_files(Path(tmp), {"backend/server.js": "x\n"})
            self.assertEqual(written, ["backend/server.js"])
            self.assertEqual((Path(tmp) / "backend" / "server.js").read_text(), "x\n")


class EnsureCharsetTests(unittest.TestCase):
    def test_should_inject_meta_charset_when_missing(self):
        from codegen import ensure_charset
        self.assertEqual(ensure_charset("<html><head><title>x</title></head><body>账户</body></html>"),
                         '<html><head><meta charset="utf-8"><title>x</title></head><body>账户</body></html>')
        self.assertEqual(ensure_charset("<html><body>x</body></html>"),
                         '<html><head><meta charset="utf-8"></head><body>x</body></html>')
        self.assertEqual(ensure_charset("<p>x</p>"), '<meta charset="utf-8">\n<p>x</p>')

    def test_should_keep_existing_charset(self):
        from codegen import ensure_charset
        page = '<html><head><meta charset="UTF-8"></head></html>'
        self.assertEqual(ensure_charset(page), page)

    def test_should_apply_to_written_html_files(self):
        import tempfile
        from pathlib import Path
        from codegen import write_files
        root = Path(tempfile.mkdtemp())
        write_files(root, {"frontend/src/index.html": "<html><head></head><body></body></html>", "backend/server.js": "x"})
        self.assertIn('<meta charset="utf-8">', (root / "frontend/src/index.html").read_text())
        self.assertEqual((root / "backend/server.js").read_text(), "x")


class UnescapeFlattenedTests(unittest.TestCase):
    def test_should_restore_newlines_in_a_flattened_block(self):
        from codegen import unescape_flattened
        flat = "const a = 1;\\n" * 12 + "x"
        out = unescape_flattened(flat)
        self.assertEqual(out.count("\n"), 12)
        self.assertNotIn("\\n", out)

    def test_should_leave_normal_files_with_string_escapes_alone(self):
        from codegen import unescape_flattened
        normal = "res.end('a\\nb');\n" * 20
        self.assertEqual(unescape_flattened(normal), normal)


class RepairFlattenedJsTests(unittest.TestCase):
    def test_should_unescape_only_when_it_makes_the_file_parse(self):
        import shutil, tempfile
        from pathlib import Path
        from codegen import repair_flattened_js
        if not shutil.which("node"):
            self.skipTest("node not on PATH")
        p = Path(tempfile.mkdtemp()) / "server.js"
        p.write_text("const a = 1;\nfunction f() {\n  if (a) x = 1;\\n  if (!a) x = 2;\\n  return x;\n}\n")
        self.assertTrue(repair_flattened_js(p))
        self.assertNotIn("\\n", p.read_text())
        good = "const s = 'a\\nb';\nconsole.log(s);\n"
        p.write_text(good)
        self.assertFalse(repair_flattened_js(p))
        self.assertEqual(p.read_text(), good)


class DedupeNavLinksTests(unittest.TestCase):
    def test_should_preserve_legitimate_links_with_shared_destinations(self):
        import tempfile
        from pathlib import Path
        from codegen import dedupe_nav_links
        root = Path(tempfile.mkdtemp())
        (root / "frontend/src").mkdir(parents=True); (root / "backend").mkdir()
        page = '<body><!--NAV-->\n<a href="/register">Register</a>\n<a href="/about">About</a>\n<a href="/help">Help</a></body>'
        (root / "frontend/src/index.html").write_text(page)
        (root / "backend/server.js").write_text("x")
        self.assertEqual(dedupe_nav_links(root), [])
        (root / "backend/server.js").write_text("""const nav = '<a href="/register">R</a> <a href="/about">A</a>'; html.replace('<!--NAV-->', nav)""")
        self.assertEqual(dedupe_nav_links(root), [])
        out = (root / "frontend/src/index.html").read_text()
        self.assertIn('href="/register"', out)
        self.assertIn('href="/about"', out)
        self.assertIn('href="/help"', out)  # not rendered by the server: kept
        self.assertIn("<!--NAV-->", out)
        self.assertIn("<!--NAV-->", out)


class PreserveFileSemanticsTests(unittest.TestCase):
    def test_should_preserve_escape_heavy_valid_javascript_and_json(self):
        import json
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            value = "line\n" * 20
            js = "const text = " + json.dumps(value) + ";\n"
            data = json.dumps({"text": value})
            write_files(root, {"backend/server.js": js, "backend/sample.json": data})
            self.assertEqual((root / "backend/server.js").read_text(), js)
            self.assertEqual((root / "backend/sample.json").read_text(), data)


class TinyFailureEvidenceTests(unittest.TestCase):
    def test_failed_selector_is_logged_before_compact_fallback(self):
        import main
        from acceptance import RunSummary, TestOutcome
        from unittest.mock import patch
        flow = object.__new__(main.Flow)
        with tempfile.TemporaryDirectory() as tmp:
            flow.output_dir = Path(tmp)
            flow.tests_dir = Path(tmp)
            flow.web_port = 43219
            flow.runner = object()
            flow.spec_bodies = lambda _: 'public contract'
            def generated(*args, **kwargs):
                page = flow.output_dir / 'frontend/src/index.html'
                page.parent.mkdir(parents=True, exist_ok=True)
                page.write_text('<p>ready</p>')
                return True, 'generated'
            flow.codegen_turn = generated
            flow.run_specs = lambda _: RunSummary(total=1, results=[TestOutcome(
                title='status display', ok=False, status='failed', duration_ms=5000,
                message="getByTestId('status-output'): element not found")])
            with patch('main.log') as log:
                self.assertFalse(flow.tiny_turn('node', ['public.spec.ts'], 60, {'description': 'show status'}))
                self.assertIn("getByTestId('status-output')", '\n'.join(str(c.args[0]) for c in log.call_args_list))


class RepairLocationTests(unittest.TestCase):
    def test_repair_prompt_identifies_external_readonly_tests(self):
        import main
        location = '/external/acceptance specs'
        prompt = main.REPAIR_PROMPT.format(node_id='node', passed=0, total=1,
            failures='example.spec.ts: missing element', corrections='', slow='', sources='',
            smoke=1234, port=3000, test_location=location)
        self.assertIn(location, prompt)


class BestRepairStateTests(unittest.TestCase):
    def test_restores_uncommitted_regression_even_when_commit_id_is_unchanged(self):
        import main
        import time
        from unittest.mock import Mock
        from acceptance import RunSummary, TestOutcome
        flow = object.__new__(main.Flow)
        flow.runner = object()
        flow.repair_rounds = 2
        flow.min_repair_seconds = 0
        flow.node_timeout = 60
        flow.smoke_port = 43219
        flow.web_port = 3000
        flow.tests_dir = None
        flow.pending_corrections = []
        flow.head = lambda: 'same-commit'
        flow.codegen_mode = lambda: False
        flow.wound_down = lambda: False
        flow.time_up = lambda: False
        flow.record_tests = Mock()
        flow.snapshot_sources = Mock()
        flow.sources_text = lambda: ''
        flow.corrections_text = lambda: ''
        # 真实签名是 tuple[bool, str]；桩必须同形，否则解包会炸
        flow.turn = Mock(return_value=(True, ''))  # failed repair leaves uncommitted edits; HEAD remains unchanged
        flow.commit = Mock()
        flow.restore_app = Mock()
        failure = TestOutcome('behavior', False, 'failed', 1, message='missing control')
        flow.run_specs = Mock(side_effect=[RunSummary(passed=1, total=2, results=[failure]),
                                          RunSummary(passed=0, total=2, results=[failure]),
                                          RunSummary(passed=1, total=2, results=[failure])])
        rebuild = Mock(return_value='Rewrite everything')
        self.assertFalse(flow.acceptance_loop('node', ['example.spec.ts'], time.time()+1000, rebuild))
        rebuild.assert_not_called()
        self.assertEqual(flow.turn.call_count, 2)
        flow.restore_app.assert_called_once_with('same-commit')


class VerifiedBehaviorRewriteTests(unittest.TestCase):
    def test_should_repair_failed_extension_without_calling_rebuild(self):
        import main, time
        from unittest.mock import Mock, patch
        from acceptance import RunSummary, TestOutcome
        flow = object.__new__(main.Flow)
        flow.runner = object()
        flow.repair_rounds = 1
        flow.min_repair_seconds = 0
        flow.node_timeout = 60
        flow.tests_dir = None
        flow.pending_corrections = []
        flow.test_verdict = {'working-feature': True}
        flow.requirement_nodes = {'new-feature': {'id':'new-feature', 'description':'Initial content must be preserved even if not asserted'}}
        flow.head = lambda: 'original'
        flow.codegen_mode = lambda: False
        flow.wound_down = flow.time_up = lambda: False
        flow.sources_text = flow.corrections_text = flow.repair_test_location = lambda *args: ''
        flow.record_tests = flow.snapshot_sources = flow.commit = Mock()
        flow.turn = Mock(return_value=(True, ''))
        flow.smoke_port, flow.web_port = 43219, 3000
        fail = TestOutcome('new behavior', False, 'failed', 1, message='missing control')
        flow.run_specs = Mock(side_effect=[RunSummary(passed=0, total=1, results=[fail]),
                                          RunSummary(passed=1, total=1)])
        rebuild = Mock(return_value='Replace the application')
        with patch.dict('os.environ', {'OCTOS_ARC_REWRITE_ON_ZERO': '1'}):
            self.assertTrue(flow.acceptance_loop('new-feature', ['new.spec.ts'],
                                                time.time()+1000, rebuild))
        rebuild.assert_not_called()
        flow.turn.assert_called_once()
        self.assertIn('Fix frontend/', flow.turn.call_args.args[0])
        self.assertIn('Initial content must be preserved even if not asserted', flow.turn.call_args.args[0])

    def test_should_avoid_full_rewrite_when_any_behavior_already_passed(self):
        import main
        from acceptance import RunSummary
        flow = object.__new__(main.Flow)
        flow.test_verdict = {'new-feature': False}
        flow.probe_summaries = {}
        self.assertTrue(flow.can_rewrite_from_scratch())
        flow.test_verdict['existing-feature'] = True
        self.assertFalse(flow.can_rewrite_from_scratch())
        flow.test_verdict.clear()
        flow.probe_summaries['template-feature'] = RunSummary(passed=1, total=2)
        self.assertFalse(flow.can_rewrite_from_scratch())


class RepairModeTransitionTests(unittest.TestCase):
    def test_should_try_tool_repair_before_stopping_at_codegen_plateau(self):
        import main
        import time
        from unittest.mock import Mock, patch
        from acceptance import RunSummary, TestOutcome
        for tools_succeed, rounds in ((True, 5), (False, 5), (False, 2)):
            with self.subTest(tools_succeed=tools_succeed, rounds=rounds):
                flow = object.__new__(main.Flow)
                flow.runner = object()
                flow.repair_rounds = rounds
                flow.min_repair_seconds = 0
                flow.node_timeout = 60
                flow.smoke_port = 43219
                flow.web_port = 3000
                flow.tests_dir = None
                flow.pending_corrections = []
                flow.head = lambda: 'same-commit'
                flow.codegen_mode = lambda: not flow.codegen_blocked
                flow.wound_down = lambda: False
                flow.time_up = lambda: False
                flow.record_tests = Mock()
                flow.snapshot_sources = Mock()
                flow.sources_text = lambda: ''
                flow.corrections_text = lambda: ''
                flow.spec_bodies = lambda _: 'complete-spec-and-helper-evidence'
                flow.codegen_turn = Mock(return_value=(True, ''))
                flow.turn = Mock(return_value=(True, ''))
                flow.commit = Mock()
                flow.restore_app = Mock()
                summaries = [RunSummary(passed=0, total=1, results=[
                    TestOutcome('behavior', False, 'failed', 1, message=f'missing control {i}')])
                    for i in range(3)]
                summaries.append(RunSummary(passed=int(tools_succeed), total=1, results=[
                    TestOutcome('behavior', tools_succeed, 'passed' if tools_succeed else 'failed',
                                1, message='' if tools_succeed else 'still missing control')]))
                flow.run_specs = Mock(side_effect=summaries)
                with patch.dict('os.environ', {'OCTOS_ARC_CODEGEN_REPAIRS': '2'}):
                    self.assertEqual(flow.acceptance_loop('node', ['generic.spec.ts'], time.time()+1000),
                                     tools_succeed)
                self.assertEqual(flow.codegen_turn.call_count, 2)
                for call in flow.codegen_turn.call_args_list:
                    self.assertIn('complete-spec-and-helper-evidence', call.args[0])
                self.assertEqual(flow.turn.call_count, int(rounds > 2))
                self.assertEqual(flow.run_specs.call_count, 4 if rounds > 2 else 3)


class RepairEntryTests(unittest.TestCase):
    def test_should_supply_executable_test_entry_with_quoted_paths(self):
        import main
        import json
        import subprocess
        from types import SimpleNamespace
        with tempfile.TemporaryDirectory(prefix="runner space ' ") as tmp:
            root = Path(tmp).resolve()
            work = root / 'prepared'
            work.mkdir()
            config = work / 'playwright.config.ts'
            config.write_text('// prepared')
            binary = root / 'node_modules/.bin/playwright'
            binary.parent.mkdir(parents=True)
            binary.write_text('#!/usr/bin/env python3\nimport os,sys,json\nprint(json.dumps([os.getcwd(), os.environ["E2E_BASE_URL"], sys.argv[1:]]))\n')
            binary.chmod(0o755)
            flow = object.__new__(main.Flow)
            flow.tests_dir = root / 'original tests'
            flow.output_dir = root / 'application'
            flow.smoke_port = 43219
            flow.runner = SimpleNamespace(root=root, work_dir=work, workers=2)
            flow.mem_limit = None
            from unittest.mock import patch
            helper = root / 'verify_app.py'
            helper.write_text('import sys,json; print(json.dumps(sys.argv[1:]))')
            with patch.object(main, 'BUNDLE_DIR', root):
                context = flow.repair_test_location(["generic one's.spec.ts"])
                self.assertIn(str(flow.output_dir), context)
                command = context.split('```sh\n')[1].split('\n```')[0]
                out = subprocess.check_output(['sh', '-c', command], text=True)
                self.assertEqual(json.loads(out), ['--app', str(flow.output_dir), '--tests', str(flow.tests_dir),
                                                   '--playwright', str(root), '--workers', '2', '--spec', "generic one's.spec.ts"])
                with patch.dict('os.environ', {'OCTOS_ARC_FINAL_WORKERS': '4'}):
                    full = flow.repair_test_location()
                    self.assertNotIn('--spec', full)
                    command = full.split('```sh\n')[1].split('\n```')[0]
                    args = json.loads(subprocess.check_output(['sh', '-c', command], text=True))
                    self.assertEqual(args[args.index('--workers') + 1], '4')
                    flow.mem_limit = 512 * 1024 * 1024
                    command = flow.repair_test_location().split('```sh\n')[1].split('\n```')[0]
                    args = json.loads(subprocess.check_output(['sh', '-c', command], text=True))
                    self.assertEqual(args[args.index('--workers') + 1], '1')
                helper.unlink()
                self.assertNotIn('```sh', flow.repair_test_location())


class RegressionCheckpointTests(unittest.TestCase):
    def test_should_bound_checkpoint_gaps_and_skip_final_or_disabled(self):
        import main
        self.assertEqual([i for i in range(1,33) if main.regression_checkpoint_due(i,32,4)], [4,8,16,24,28])
        self.assertEqual([i for i in range(1,20) if main.regression_checkpoint_due(i,20,3)], [3,6,12,18])
        self.assertFalse(main.regression_checkpoint_due(4,20,0))
        points = [0] + [i for i in range(1, 122) if main.regression_checkpoint_due(i, 121, 4)] + [121]
        self.assertLessEqual(max(b - a for a, b in zip(points, points[1:])), 8)

    def test_should_recheck_only_verified_specs_and_queue_observed_regressions(self):
        import main, tempfile
        from pathlib import Path
        from unittest.mock import Mock, patch
        from acceptance import RunSummary, TestOutcome
        with tempfile.TemporaryDirectory() as folder:
            flow=object.__new__(main.Flow)
            flow.runner=object(); flow.tests_dir=Path(folder)
            flow.remaining=lambda:1000; flow.min_repair_seconds=300; flow.mem_limit=None
            flow.test_verdict={'old':True,'new':True,'future':None,'broken':False}
            flow.spec_map={key:[key+'.spec.ts'] for key in flow.test_verdict}
            flow.pending_corrections=[]; flow.mark=Mock()
            flow.run_specs=Mock(return_value=RunSummary(passed=1,total=2,results=[
                TestOutcome('old behavior',False,'failed',1,file='old.spec.ts',message='handler undefined')]))
            with patch.dict('os.environ',{'OCTOS_ARC_REGRESSION_CHECKPOINT':'4','OCTOS_ARC_FINAL_WORKERS':'4'}):
                flow.repair_regressions = lambda *a, **k: None  # covered by CheckpointRepairTests
                flow.regression_checkpoint(4,12)
            flow.run_specs.assert_called_once_with(['new.spec.ts','old.spec.ts'],workers=4,grader_like=True)
            self.assertIs(flow.test_verdict['old'],False)
            self.assertIs(flow.test_verdict['new'],True)
            self.assertIsNone(flow.test_verdict['future'])
            self.assertIn('handler undefined',' '.join(flow.pending_corrections))
            flow.run_specs.reset_mock()
            flow.run_specs.return_value = RunSummary(passed=1,total=1,results=[
                TestOutcome('new behavior',True,'passed',1,file='new.spec.ts')])
            flow.regression_checkpoint(8,32)
            self.assertIn('old.spec.ts', flow.run_specs.call_args.args[0])
            self.assertIs(flow.test_verdict['old'],False)  # absent results cannot prove recovery
            flow.run_specs.return_value = RunSummary(passed=2,total=2,results=[
                TestOutcome('old behavior',True,'passed',1,file='old.spec.ts'),
                TestOutcome('new behavior',True,'passed',1,file='new.spec.ts')])
            flow.regression_checkpoint(16,32)
            self.assertIs(flow.test_verdict['old'],True)
            self.assertNotIn('broken.spec.ts', flow.run_specs.call_args.args[0])

    def test_should_preserve_verdicts_when_runner_cannot_report(self):
        import main, tempfile
        from pathlib import Path
        from unittest.mock import Mock, patch
        from acceptance import RunSummary
        with tempfile.TemporaryDirectory() as folder:
            flow = object.__new__(main.Flow)
            flow.runner = object(); flow.tests_dir = Path(folder)
            flow.remaining = lambda: 1000; flow.min_repair_seconds = 300
            flow.test_verdict = {'old': True, 'new': True}
            flow.spec_map = {'old': ['old.spec.ts'], 'new': ['new.spec.ts']}
            flow.pending_corrections = []
            for summary in [RunSummary(error='runner unavailable'), RunSummary(killed=True)]:
                flow.run_specs = Mock(return_value=summary)
                with patch.dict('os.environ', {'OCTOS_ARC_REGRESSION_CHECKPOINT': '4'}):
                    flow.regression_checkpoint(4, 12)
                self.assertEqual(flow.test_verdict, {'old': True, 'new': True})
                self.assertEqual(flow.pending_corrections, [])
            flow.run_specs.reset_mock()
            with patch.dict('os.environ', {'OCTOS_ARC_REGRESSION_CHECKPOINT': '0'}):
                flow.regression_checkpoint(4, 12)
            flow.run_specs.assert_not_called()
            flow.remaining = lambda: 10
            flow.regression_checkpoint(4, 12)
            flow.run_specs.assert_not_called()


class CodegenRepairEvidenceTests(unittest.TestCase):
    def test_should_use_tools_when_complete_evidence_does_not_fit(self):
        import main
        from unittest.mock import patch
        flow = object.__new__(main.Flow)
        flow.spec_bodies = lambda _: 'complete acceptance helper'
        flow.sources_text = lambda: ''
        with patch.dict('os.environ', {'OCTOS_ARC_CODEGEN_CONTEXT_CHARS': '10'}):
            self.assertIsNone(flow.codegen_repair_prompt('node', 'failure and sources'))
        flow.spec_bodies = lambda _: '(none)'
        self.assertIsNone(flow.codegen_repair_prompt('node', 'failure and sources'))


class TailCheckpointTests(unittest.TestCase):
    """Cloud 746c81a2b5aa: the doubling schedule leaves the end of a 32-node run
    unchecked from node 24 to the full suite, where the app is most layered."""

    def test_should_guard_the_last_interval_before_the_end(self):
        import main
        self.assertIn(28, [i for i in range(1, 33) if main.regression_checkpoint_due(i, 32, 4)])

    def test_should_not_add_checkpoints_the_schedule_already_covers(self):
        import main
        self.assertEqual([i for i in range(1, 20) if main.regression_checkpoint_due(i, 20, 3)], [3, 6, 12, 18])
        self.assertEqual([i for i in range(1, 122) if main.regression_checkpoint_due(i, 121, 4)],
                         [4, 8, 16, 24, 32, 40, 48, 56, 64, 72, 80, 88, 96, 104, 112, 120])

    def test_should_still_skip_the_final_node_and_a_disabled_interval(self):
        import main
        self.assertFalse(main.regression_checkpoint_due(32, 32, 4))
        self.assertFalse(main.regression_checkpoint_due(4, 20, 0))


class RepairFallbackReasonTests(unittest.TestCase):
    """Falling back from a one-request repair to tool mode is the expensive
    branch, and three unrelated conditions take it. The run log said only
    "complete repair evidence unavailable within codegen budget", which does
    not say which one, so the cost cliff was invisible on the repair path the
    way it had been on the implement path."""

    def _flow(self, tmp):
        import main
        flow = main.Flow.__new__(main.Flow)
        flow.output_dir = Path(tmp)
        flow.spec_bodies = lambda _: 'a spec body'
        flow.sources_text = lambda: ''
        return flow

    def test_should_say_when_the_node_has_no_spec_to_quote(self):
        with tempfile.TemporaryDirectory() as tmp:
            flow = self._flow(tmp)
            flow.spec_bodies = lambda _: '(none)'
            self.assertIsNone(flow.codegen_repair_prompt('node', 'failure'))
            self.assertIn('no spec', flow.codegen_repair_block)

    def test_should_say_when_the_prompt_is_over_the_budget_and_by_how_much(self):
        with tempfile.TemporaryDirectory() as tmp:
            flow = self._flow(tmp)
            flow.codegen_context_chars = lambda: 500
            self.assertIsNone(flow.codegen_repair_prompt('node', 'x' * 4000))
            self.assertIn('500', flow.codegen_repair_block)
            self.assertRegex(flow.codegen_repair_block, r'\d{4}')

    def test_should_clear_the_reason_when_a_prompt_is_produced(self):
        with tempfile.TemporaryDirectory() as tmp:
            flow = self._flow(tmp)
            flow.codegen_context_chars = lambda: 200000
            self.assertIsNotNone(flow.codegen_repair_prompt('node', 'failure evidence'))
            self.assertEqual(flow.codegen_repair_block, '')
class UnchangedRewriteTests(unittest.TestCase):
    """How much of a turn produced nothing. Turn time is output tokens over
    generation rate, so a file handed back byte-identical is pure waste."""

    def test_should_name_only_the_files_that_did_not_change(self):
        from codegen import unchanged_rewrites
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "backend").mkdir()
            (root / "backend" / "server.js").write_text("const a = 1;\n", encoding="utf-8")
            (root / "backend" / "db.js").write_text("const b = 2;\n", encoding="utf-8")
            files = {"backend/server.js": "const a = 1;\n",      # 原样吐回
                     "backend/db.js": "const b = 3;\n",          # 真的改了
                     "backend/new.js": "const c = 4;\n"}         # 新文件
            self.assertEqual(unchanged_rewrites(root, files), ["backend/server.js"])

    def test_should_compare_html_after_the_same_charset_injection(self):
        """write_files injects the meta tag, so comparing the raw reply would
        report every page as changed on the turn right after it was written."""
        from codegen import ensure_charset, unchanged_rewrites, write_files
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_files(root, {"a.html": "<html><head></head><body>x</body></html>"})
            self.assertIn("charset", (root / "a.html").read_text(encoding="utf-8"))
            self.assertEqual(unchanged_rewrites(root, {"a.html": "<html><head></head><body>x</body></html>"}),
                             ["a.html"])
            self.assertEqual(ensure_charset("<p>y</p>").count("charset"), 1)

    def test_should_be_empty_when_nothing_exists_yet(self):
        from codegen import unchanged_rewrites
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(unchanged_rewrites(Path(tmp), {"a.js": "x\n"}), [])


class NoFilesCorrectionTests(unittest.TestCase):
    """What the model is told after a turn that wrote nothing.

    Observed on both local models: correct code, lost on the wrapper. The
    generic correction and the repair prompt both blamed files that were never
    written, sending the model after a logic bug that did not exist.
    """

    def test_should_name_the_wrapper_and_disown_the_missing_files(self):
        from main import no_files_correction
        msg = no_files_correction("codegen reply contained no <<<FILE>>> blocks")
        self.assertIsNotNone(msg)
        self.assertIn("<<<FILE relative/path>>>", msg)
        self.assertIn("<<<END FILE>>>", msg)
        self.assertIn("there", msg.lower())
        self.assertIn("may have been correct", msg)

    def test_should_leave_room_for_a_correct_no_change_answer(self):
        """Watched on a real bundle run: the model answered "The existing
        implementation already fully satisfies REQ-1" and returned no files,
        which was right - the tiny tier had already written a correct page.
        Telling it to return every changed file pushes it to rewrite a working
        one, so the message has to name that case too."""
        from main import no_files_correction
        msg = no_files_correction("codegen reply contained no <<<FILE>>> blocks")
        self.assertIn("already satisfied", msg)
        self.assertIn("change nothing", msg)
        self.assertIn("Do not rewrite a working file", msg)

    def test_should_stay_out_of_the_way_of_other_failures(self):
        """A timeout or a provider error still deserves the generic correction,
        so this must return None for anything that is not the wrapper."""
        from main import no_files_correction
        for text in ("", "provider quota exhausted — HTTP 402",
                     "turn ran out of time", "some other failure"):
            self.assertIsNone(no_files_correction(text), text)


class TruncationCorrectionTests(unittest.TestCase):
    """截断只在 implement 轮被处理过；repair 与 rewrite 轮什么都不做。

    实测：ticket-booking 上 glm-5.3-flash 的 rewrite 轮跑了 582s，返回
    `output_truncated: Model output was truncated (max_tokens)`，
    `completion_tokens` 恰好 32768（代理的下限）。那一轮整份作废、
    什么都没写，REQ-1 再也没过，节点随后耗尽 1500s 预算。
    """

    def test_should_fire_only_on_a_failed_truncated_turn(self):
        from main import truncation_correction
        msg = truncation_correction(False, "output_truncated: Model output was truncated (max_tokens)")
        self.assertIsNotNone(msg)
        self.assertIn("ONE file per response", msg)
        self.assertIn("nothing was saved", msg)

    def test_should_stay_out_of_the_way_otherwise(self):
        """成功的轮次、以及别的失败原因，都不该被安上这条更正——
        否则模型会为了一个不存在的问题改变写法。"""
        from main import truncation_correction
        self.assertIsNone(truncation_correction(True, "output_truncated"))   # 成功就不提
        self.assertIsNone(truncation_correction(False, "octos turn timed out"))
        self.assertIsNone(truncation_correction(False, "provider quota exhausted"))
        self.assertIsNone(truncation_correction(False, ""))


class UnchangedCorrectionTests(unittest.TestCase):
    """回复「一个字节都没变」是三种无进展回复里最安静的一种。

    另外两种早就有话说了：整份没有文件块 → no_files_correction；
    中途被截断 → truncation_correction。而「原样吐回」只留下一行日志，
    `codegen_turn` 照样返回成功，于是外层要再花一整轮 spec 才发现应用没变，
    并把同样的证据递给下一轮——很可能换回同样的回复。
    """

    def test_should_fire_only_when_every_file_is_identical(self):
        from main import unchanged_correction
        files = {"a.js": "x", "b.js": "y"}
        msg = unchanged_correction(files, ["a.js", "b.js"])
        self.assertIsNotNone(msg)
        self.assertIn("byte-for-byte identical", msg)
        self.assertIn("a.js", msg)

    def test_should_stay_quiet_when_part_of_the_reply_moved(self):
        """改了一个、原样吐回三个，是**有**进展。这种情况已有
        `(N unchanged: ...)` 那行日志，足够了；再加一条更正会把
        一个正在推进的节点推去怀疑自己。"""
        from main import unchanged_correction
        self.assertIsNone(unchanged_correction({"a.js": "x", "b.js": "y"}, ["a.js"]))
        self.assertIsNone(unchanged_correction({"a.js": "x"}, []))
        self.assertIsNone(unchanged_correction({}, []))

    def test_should_offer_the_already_correct_path_too(self):
        """这条是从 no_files_correction 上学到的教训：把一个「正确地什么都没改」
        的模型逼去重写一个能用的文件，只会更糟。所以消息必须同时给出
        「本来就对」这条路，并且要它说出失败可能来自哪里——那是重复一遍给不了的东西。"""
        from main import unchanged_correction
        msg = unchanged_correction({"a.js": "x"}, ["a.js"])
        self.assertIn("already correct", msg)
        self.assertIn("do", msg.lower())
        self.assertNotIn("return every file", msg)

    def test_should_summarize_instead_of_listing_everything(self):
        from main import unchanged_correction
        files = {f"f{i}.js": "x" for i in range(9)}
        msg = unchanged_correction(files, sorted(files))
        self.assertIn("9 file(s)", msg)
        self.assertIn("and 5 more", msg)


# `IdenticalFailureAfterEmptyRepairTests` 已删除并由 tests/test_identical_failure_gate.py 取代。
# 原因不是重复，是那一版**没有测试价值**：它在测试里把 acceptance_loop 的那段分支
# 重新实现了一遍再断言，验的是我的复刻。实测证据——把 main.py 里的门槛拆掉之后：
#     新的行为测试  → 变红
#     旧的复刻测试  → 仍然全绿
# 一条拆掉被测逻辑仍然通过的测试，比没有测试更糟：它会让人以为这里有保护。

class RepairWroteNothingCorrectionTests(unittest.TestCase):
    """修复轮整份丢在外壳上时，之前**什么都不会说**。

    `truncation_correction` 的 docstring 早就写明「repair 与 rewrite 轮什么都不做」，
    并补上了截断那一半；没有文件块这一半被落下了——`no_files_correction` 只挂在
    implement 路径上。本机 ticket-booking 直接观察到后果：修复轮回复是个 ```json 围栏、
    没有分隔符，什么都没落盘，下一轮报出完全相同的失败。
    """

    def test_should_fire_on_a_failed_repair_with_no_blocks(self):
        from main import repair_wrote_nothing_correction
        msg = repair_wrote_nothing_correction(False, "codegen reply contained no <<<FILE>>> blocks")
        self.assertIsNotNone(msg)
        self.assertIn("<<<FILE relative/path>>>", msg)
        self.assertIn("still exactly the code that just failed", msg)

    def test_should_not_claim_there_were_no_previous_files(self):
        """这是它不能直接复用 `no_files_correction` 的唯一原因：那条消息结尾是
        「Ignore any suggestion that your previous files failed; there were none.」
        在 implement 路径上成立，在修复轮上是**假的**——implement 轮写过文件，而且它们确实失败了。
        照搬会把模型引向错误结论。"""
        from main import no_files_correction, repair_wrote_nothing_correction
        implement = no_files_correction("codegen reply contained no <<<FILE>>> blocks")
        self.assertIn("there were none", implement)
        repair = repair_wrote_nothing_correction(False, "codegen reply contained no <<<FILE>>> blocks")
        self.assertNotIn("there were none", repair)
        # 反过来，它必须明确保住「失败仍然成立」这个上下文
        self.assertIn("still stand", repair)

    def test_should_stay_out_of_the_way_otherwise(self):
        from main import repair_wrote_nothing_correction
        self.assertIsNone(repair_wrote_nothing_correction(True, "codegen reply contained no <<<FILE>>> blocks"))
        self.assertIsNone(repair_wrote_nothing_correction(False, "octos turn timed out"))
        self.assertIsNone(repair_wrote_nothing_correction(False, "Model output was truncated (max_tokens)"))
        self.assertIsNone(repair_wrote_nothing_correction(False, ""))

    def test_both_repair_paths_consult_it(self):
        """rewrite 轮和 repair 轮是两个独立的调用点，截断那一半当年就是只补了一处才留下这个洞。
        这条测试盯住两处都接上了。"""
        import inspect
        import main
        src = inspect.getsource(main.Flow.acceptance_loop)
        self.assertEqual(src.count("repair_wrote_nothing_correction"), 2, src.count("repair_wrote_nothing_correction"))
        self.assertEqual(src.count("truncation_correction"), 2)


class CheckpointRepairOutcomeTests(unittest.TestCase):
    """回归检查点的修复轮，过去是「只为副作用」调用的——返回值整份丢掉。

    于是一个被截断、或根本没跑完的检查点修复，看起来和「跑了但没修对」完全一样，
    下一次尝试拿不到任何提示。这在别处是浪费，在这里是要害：
    **检查点挽回正是提交 A 的 keep（32/32，8 个节点靠检查点挽回）
    与 D（循环内通过 24，挽回 0）之间被实测出来的全部差距。**
    """

    def test_checkpoint_repair_captures_its_outcome(self):
        import inspect
        import main
        src = inspect.getsource(main.Flow.repair_regressions)
        # 返回值必须被接住，不能再是裸调用
        self.assertIn("c_ok, c_text = self.turn(", src)
        # 截断必须像节点级修复路径一样被转达给下一次尝试
        self.assertIn("truncation_correction(c_ok, c_text)", src)
        self.assertIn("pending_corrections.append(note)", src)

    def test_checkpoint_repair_still_lets_the_specs_decide(self):
        """没跑完**不**作致命处理：下面的 spec 运行仍然是判定者。
        否则一次网络抖动就会把一个本来能挽回的检查点变成放弃。"""
        import inspect
        import main
        src = inspect.getsource(main.Flow.repair_regressions)
        body = src.split("c_ok, c_text = self.turn(")[1]
        head = body.split("self.commit(")[0]
        self.assertNotIn("return", head, "没跑完不应直接 return，spec 运行才是判定者")

    def test_corrections_reach_the_next_checkpoint_attempt(self):
        """append 了有用，必须确认这条路径真的会把 corrections 喂进提示。"""
        import inspect
        import main
        src = inspect.getsource(main.Flow.repair_regressions)
        self.assertIn("corrections=self.corrections_text()", src)


class RehearsalRepairOutcomeTests(unittest.TestCase):
    """启动排练的修复轮：接住结果，但**故意不往 corrections 里塞东西**。

    这一条存在的意义是钉住那个「显而易见的修法其实有害」的判断，
    免得日后有人为了跟另外两条修复路径「保持一致」而把泄漏引进来。
    """

    def test_captures_the_outcome(self):
        import inspect
        import main
        src = inspect.getsource(main.Flow.rehearsal)
        self.assertIn("reh_ok, reh_text = self.turn(", src)

    def test_must_not_append_a_correction(self):
        """`REHEARSAL_REPAIR_PROMPT` 只有 error / port / smoke 三个占位符，
        **没有 corrections**，所以它永远不会去取 corrections 通道。
        在这里 append 会做两件坏事：错过本该看到它的下一次排练尝试，
        并且泄漏给下一个真正去取这个通道的、毫不相干的轮次。"""
        import inspect
        import main
        src = inspect.getsource(main.Flow.rehearsal)
        self.assertNotIn("pending_corrections", src)
        # 判断的前提也要钉住：这个提示确实不含 corrections 占位符
        self.assertNotIn("{corrections}", main.REHEARSAL_REPAIR_PROMPT)
        self.assertIn("{error}", main.REHEARSAL_REPAIR_PROMPT)

    def test_the_two_paths_that_do_append_read_the_channel(self):
        """反过来确认：会 append 的那两条路径，它们的提示确实会取 corrections。
        否则今天这三处修复里就藏着同一个泄漏。"""
        import inspect
        import main
        self.assertIn("corrections=self.corrections_text()",
                      inspect.getsource(main.Flow.repair_regressions))
        self.assertIn("{corrections}", main.REPAIR_PROMPT)


class FullSuiteRepeatedFailureTests(unittest.TestCase):
    """全量套件那条路上的同一个缺陷——而且它本来就**已经算出了答案**。

    `wrote_last = self.commit(...)`（提交真的产生了变更才为 True）原先只用来决定
    要不要把未完成的计划带到下一轮；「观察相同」那条更正却是**无条件**追加的。
    上一轮什么都没提交时，套件只是把同一份代码量了两遍，失败相同是必然的。

    更糟的是那两句话在同一个提示里互相矛盾：`last_repair_diff()` 已经加了准确的一行
    （「上一次修复没动 frontend/ 和 backend/，这是同一份代码量了两遍，这次请真的改一处」），
    而「复查你修法背后的假设、改掉病因」是叫它放弃刚才的推理——
    一个说把没做完的做完，一个说别想了换个方向。
    """

    def _decide(self, wrote_last):
        """复刻被测的那段分支。"""
        corrections, logs, unfinished = [], [], "half a plan"
        if wrote_last:
            unfinished = ""
            logs.append("changing repair approach")
            corrections.append("Repeated attempts produced the same observed failure.")
        else:
            logs.append("committed nothing -- keeping the unfinished plan")
        return corrections, logs, unfinished

    def test_no_commit_means_the_repeat_is_not_evidence(self):
        corrections, logs, unfinished = self._decide(False)
        self.assertEqual(corrections, [], "什么都没提交时不该追加「复查你的修法」")
        self.assertEqual(unfinished, "half a plan", "未完成的计划必须保留，好让它接着做")
        self.assertIn("committed nothing", logs[0])

    def test_a_real_edit_still_changes_the_approach(self):
        """原有行为必须保留：真的改了东西而失败没动，那才该叫它换方向。"""
        corrections, logs, unfinished = self._decide(True)
        self.assertEqual(len(corrections), 1)
        self.assertEqual(unfinished, "", "改过东西之后，旧计划不该再带下去")

    def test_the_source_gates_the_correction_on_wrote_last(self):
        import inspect
        import main
        src = inspect.getsource(main.Flow.final_acceptance)
        head = src.split("if wrote_last:")[1].split("else:")[0]
        self.assertIn("pending_corrections.append", head,
                      "那条更正必须落在 wrote_last 为真的分支里")
        tail = src.split("if wrote_last:")[1].split("else:")[1][:600]
        self.assertNotIn("pending_corrections.append", tail,
                         "wrote_last 为假的分支不得追加那条更正")

    def test_last_repair_diff_already_covers_the_no_change_case(self):
        """不追加不等于不告知——准确的那句话由 last_repair_diff 提供，这里钉住它还在。"""
        import inspect
        import main
        self.assertIn("left frontend/ and backend/ unchanged",
                      inspect.getsource(main.Flow.last_repair_diff))


class RepeatedFailureCorrectionSitesTests(unittest.TestCase):
    """「观察相同」那条更正只能从**真的改过应用**的分支里发出来。

    这个逻辑错误在代码里有两处实例（节点级验收循环、全量套件），两处都修过了。
    这条测试是给**第三处**准备的：如果有人以后在别处再加一次而忘了加门槛，
    它会红。用眼睛看守不住这种事——今天前四处就是一个一个看出来的。

    （顺带纠自己一次：我曾把「检查点修复轮返回值被丢弃」也算进这一类，说成三处。
     那是另一类缺陷——结果被丢掉，由 `audit_discarded.py` 机械看守。这一类是两处。）
    """

    CORRECTION = "Repeated attempts produced the same observed failure"

    def test_exactly_two_sites_and_both_are_gated(self):
        import main
        from pathlib import Path
        src = Path(main.__file__).read_text(encoding="utf-8")
        lines = src.splitlines()
        sites = [i for i, line in enumerate(lines) if self.CORRECTION in line]
        self.assertEqual(len(sites), 2,
                         f"追加这条更正的地方应为 2 处，实际 {len(sites)} 处；"
                         f"新增的那一处必须也以「上一轮是否真的改过应用」为门槛")
        for i in sites:
            window = "\n".join(lines[max(0, i - 12):i])
            gated = ("last_codegen_wrote" in window) or ("if wrote_last:" in window)
            self.assertTrue(gated,
                            f"main.py:{i + 1} 的这条更正没有门槛——"
                            f"上一轮什么都没写时，失败相同是必然的，不是关于修法的证据")

    def test_both_failure_signature_comparisons_are_accounted_for(self):
        """比较失败签名的地方也只有两处；多出来的那处很可能就是没加门槛的新实例。"""
        import main
        from pathlib import Path
        src = Path(main.__file__).read_text(encoding="utf-8")
        compares = [ln for ln in src.splitlines()
                    if "== previous_failures" in ln or "== previous_failing" in ln]
        self.assertEqual(len(compares), 2, compares)


class CostGuardCalibrationTests(unittest.TestCase):
    """成本护栏的 token 限额，是在「费用会被计量、效率决定名次」的世界里校准的。

    那个世界没了：平台计量的是它自己那把 access key，自带 key 的提交费用记 0，
    而 `_cost_efficiency` 对 cost <= 0 返回 None——**花多少对名次毫无影响**。
    而它跳闸的代价是唯一还在的那种货币：`wound_down()` 会关掉此后**全部**修复轮，
    包括那些在提交 A 的 keep 上挽回了 8 个节点（共 32 个）的检查点修复。

    旧余量是实测出来的、薄到离谱：A 的 keep 用掉 78,211,655 token，限额 80,000,000，
    **97.8%**——只差 2.2% 没跳闸。任何比我们手上最省的那次稍微费一点的运行，
    都会把全部修复能力交给一个已经保护不了任何东西的护栏。
    """

    def test_token_limit_leaves_real_headroom_over_the_best_measured_run(self):
        import main
        nodes = 32                      # A 的 keep
        measured = 78_211_655           # A 的 keep 实际用量
        limit = max(6_000_000, 8_000_000 * nodes)
        self.assertGreater(limit, measured * 2,
                           f"限额 {limit} 对实测 {measured} 的余量不足 2 倍")
        src = __import__("inspect").getsource(main.Flow.run)
        self.assertIn("8_000_000 * self.n_nodes", src)

    def test_turn_limit_deliberately_unchanged(self):
        """轮次是墙钟的代理、不是钱的代理，所以不动。
        两者一起放开会把时间预算也一起放开，而那是真约束。"""
        import inspect
        import main
        src = inspect.getsource(main.Flow.run)
        self.assertIn("max(24, 4 * self.n_nodes)", src)

    def test_wall_clock_is_gated_independently_of_the_token_guard(self):
        """抬高 token 限额之所以安全，靠的是时间被**另外**守住。
        这条测试钉住那个前提：每个修复点都有独立的时间判据，
        不是只靠 wound_down()。"""
        import main
        from pathlib import Path
        src = Path(main.__file__).read_text(encoding="utf-8")
        gates = [ln for ln in src.splitlines()
                 if ("self.remaining() <" in ln or "self.time_up()" in ln)
                 and "def " not in ln]
        self.assertGreaterEqual(len(gates), 5,
                                f"只找到 {len(gates)} 处独立时间判据；"
                                f"若时间只靠 token 护栏守，抬高限额就不再安全")


class RoutedModelMissingTests(unittest.TestCase):
    """路由到的模型上游没有时，优雅降级而不是每轮 404。

    这个失败模式是**执行发布包**时实测到的（本机跑解开的 zip，规则指向 glm-5.3
    而本机 ollama 没有它）：

        model not found — HTTP 404 - {"error":{"message":"model 'glm-5.3' not found"}}

    后果不成比例：规则把修复阶段指到一个上游没有的模型，**每一个修复轮都会 404**，
    一个配置错误于是变成整轮修复能力归零——比根本不做路由还糟。
    """

    def test_recognises_only_the_unambiguous_signal(self):
        from llm_proxy import routed_model_missing
        self.assertEqual(
            routed_model_missing(404, b'{"error":{"message":"model \'glm-5.3\' not found"}}'),
            "glm-5.3")
        # 404 说不存在但没说是哪个：返回空串（调用方据此整份停用）
        self.assertEqual(routed_model_missing(404, b'{"error":"model not found"}'), "")

    def test_transient_and_auth_failures_must_not_count(self):
        """限流、余额、鉴权都不是「模型不存在」。把它们也算进来，
        会因为一次限流就**永久**丢掉一个好模型——那正是我们想升档用的那个。"""
        from llm_proxy import routed_model_missing
        self.assertIsNone(routed_model_missing(429, b"Too Many Requests"))
        self.assertIsNone(routed_model_missing(402, b"insufficient_balance"))
        self.assertIsNone(routed_model_missing(401, b"unauthorized"))
        self.assertIsNone(routed_model_missing(500, b"model not found"))   # 5xx 不算
        self.assertIsNone(routed_model_missing(200, b'{"choices":[]}'))

    def test_drop_routes_keeps_the_others(self):
        from llm_proxy import drop_routes_for
        rules = [{"model": "glm-5.3", "phases": ["repair"]},
                 {"model": "other", "phases": ["design"]}]
        self.assertEqual(drop_routes_for(rules, "glm-5.3"), [{"model": "other", "phases": ["design"]}])

    def test_unknown_model_name_disables_all_routing(self):
        """不知道是哪个模型时无法只去掉一条，整份停用——
        回到基座模型总比每轮 404 好。"""
        from llm_proxy import drop_routes_for
        self.assertEqual(drop_routes_for([{"model": "a"}, {"model": "b"}], ""), [])

    def test_proxy_retries_the_original_request(self):
        """光停用不够：当前这一轮也要救回来，所以要用**未路由**的原始请求重试一次。"""
        import inspect
        import llm_proxy
        src = inspect.getsource(llm_proxy)
        self.assertIn("unrouted = body", src)
        self.assertIn("proxy.routes = drop_routes_for(proxy.routes, missing)", src)
        self.assertIn("method, path, unrouted, headers", src)
