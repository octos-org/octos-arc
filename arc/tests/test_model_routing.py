import json
import unittest
from llm_proxy import model_routes, route_request


class RoutingTests(unittest.TestCase):
    def test_shared_python_rust_contract(self):
        from pathlib import Path
        cases = json.loads((Path(__file__).parent / 'fixtures/model-routing.json').read_text())
        for case in cases:
            with self.subTest(case=case['name']):
                body = json.dumps(case['body'], ensure_ascii=False).encode()
                result = route_request(body, model_routes(json.dumps(case['rules'])), case['phase'])
                self.assertEqual(json.loads(result)['model'], case['model'])
                if case['model'] == 'original':
                    self.assertEqual(result, body)

    def setUp(self):
        self.rules = model_routes(json.dumps([
            {'model': 'small-model', 'phases': ['implement'], 'max_input_chars': 1000, 'tools': False},
            {'model': 'repair-model', 'phases': ['repair'], 'parameters': {'temperature': 0.2}},
        ]))

    def request(self, text='hello', tools=False):
        d = {'model': 'original', 'messages': [{'role': 'user', 'content': text}], 'thinking': {'type': 'enabled'}}
        if tools:
            d['tools'] = [{'type': 'function', 'function': {'name': 'read'}}]
        return json.dumps(d).encode()

    def test_same_task_can_use_different_models_by_phase(self):
        first = json.loads(route_request(self.request(), self.rules, 'implement'))
        repair = json.loads(route_request(self.request(), self.rules, 'repair'))
        self.assertEqual(first['model'], 'small-model')
        self.assertEqual(repair['model'], 'repair-model')
        self.assertNotIn('thinking', first)
        self.assertEqual(repair['temperature'], 0.2)

    def test_tools_and_large_context_keep_original_without_eligible_route(self):
        for body in (self.request(tools=True), self.request('x' * 1001)):
            self.assertEqual(route_request(body, self.rules, 'implement'), body)

    def test_no_config_preserves_request_exactly(self):
        body = self.request()
        self.assertEqual(route_request(body, [], 'implement'), body)

    def test_reject_invalid_rules_and_prompt_replacement(self):
        for rule in [{'model': ''}, {'model': 'm', 'max_input_chars': 0},
                     {'model': 'm', 'parameters': {'messages': []}},
                     {'model': 'm', 'phases': ['some-task-name']}]:
            with self.assertRaises(ValueError):
                model_routes(json.dumps([rule]))

    def test_image_and_tool_history_require_declared_capabilities(self):
        for message in ({'role': 'user', 'content': [{'type': 'image_url', 'image_url': {'url': 'data:image/png;base64,x'}}]},
                        {'role': 'tool', 'tool_call_id': 'id', 'content': 'result'}):
            body = json.dumps({'model': 'original', 'messages': [message]}).encode()
            self.assertEqual(route_request(body, self.rules, 'implement'), body)

    def test_proxy_routes_real_http_requests_and_records_actual_models(self):
        import tempfile
        import threading
        import urllib.request
        from pathlib import Path
        from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
        from unittest.mock import patch
        from llm_proxy import LlmProxy
        received = []
        class Provider(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass
            def do_POST(self):
                received.append(json.loads(self.rfile.read(int(self.headers['Content-Length']))))
                payload = json.dumps({'choices': [{'message': {'role': 'assistant', 'content': 'ok'}}], 'usage': {'prompt_tokens': 3, 'completion_tokens': 1}}).encode()
                self.send_response(200)
                self.send_header('Content-Length', str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)
        upstream = ThreadingHTTPServer(('127.0.0.1', 0), Provider)
        worker = threading.Thread(target=upstream.serve_forever, daemon=True)
        worker.start()
        try:
            with tempfile.TemporaryDirectory() as tmp, patch.dict('os.environ', {'OCTOS_ARC_MODEL_ROUTES': json.dumps(self.rules)}):
                log = Path(tmp) / 'usage.jsonl'
                proxy = LlmProxy(f'http://127.0.0.1:{upstream.server_port}/v1', 'low', log_path=log, trim=False).start()
                client = urllib.request.build_opener(urllib.request.ProxyHandler({}))
                try:
                    for phase in ('implement', 'repair'):
                        proxy.phase = phase
                        request = urllib.request.Request(proxy.base_url + '/chat/completions', data=self.request(), headers={'Content-Type': 'application/json'})
                        # The client must bypass the system proxy, for the same reason
                        # llm_proxy.open_upstream does: urllib honours the macOS system
                        # proxy settings (getproxies() returns 127.0.0.1:1082 here with no
                        # env var set), and that proxy refuses to forward to loopback, so
                        # this call died with RemoteDisconnected and the failure was being
                        # written off as an unfixable environment quirk.
                        with client.open(request, timeout=5) as response:
                            self.assertEqual(response.status, 200)
                finally:
                    proxy.stop()
                entries = [json.loads(line) for line in log.read_text().splitlines()]
                self.assertEqual([x['model'] for x in received], ['small-model', 'repair-model'])
                self.assertEqual([x['model'] for x in entries], ['small-model', 'repair-model'])
                self.assertEqual([x['phase'] for x in entries], ['implement', 'repair'])
        finally:
            upstream.shutdown()
            upstream.server_close()
            worker.join()


class BundledRoutingTests(unittest.TestCase):
    def test_bundle_config_and_explicit_environment_override(self):
        import tempfile
        from pathlib import Path
        from llm_proxy import configured_model_routes
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.assertEqual(configured_model_routes({}, root), '')
            root.joinpath('model-routes.json').write_text('[{"model":"bundled"}]')
            self.assertEqual(json.loads(configured_model_routes({}, root))[0]['model'], 'bundled')
            self.assertEqual(configured_model_routes({'OCTOS_ARC_MODEL_ROUTES': ''}, root), '')
            explicit = '[{"model":"override"}]'
            self.assertEqual(configured_model_routes({'OCTOS_ARC_MODEL_ROUTES': explicit}, root), explicit)
            root.joinpath('model-routes.json').write_text('[{"model":"m","parameters":{"messages":[]}}]')
            with self.assertRaises(ValueError):
                configured_model_routes({}, root)


class RoutingStartupTests(unittest.TestCase):
    def test_passthrough_reasoning_does_not_disable_configured_routing(self):
        import main
        import tempfile
        from pathlib import Path
        from unittest.mock import patch
        flow = object.__new__(main.Flow)
        with tempfile.TemporaryDirectory() as tmp:
            flow.output_dir = Path(tmp)
            env = {'OPENAI_BASE_URL': 'http://localhost/v1', 'OCTOS_ARC_REASONING': 'passthrough',
                   'OCTOS_ARC_MODEL_ROUTES': '[{"model":"configured"}]'}
            with patch.dict('os.environ', env), patch('main.LlmProxy') as proxy:
                proxy.return_value.start.return_value.base_url = 'http://localhost:1234/v1'
                flow.start_llm_proxy()
                proxy.assert_called_once()

    def test_configured_routes_do_not_silently_fall_back_after_proxy_failure(self):
        import main
        import tempfile
        from pathlib import Path
        from unittest.mock import patch
        flow = object.__new__(main.Flow)
        with tempfile.TemporaryDirectory() as tmp:
            flow.output_dir = Path(tmp)
            env = {'OPENAI_BASE_URL': 'http://localhost/v1', 'OCTOS_ARC_MODEL_ROUTES': '[{"model":"configured"}]'}
            with patch.dict('os.environ', env), patch('main.LlmProxy', side_effect=OSError('bind failed')):
                with self.assertRaises(OSError):
                    flow.start_llm_proxy()


class ShippedEscalationRouteTests(unittest.TestCase):
    """`arc/model-routes-glm-escalate.json` is staged for the submission after D.

    The design is cheap-first: the submission's own model (glm-5.3-flash) does
    every implement, verify and design turn, and only a *repair* -- a turn that
    exists because the cheap model already failed -- escalates to glm-5.3. Always
    using the stronger model would spend more on the turns that did not need it.

    Locked by a test because a staged config that nobody runs is exactly the kind
    of thing that rots: both model IDs answer on the coding-plan endpoint today,
    and `phases` has to stay `["repair"]` for the escalation to mean anything.
    """

    def _rules(self):
        from pathlib import Path
        raw = (Path(__file__).resolve().parent.parent / 'model-routes-glm-escalate.json').read_text(encoding='utf-8')
        return model_routes(raw)

    def _model_for(self, rules, phase, tools=False):
        body = {'model': 'glm-5.3-flash', 'messages': [{'role': 'user', 'content': 'x'}]}
        if tools:
            body['tools'] = [{'type': 'function', 'function': {'name': 'f'}}]
        return json.loads(route_request(json.dumps(body).encode(), rules, phase))['model']

    def test_should_escalate_only_repairs(self):
        rules = self._rules()
        self.assertEqual(self._model_for(rules, 'repair'), 'glm-5.3')
        for phase in ('implement', 'verify', 'design'):
            self.assertEqual(self._model_for(rules, phase), 'glm-5.3-flash', phase)

    def test_should_escalate_a_repair_that_needs_tools(self):
        """Tool-mode repairs are the ones most likely to need the stronger model,
        so the rule must not be skipped by the tools check."""
        self.assertEqual(self._model_for(self._rules(), 'repair', tools=True), 'glm-5.3')


class PhaseForLabelTests(unittest.TestCase):
    """What feeds the routing rules, on the labels the flow really emits.

    A rule saying `"phases": ["repair"]` is worth exactly as much as this
    function's agreement about what counts as a repair. keep's failures in
    submission D are logged as `acceptance specs still failing after repair
    rounds`, so the escalation only helps if those turns really land on
    `repair`.
    """

    def test_should_classify_the_labels_the_flow_emits(self):
        from main import phase_for_label
        cases = {
            'REQ-3.2 implement': 'implement',
            'REQ-3.2 implement (tiny)': 'implement',
            'REQ-3.2 repair 1/5': 'repair',
            'REQ-3.2 rewrite (repair 1)': 'repair',
            'full-suite repair 1/3': 'repair',
            'final check': 'verify',
            'design': 'design',
        }
        for label, phase in cases.items():
            self.assertEqual(phase_for_label(label), phase, label)

    def test_should_send_every_repair_label_to_the_escalated_model(self):
        """End to end against the shipped config: the labels that fail in D must
        select glm-5.3, and implement must stay on the submission's own model."""
        from pathlib import Path
        from main import phase_for_label
        rules = model_routes((Path(__file__).resolve().parent.parent
                              / 'model-routes-glm-escalate.json').read_text(encoding='utf-8'))
        body = json.dumps({'model': 'glm-5.3-flash',
                           'messages': [{'role': 'user', 'content': 'x'}]}).encode()

        def model_for(label):
            return json.loads(route_request(body, rules, phase_for_label(label)))['model']

        for label in ('REQ-3.2 repair 1/5', 'REQ-3.2 rewrite (repair 1)', 'full-suite repair 1/3'):
            self.assertEqual(model_for(label), 'glm-5.3', label)
        self.assertEqual(model_for('REQ-3.2 implement'), 'glm-5.3-flash')
