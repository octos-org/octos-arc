import json
import unittest

from rust_engine import Translator, parse_event, write_runner_spec


class Calls:
    def __init__(self):
        self.calls = []

    def __getattr__(self, name):
        def record(*args, **kwargs):
            self.calls.append((name, args, kwargs))
            return []
        return record


class Runtime:
    def __init__(self):
        self.events = Calls()
        self.traceability = Calls()


class ParseTests(unittest.TestCase):
    def test_should_only_accept_prefixed_json_objects(self):
        self.assertEqual(parse_event('@@arc-event {"event": "x"}'), {"event": "x"})
        self.assertIsNone(parse_event("[flow] plain log"))
        self.assertIsNone(parse_event("@@arc-event not json"))
        self.assertIsNone(parse_event("@@arc-event [1]"))


class TranslatorTests(unittest.TestCase):
    def test_should_mirror_requirement_states_onto_aliases_and_record_tests(self):
        runtime = Runtime()
        t = Translator(runtime, lambda m: None)
        t.handle({"event": "requirement_state", "node_id": "REQ-1", "phase": "test", "status": "passed",
                  "message": "ok", "aliases": ["REQ-1.1"]})
        t.handle({"event": "test_result", "node_id": "REQ-1", "test_id": "REQ-1-increments", "title": "REQ-1: increments",
                  "file": "REQ-1.spec.ts", "ok": True, "type": "e2e"})
        t.handle({"event": "commit", "message": "m", "sha": "abc"})
        t.handle({"event": "run_completed", "message": "all requirement nodes implemented and verified"})
        names = [c[0] for c in runtime.events.calls]
        self.assertEqual(names[:2], ["mark_test_passed", "mark_test_passed"])
        self.assertEqual(runtime.events.calls[1][1], ("REQ-1.1", "ok"))
        self.assertIn("notify_commit_history_changed", names)
        self.assertEqual(runtime.traceability.calls[0][0], "list_interfaces")
        upsert = next(c for c in runtime.traceability.calls if c[0] == "upsert_test")
        self.assertEqual(upsert[2]["req_id"], "REQ-1")
        self.assertTrue(upsert[2]["passed"])
        self.assertEqual(t.final["event"], "run_completed")

    def test_should_ignore_unknown_states(self):
        runtime = Runtime()
        t = Translator(runtime, lambda m: None)
        t.handle({"event": "requirement_state", "node_id": "REQ-1", "phase": "design", "status": "weird"})
        t.handle({"event": "usage", "prompt_tokens": 1})
        self.assertEqual(runtime.events.calls, [])


class RunnerSpecTests(unittest.TestCase):
    def test_should_write_the_kernel_contract(self):
        import os, tempfile
        from pathlib import Path
        with tempfile.TemporaryDirectory() as tmp:
            os.environ["MODEL"] = "deepseek-v4-flash"
            os.environ["OPENAI_BASE_URL"] = "https://api.arc-bench.com/v1"
            path = Path(tmp) / ".arc" / "runner-spec.json"
            spec = write_runner_spec(path, req_dir=Path(tmp) / "req", output_dir=Path(tmp), web_port=3000, tests_dir=None)
            data = json.loads(path.read_text())
            self.assertEqual(data["schema_version"], 1)
            self.assertEqual(data["web_port"], 3000)
            self.assertIsNone(data["tests_dir"])
            self.assertEqual(data["model"]["model"], "deepseek-v4-flash")
            self.assertEqual(spec["model"]["api_key_env"], "OPENAI_API_KEY")
            self.assertNotIn("sk-", path.read_text())
            self.assertIsNone(data["previous_requirements"])

    def test_should_snapshot_the_template_requirement_table_before_it_is_overwritten(self):
        import tempfile
        from pathlib import Path
        from rust_engine import snapshot_previous_requirements
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            empty = snapshot_previous_requirements(out)  # no template table: an empty snapshot, never None
            self.assertEqual(json.loads(empty.read_text()), {})
            table = out / ".arc" / "traceability" / "requirements.json"
            table.parent.mkdir(parents=True)
            table.write_text(json.dumps({"REQ-1": {"id": "REQ-1", "description": "old"}, "ROOT": {"id": "ROOT"}}))
            copy = snapshot_previous_requirements(out)
            self.assertEqual(copy, out / ".arc" / "previous-requirements.json")
            self.assertEqual(json.loads(copy.read_text())["REQ-1"]["description"], "old")
            path = out / ".arc" / "runner-spec.json"
            write_runner_spec(path, req_dir=out / "req", output_dir=out, web_port=3000, tests_dir=None,
                              previous_requirements=copy)
            self.assertEqual(json.loads(path.read_text())["previous_requirements"], str(copy))


if __name__ == "__main__":
    unittest.main()


class ModelRouteCommandTests(unittest.TestCase):
    def test_routes_are_explicit_kernel_argument(self):
        from pathlib import Path
        from unittest.mock import patch, MagicMock
        from rust_engine import run_kernel
        proc = MagicMock()
        proc.stdout = []
        proc.wait.return_value = 0
        rules = '[{"model":"small","phases":["implement"]}]'
        with patch('rust_engine.subprocess.Popen', return_value=proc) as launch:
            run_kernel('octos', Path('spec.json'), Path('policy.toml'), None,
                       lambda _: None, {'OCTOS_ARC_MODEL_ROUTES': rules})
        args = launch.call_args.args[0]
        self.assertEqual(args[args.index('--model-routes-json') + 1], rules)
