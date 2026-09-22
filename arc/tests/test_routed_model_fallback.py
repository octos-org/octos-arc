"""路由到的模型上游没有时，代理要优雅降级，而不是让每一轮修复都 404。

这一条是**执行发布包**时实测出来的：包里的 model-routes.json 把修复阶段指向 glm-5.3，
本机 ollama 没有这个模型，日志里是

    model not found — HTTP 404 - {"error":{"message":"model 'glm-5.3' not found"}}

后果与成因不成比例——一个配置错误会让整轮修复能力归零，比根本不做路由还糟。

`test_codegen.py` 里已经单测了两个判据函数，但**没有测接线**。
所以这里起一个真的 LlmProxy，对着一个桩上游，走完整条请求路径。
不用本机模型：那个东西会间歇性不可用，拿它当测试依赖只会让测试变成掷骰子。
"""
import json
import threading
import unittest
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from llm_proxy import LlmProxy

MISSING = "glm-5.3"


class _Upstream:
    """桩上游：被问到 MISSING 就 404 model not found，其余 200。记录收到过哪些模型。"""

    def __init__(self):
        self.seen: list[str] = []
        outer = self

        class H(BaseHTTPRequestHandler):
            # 必须和代理一样是 HTTP/1.1。默认的 HTTP/1.0 会让连接在响应后关闭，
            # 而代理是 keep-alive 的——两边不一致时客户端拿到的是 RemoteDisconnected，
            # 看起来像代理坏了，其实是桩写得不对。（这个坑花了我好几轮才定位。）
            protocol_version = "HTTP/1.1"

            def log_message(self, *a):  # 不要把测试输出弄脏
                pass

            def do_POST(self):
                raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                model = json.loads(raw or b"{}").get("model", "")
                outer.seen.append(model)
                if model == MISSING:
                    body = json.dumps({"error": {"message": f"model '{MISSING}' not found",
                                                 "type": "not_found_error"}}).encode()
                    self.send_response(404)
                else:
                    body = json.dumps({"choices": [{"message": {"role": "assistant", "content": "ok"},
                                                    "finish_reason": "stop"}]}).encode()
                    self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def stop(self):
        self.server.shutdown()
        self.server.server_close()


class RoutedModelFallbackWiringTests(unittest.TestCase):
    def setUp(self):
        self.up = _Upstream()
        self.proxy = LlmProxy(self.up.url, mode="passthrough", destream=False, trim=False).start()
        self.proxy.routes = [{"model": MISSING, "phases": ["repair"]}]
        self.proxy.phase = "repair"
        self.addCleanup(self.up.stop)
        self.addCleanup(self.proxy.stop)

    def _post(self):
        body = json.dumps({"model": "base-model",
                           "messages": [{"role": "user", "content": "hi"}]}).encode()
        conn = http.client.HTTPConnection("127.0.0.1", self.proxy.port, timeout=30)
        try:
            conn.request("POST", "/v1/chat/completions", body=body,
                         headers={"Content-Type": "application/json",
                                  "Content-Length": str(len(body))})
            resp = conn.getresponse()
            return resp.status, json.loads(resp.read())
        finally:
            conn.close()

    def test_caller_gets_a_usable_answer_not_the_404(self):
        status, payload = self._post()
        self.assertEqual(status, 200)
        self.assertEqual(payload["choices"][0]["message"]["content"], "ok")

    def test_it_tried_the_routed_model_then_fell_back_to_the_callers(self):
        self._post()
        self.assertEqual(self.up.seen, [MISSING, "base-model"],
                         f"上游收到的顺序应为 [{MISSING}, base-model]，实际 {self.up.seen}")

    def test_the_dead_route_is_dropped_so_it_is_not_paid_for_again(self):
        self._post()
        self.assertEqual(self.proxy.routes, [])
        self.up.seen.clear()
        self._post()
        self.assertEqual(self.up.seen, ["base-model"],
                         "第二次请求不该再去试那个不存在的模型")

    def test_a_healthy_route_is_left_alone(self):
        """反向：上游有这个模型时，不得回退、不得丢路由。"""
        self.proxy.routes = [{"model": "base-model", "phases": ["repair"]}]
        status, _ = self._post()
        self.assertEqual(status, 200)
        self.assertEqual(self.up.seen, ["base-model"])
        self.assertEqual(len(self.proxy.routes), 1)


if __name__ == "__main__":
    unittest.main()
