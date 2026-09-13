#!/usr/bin/env python3
"""Exercise the bundled binary's stdio turn and shell path in a container.

The local HTTP server is a deterministic OpenAI-compatible fixture: it asks
the agent to run one shell command, then returns OK.  This keeps the check
offline while proving that ``serve --stdio --solo`` actually reaches the
shell tool and that the container sandbox decision is visible in stderr.
"""

from __future__ import annotations

import json
import os
import sys
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, "/src")

from arc.octos_stdio import OctosStdioSession


class FixtureHandler(BaseHTTPRequestHandler):
    request_count = 0
    request_lock = threading.Lock()

    def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
        length = int(self.headers.get("content-length", "0"))
        payload = json.loads(self.rfile.read(length))
        messages = payload.get("messages", [])
        has_tool_result = any(message.get("role") == "tool" for message in messages)
        with self.request_lock:
            type(self).request_count += 1
            request_number = type(self).request_count

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()

        if not has_tool_result:
            chunks = [
                {
                    "choices": [
                        {
                            "index": 0,
                            "delta": {
                                "role": "assistant",
                                "tool_calls": [
                                    {
                                        "index": 0,
                                        "id": "b5-shell-call",
                                        "type": "function",
                                        "function": {
                                            "name": "shell",
                                            "arguments": json.dumps(
                                                {
                                                    "command": "printf b5-sandbox-exec > /workspace/b5-marker",
                                                }
                                            ),
                                        },
                                    }
                                ],
                            },
                            "finish_reason": None,
                        }
                    ]
                },
                {"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
            ]
        else:
            chunks = [
                {
                    "choices": [
                        {
                            "index": 0,
                            "delta": {"content": "OK"},
                            "finish_reason": None,
                        }
                    ]
                },
                {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
            ]

        for chunk in chunks:
            self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
            self.wfile.flush()
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()
        print(f"fixture_request={request_number} has_tool_result={has_tool_result}", flush=True)

    def log_message(self, format: str, *args: object) -> None:
        return


def main() -> int:
    workspace = Path("/workspace")
    workspace.mkdir(parents=True, exist_ok=True)
    marker = workspace / "b5-marker"
    if marker.exists():
        marker.unlink()

    server = ThreadingHTTPServer(("127.0.0.1", 0), FixtureHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    env = os.environ.copy()
    env["B5_FAKE_API_KEY"] = "fixture-only"
    env["OPENAI_BASE_URL"] = f"http://127.0.0.1:{server.server_port}/v1"
    env["PYTHONDONTWRITEBYTECODE"] = "1"
    events: list[tuple[str, dict]] = []
    session: OctosStdioSession | None = None
    stderr = ""
    text = ""
    ok = False
    try:
        with tempfile.TemporaryDirectory(prefix="octos-b5-data-") as data_dir:
            session = OctosStdioSession(
                "/src/target/release/octos",
                workspace,
                env,
                Path(data_dir),
                on_event=lambda method, params: events.append((method, params)),
                extra_args=[
                    "--cwd",
                    str(workspace),
                    "--provider",
                    "openai",
                    "--model",
                    "fixture",
                    "--auth-token",
                    "fixture-only",
                ],
            )
            session.bootstrap_profile(
                provider="openai",
                model="fixture",
                base_url=f"http://127.0.0.1:{server.server_port}/v1",
                api_key_env="B5_FAKE_API_KEY",
                timeout=180.0,
            )
            session.open(timeout=120.0)
            ok, text = session.run_turn(
                "Run the shell command requested by the fixture and then reply OK.",
                timeout=120.0,
            )
            stderr = session.stderr_tail(200)
    finally:
        if session is not None:
            session.close()
        server.shutdown()
        server.server_close()

    marker_value = marker.read_text(encoding="utf-8") if marker.is_file() else ""
    container_marker = Path("/.dockerenv").is_file()
    sandbox_log = any("sandbox" in line.lower() for line in stderr.splitlines())
    report = {
        "container_marker": container_marker,
        "fixture_requests": FixtureHandler.request_count,
        "turn_ok": ok,
        "reply": text,
        "marker": marker_value,
        "sandbox_log": sandbox_log,
        "stderr_tail": stderr,
        "event_methods": [method for method, _ in events],
    }
    Path("/src/b5-container-smoke.log").write_text(
        json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(json.dumps(report, ensure_ascii=False, indent=2))
    if not container_marker:
        raise SystemExit("B5 failed: /.dockerenv was not present")
    if not ok or marker_value != "b5-sandbox-exec":
        raise SystemExit("B5 failed: stdio turn did not execute the shell marker")
    if not sandbox_log:
        raise SystemExit("B5 failed: sandbox decision was not visible in stderr")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
