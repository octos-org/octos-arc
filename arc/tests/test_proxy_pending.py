import json
import tempfile
import threading
import unittest
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from unittest.mock import patch

from llm_proxy import LlmProxy


class PendingCompletionTests(unittest.TestCase):
    def test_pending_retry_shares_response_but_completed_requests_run_again(self):
        started, release, joined = threading.Event(), threading.Event(), threading.Event()
        payload = json.dumps({'usage': {'prompt_tokens': 2, 'completion_tokens': 1}}).encode()
        class Response:
            status = 200
            headers = {'Content-Type': 'application/json'}
            def __enter__(self): return self
            def __exit__(self, *args): pass
            def read(self):
                started.set()
                if not release.wait(3): raise TimeoutError('test release missing')
                return payload
        with tempfile.TemporaryDirectory() as folder:
            proxy = LlmProxy('http://unused/v1', 'none', log_path=Path(folder) / 'usage.jsonl')
            args = ('POST', '/chat/completions', b'{"model":"generic","messages":[]}', {'Authorization': 'test'})
            try:
                with patch('llm_proxy.urllib.request.urlopen', return_value=Response()) as upstream:
                    with ThreadPoolExecutor(max_workers=2) as pool:
                        first = pool.submit(proxy._request_upstream, *args)
                        self.assertTrue(started.wait(2))
                        future = next(iter(proxy._inflight.values()))
                        original = future.result
                        def waiting(*a, **kw):
                            joined.set()
                            return original(*a, **kw)
                        with patch.object(future, 'result', side_effect=waiting):
                            second = pool.submit(proxy._request_upstream, *args)
                            self.assertTrue(joined.wait(2))
                            self.assertEqual(upstream.call_count, 1)
                            release.set()
                            self.assertEqual(first.result(2), second.result(2))
                    self.assertFalse(proxy._inflight)
                    self.assertEqual(proxy.total_requests, 1)
                    proxy._request_upstream(*args)
                    self.assertEqual(upstream.call_count, 2)
                    self.assertEqual(proxy.total_requests, 2)
            finally:
                release.set()
                proxy.server.server_close()

    def test_different_inputs_credentials_and_phases_remain_independent(self):
        for variant in ('body', 'credentials', 'phase'):
            with self.subTest(variant=variant):
                first_started, both_started, release = threading.Event(), threading.Event(), threading.Event()
                calls = []
                class Response:
                    status = 200
                    headers = {}
                    def __enter__(self): return self
                    def __exit__(self, *args): pass
                    def read(self):
                        calls.append(1)
                        first_started.set()
                        if len(calls) == 2: both_started.set()
                        release.wait(2)
                        return b'{}'
                proxy = LlmProxy('http://unused/v1', 'none')
                args = ['POST', '/chat/completions', b'{"messages":[]}', {'Authorization': 'first'}]
                try:
                    with patch('llm_proxy.urllib.request.urlopen', return_value=Response()):
                        with ThreadPoolExecutor(max_workers=2) as pool:
                            first = pool.submit(proxy._request_upstream, *args)
                            self.assertTrue(first_started.wait(1))
                            if variant == 'body': args[2] = b'{"messages":[{}]}'
                            elif variant == 'credentials': args[3] = {'Authorization': 'second'}
                            else: proxy.phase = 'repair'
                            second = pool.submit(proxy._request_upstream, *args)
                            try: self.assertTrue(both_started.wait(1))
                            finally: release.set()
                            first.result(2); second.result(2)
                    self.assertFalse(proxy._inflight)
                finally:
                    release.set()
                    proxy.server.server_close()

    def test_upstream_error_is_released_for_later_retry(self):
        proxy = LlmProxy('http://unused/v1', 'none')
        try:
            with patch('llm_proxy.urllib.request.urlopen', side_effect=TimeoutError('upstream timed out')) as upstream:
                for _ in range(2):
                    status, _, _ = proxy._request_upstream('POST', '/chat/completions', b'{}', {})
                    self.assertEqual(status, 502)
                    self.assertFalse(proxy._inflight)
                self.assertEqual(upstream.call_count, 2)
        finally:
            proxy.server.server_close()
