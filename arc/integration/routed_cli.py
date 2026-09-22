#!/usr/bin/env python3
"""Exercise the real Rust CLI and stdio kernel using a local, unbilled provider.

Usage: python3 arc/integration/routed_cli.py /path/to/octos /path/to/evidence
The provider returns a design response, then rejects implementation. This tests
cross-phase routing and operational failure handling without generating an app.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def verify(binary: Path, evidence: Path):
    received = []

    class Provider(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def send_json(self, status, payload):
            body = json.dumps(payload).encode()
            self.send_response(status)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            self.send_json(200, {'object': 'list', 'data': []})

        def do_POST(self):
            request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            received.append({
                'path': self.path,
                'model': request.get('model'),
                'tools': len(request.get('tools') or []),
                'messages': len(request.get('messages') or []),
                'authorized': self.headers.get('Authorization') == 'Bearer local-test-only',
            })
            if request.get('model') == 'design-model':
                self.send_json(200, {
                    'id': 'local-design', 'object': 'chat.completion',
                    'model': 'design-model',
                    'choices': [{'index': 0, 'message': {'role': 'assistant', 'content': '{"notes":"Implement the supplied requirement."}'}, 'finish_reason': 'stop'}],
                    'usage': {'prompt_tokens': 10, 'completion_tokens': 5, 'total_tokens': 15},
                })
            else:
                self.send_json(402, {'error': {'code': 'insufficient_balance', 'message': 'local test rejection'}})

    server = ThreadingHTTPServer(('127.0.0.1', 0), Provider)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    evidence.mkdir(parents=True, exist_ok=True)
    try:
        with tempfile.TemporaryDirectory(prefix='octos-routing-cli-') as tmp:
            root = Path(tmp)
            req = root / 'requirements.json'
            req.write_text(json.dumps({'id': 'generic-node', 'type': 'ATOMIC', 'description': 'Provide a page that displays the supplied message.'}))
            out = root / 'output'
            spec = root / 'runner-spec.json'
            spec.write_text(json.dumps({
                'requirement_path': str(req), 'output_dir': str(out), 'web_port': 43219,
                'model': {'provider': 'openai', 'model': 'original-model',
                          'base_url': f'http://127.0.0.1:{server.server_port}/v1', 'api_key_env': 'OPENAI_API_KEY'},
            }))
            rules = [
                {'model': 'design-model', 'phases': ['design'], 'tools': True},
                {'model': 'implementation-model', 'phases': ['implement']},
            ]
            env = {k: v for k, v in os.environ.items() if not k.startswith(('OCTOS_', 'ARCBENCH_', 'OPENAI_', 'DEEPSEEK_', 'ANTHROPIC_'))}
            env.update(OPENAI_API_KEY='local-test-only', OCTOS_CONFIG_DIR=str(root / 'config'), OCTOS_DISABLE_STREAMING='1')
            policy = root / 'policy.toml'
            policy.write_text('[mode]\ndesign_min_nodes=1\ndesign_mode="separate"\ntiny=false\n'
                              '[reasoning]\ntransient_retries=0\nprobe_patience_seconds=1\n'
                              '[budget]\ntime_budget_seconds=120\n')
            command = [str(binary), 'arc', 'run', '--spec', str(spec), '--policy', str(policy),
                       '--model-routes-json', json.dumps(rules)]
            result = subprocess.run(command, cwd=root, env=env, capture_output=True, text=True, timeout=120)
            (evidence / 'stdout.log').write_text(result.stdout)
            (evidence / 'stderr.log').write_text(result.stderr)
            (evidence / 'requests.json').write_text(json.dumps(received, indent=2) + '\n')
            for name in ['model-routes.jsonl', 'octos-arc-events.jsonl']:
                source = out / '.arc' / name
                if source.exists():
                    (evidence / name).write_bytes(source.read_bytes())
            assert result.returncode == 1, f'expected operational failure, got {result.returncode}; see {evidence}'
            assert [r['model'] for r in received] == ['design-model', 'implementation-model'], received
            assert received[0]['tools'] > 0 and received[1]['tools'] == 0, received
            assert all(r['authorized'] and r['path'] == '/v1/chat/completions' for r in received), received
            events = [json.loads(line) for line in (evidence / 'octos-arc-events.jsonl').read_text().splitlines()]
            assert any(e['event'] == 'run_failed' for e in events), events[-3:]
            assert not any(e['event'] == 'run_completed' for e in events)
            assert any(e['event'] == 'usage_total' and e['estimated_cost'] is None for e in events)
            print('PASS: real stdio design -> direct codegen, two routed models, then one permanent rejection; exit 1; no follow-up requests.')
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary', type=Path)
    parser.add_argument('evidence', type=Path)
    args = parser.parse_args()
    verify(args.binary.resolve(), args.evidence.resolve())
