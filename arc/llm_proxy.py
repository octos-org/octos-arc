"""Local pass-through proxy in front of the OpenAI-compatible endpoint.

Why: the octos kernel emits DeepSeek's `reasoning_effort` / `thinking` fields
only for api.deepseek.com URLs, while the ARC proxy (api.arc-bench.com)
honours them too — measured on 2026-09-13: default 455 completion tokens,
`reasoning_effort: low` 279, `thinking: disabled` 132 for the same prompt.
This proxy injects the fields into every chat completion request and logs
the provider's `usage` block per request (exact billed tokens, cache hits).

Pure functions (`inject_reasoning`, `usage_record`) are unit-tested; the
server is stdlib `http.server` on 127.0.0.1 and forwards headers verbatim.
"""

from __future__ import annotations

import json
import os
import threading
import time
import urllib.error
import urllib.request
from concurrent.futures import Future
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

HOP_HEADERS = {"host", "content-length", "transfer-encoding", "connection", "accept-encoding"}


def model_routes(raw: str) -> list[dict]:
    """Ordered, opt-in model policies. No model names or task IDs are defaults."""
    rules = json.loads(raw or "[]")
    if not isinstance(rules, list):
        raise ValueError("model routes must be a JSON array")
    phases = {"implement", "repair", "verify", "design"}
    parameters = {"temperature", "top_p", "max_tokens", "max_completion_tokens", "thinking", "reasoning_effort"}
    for rule in rules:
        if not isinstance(rule, dict) or set(rule) - {"model", "phases", "max_input_chars", "tools", "images", "parameters"}:
            raise ValueError("invalid model route fields")
        if not isinstance(rule.get("model"), str) or not rule["model"].strip():
            raise ValueError("model route requires a provider model ID")
        selected = rule.get("phases", list(phases))
        if not isinstance(selected, list) or not selected or any(p not in phases for p in selected):
            raise ValueError("invalid model route phases")
        limit = rule.get("max_input_chars")
        if limit is not None and (type(limit) is not int or limit <= 0):
            raise ValueError("max_input_chars must be positive")
        for name in ("tools", "images"):
            if name in rule and type(rule[name]) is not bool:
                raise ValueError(f"{name} must be boolean")
        opts = rule.get("parameters", {})
        if not isinstance(opts, dict) or set(opts) - parameters:
            raise ValueError("model route parameters cannot replace messages, tools or routing")
        if "max_tokens" in opts and "max_completion_tokens" in opts:
            raise ValueError("choose one output token limit")
    return rules


def configured_model_routes(env=None, bundle_dir: Path | None = None) -> str:
    """Environment wins (including empty); otherwise use optional bundle policy."""
    env = os.environ if env is None else env
    if "OCTOS_ARC_MODEL_ROUTES" in env:
        raw = env["OCTOS_ARC_MODEL_ROUTES"]
    else:
        path = (bundle_dir or Path(__file__).resolve().parent) / "model-routes.json"
        raw = path.read_text(encoding="utf-8") if path.exists() else ""
    model_routes(raw)  # Reject invalid configuration before any provider request.
    return raw


def route_request(body: bytes, rules: list[dict], phase: str) -> bytes:
    """Choose per request from phase, complete input size and tool/image needs.

    Configuration order expresses preference; provider catalogs need not expose
    trustworthy prices. A repair can select a different model in the same task.
    Unmatched requests preserve the caller's model and parameters exactly.
    """
    if not rules:
        return body
    data = json.loads(body)
    if not isinstance(data, dict) or not isinstance(data.get("messages"), list):
        return body
    messages = data["messages"]
    has_images = any(isinstance(m.get("content"), list) and any(
        isinstance(c, dict) and c.get("type") in {"image_url", "input_image"}
        for c in m["content"]) for m in messages)
    needs_tools = bool(data.get("tools")) or any(m.get("tool_calls") or m.get("role") == "tool" for m in messages)
    chars = len(json.dumps({"messages": messages, "tools": data.get("tools", [])}, ensure_ascii=False, separators=(",", ":")))
    for rule in rules:
        if phase not in rule.get("phases", ["implement", "repair", "verify", "design"]):
            continue
        if rule.get("max_input_chars") is not None and chars > rule["max_input_chars"]:
            continue
        if needs_tools and not rule.get("tools", False):
            continue
        if has_images and not rule.get("images", False):
            continue
        data["model"] = rule["model"]
        # Do not carry vendor reasoning fields into a different model. The route
        # supplies supported fields explicitly; there is no name-based guess.
        data.pop("thinking", None)
        data.pop("reasoning_effort", None)
        opts = rule.get("parameters", {})
        if "max_completion_tokens" in opts:
            data.pop("max_tokens", None)
        if "max_tokens" in opts:
            data.pop("max_completion_tokens", None)
        data.update(opts)
        return json.dumps(data, ensure_ascii=False).encode()
    return body


# Families measured to honour `reasoning_effort` / `thinking` on the
# chat/completions path. Anything else is passed through untouched rather than
# risking a 400 from an endpoint that rejects the fields. GLM was measured on
# z.ai's coding endpoint (identical prompt): no field 787 reasoning tokens in
# 17s, `thinking: disabled` 6 in 8s, `reasoning_effort: low` 0 in 6s.
REASONING_CONTROL_FAMILIES = ("deepseek", "glm")


def inject_reasoning(body: bytes, mode: str) -> bytes:
    """mode: "low"|"medium"|"high" -> reasoning_effort (+ thinking enabled);
    "none"/"off" -> thinking disabled; anything else -> unchanged. Fields the
    client already set are respected."""
    if not mode or mode == "passthrough":
        return body
    try:
        data = json.loads(body)
    except (ValueError, UnicodeDecodeError):
        return body
    if not isinstance(data, dict) or "messages" not in data:
        return body
    model = str(data.get("model") or "").lower()
    if not any(family in model for family in REASONING_CONTROL_FAMILIES):
        return body
    if mode in ("none", "off", "disabled"):
        data.setdefault("thinking", {"type": "disabled"})
        data.pop("reasoning_effort", None)
    else:
        data.setdefault("reasoning_effort", mode)
        data.setdefault("thinking", {"type": "enabled"})
    if data.get("stream"):
        opts = data.get("stream_options") if isinstance(data.get("stream_options"), dict) else {}
        opts.setdefault("include_usage", True)
        data["stream_options"] = opts
    return json.dumps(data, ensure_ascii=False).encode("utf-8")


def _usage_from_body(response_body: bytes):
    """JSON body -> its usage dict; SSE body -> usage of the last chunk carrying one."""
    text = response_body.decode("utf-8", errors="replace")
    if text.lstrip().startswith("data:"):
        usage = None
        for line in text.splitlines():
            line = line.strip()
            if not line.startswith("data:") or line == "data: [DONE]":
                continue
            try:
                chunk = json.loads(line[5:].strip())
            except ValueError:
                continue
            if isinstance(chunk, dict) and isinstance(chunk.get("usage"), dict):
                usage = chunk["usage"]
        return usage
    try:
        data = json.loads(text)
    except ValueError:
        return None
    return data.get("usage") if isinstance(data, dict) else None


def request_shape(body: bytes) -> dict | None:
    """Character counts per message role and tool schemas — what the prompt is
    made of (kernel system prompt vs tool schemas vs conversation)."""
    try:
        data = json.loads(body)
    except (ValueError, UnicodeDecodeError):
        return None
    if not isinstance(data, dict) or "messages" not in data:
        return None
    shape: dict = {"messages": len(data.get("messages") or []), "tools": len(data.get("tools") or []),
                   "tools_chars": len(json.dumps(data.get("tools") or [], ensure_ascii=False))}
    for msg in data.get("messages") or []:
        role = str(msg.get("role", "?"))
        content = msg.get("content")
        chars = len(content) if isinstance(content, str) else len(json.dumps(content or "", ensure_ascii=False))
        if msg.get("tool_calls"):
            chars += len(json.dumps(msg["tool_calls"], ensure_ascii=False))
        shape[f"{role}_chars"] = shape.get(f"{role}_chars", 0) + chars
    return shape


# System-prompt sections of the octos coding profile that no ARC task uses.
# Each entry: (heading the cut starts at, heading it stops before). Cuts are
# whole sections, so the kept text stays byte-identical to the kernel's.
DROP_SECTIONS: list[tuple[str, str | None]] = [
    ("## Research & Search Rules", None),
    ("## Rich Card Rendering", None),
    ("## Pipelines", None),
    ("## Background Tasks", None),
    ("## Cancellation", None),
    ("## Scheduled Tasks (Cron)", None),
    ("## Queue Behavior", None),
    ("## Slash Commands", None),
    ("## Active Skills", "## Tool use discipline"),  # cron / skill-store skill docs (H1s inside)
]
# Tools the coding turns never need; the model cannot call what it cannot see.
DROP_TOOLS = {"spawn", "ask_user_question", "check", "tool_search", "update_plan", "exec_command"}


def trim_system_prompt(text: str, drops: list[tuple[str, str | None]] = DROP_SECTIONS) -> str:
    lines = text.split("\n")

    def level(line: str) -> int:
        stripped = line.lstrip("#")
        return len(line) - len(stripped) if line.startswith("#") and stripped.startswith(" ") else 0

    out: list[str] = []
    i = 0
    while i < len(lines):
        line = lines[i]
        rule = next((d for d in drops if line.startswith(d[0])), None)
        if rule is None:
            out.append(line)
            i += 1
            continue
        start_level = level(line)
        j = i + 1
        while j < len(lines):
            if rule[1] is not None:
                if lines[j].startswith(rule[1]):
                    break
            elif 0 < level(lines[j]) <= start_level:
                break
            j += 1
        i = j
    trimmed = "\n".join(out)
    while "\n\n\n\n" in trimmed:
        trimmed = trimmed.replace("\n\n\n\n", "\n\n\n")
    return trimmed


def trim_request(body: bytes, drop_tools: set[str] = DROP_TOOLS) -> bytes:
    """Drop irrelevant system-prompt sections and unused tool schemas from a
    chat request (the platform meters request bytes; Counter: 45k -> ~17k)."""
    try:
        data = json.loads(body)
    except (ValueError, UnicodeDecodeError):
        return body
    if not isinstance(data, dict) or "messages" not in data:
        return body
    for msg in data.get("messages") or []:
        if msg.get("role") == "system" and isinstance(msg.get("content"), str):
            msg["content"] = trim_system_prompt(msg["content"])
    if isinstance(data.get("tools"), list):
        data["tools"] = [t for t in data["tools"]
                         if ((t.get("function") or {}).get("name") or t.get("name")) not in drop_tools]
    return json.dumps(data, ensure_ascii=False).encode("utf-8")


BUDGET_NOTICE = ("Tool budget for this turn is exhausted. Do not call any more tools: reply now with a one-line "
                 "summary of what you changed. The harness will build and test the app.")


def enforce_turn_budget(body: bytes, used: int, budget: int) -> bytes:
    """Once `used` requests have been made in the current turn, strip the tool
    schemas and append a user notice so the model must answer (ending the turn).
    A hard cap the model cannot ignore, unlike prompt budgets (v11-tb: 20-call
    repair turns)."""
    if budget <= 0 or used < budget:
        return body
    try:
        data = json.loads(body)
    except (ValueError, UnicodeDecodeError):
        return body
    if not isinstance(data, dict) or "messages" not in data:
        return body
    data.pop("tools", None)
    data.pop("tool_choice", None)
    msgs = list(data.get("messages") or [])
    if not msgs or msgs[-1].get("role") != "user" or msgs[-1].get("content") != BUDGET_NOTICE:
        msgs.append({"role": "user", "content": BUDGET_NOTICE})
    data["messages"] = msgs
    return json.dumps(data, ensure_ascii=False).encode("utf-8")


def ensure_max_tokens(body: bytes, minimum: int) -> bytes:
    """Raise a too-small `max_tokens` (kernel arc.11 sends 4096; a whole node's
    files need 10-25k — cloud 76fb32a69d81 truncated both implement turns and
    wrote nothing). Never lowers a larger value."""
    if minimum <= 0:
        return body
    try:
        data = json.loads(body)
    except (ValueError, UnicodeDecodeError):
        return body
    if not isinstance(data, dict) or "messages" not in data:
        return body
    current = data.get("max_tokens")
    if not isinstance(current, int) or current < minimum:
        data["max_tokens"] = minimum
        return json.dumps(data, ensure_ascii=False).encode("utf-8")
    return body


def replace_system_prompt(body: bytes, text: str) -> bytes:
    """Codegen turns have no tools; the kernel's worker system prompt (2.4k
    chars of tool guidance) is dead weight there. Keep one system message."""
    try:
        data = json.loads(body)
    except (ValueError, UnicodeDecodeError):
        return body
    if not isinstance(data, dict) or "messages" not in data:
        return body
    msgs = [m for m in data.get("messages") or [] if m.get("role") != "system"]
    data["messages"] = [{"role": "system", "content": text}] + msgs
    return json.dumps(data, ensure_ascii=False).encode("utf-8")


def strip_all_tools(body: bytes) -> bytes:
    try:
        data = json.loads(body)
    except (ValueError, UnicodeDecodeError):
        return body
    if not isinstance(data, dict) or "messages" not in data:
        return body
    data.pop("tools", None)
    data.pop("tool_choice", None)
    return json.dumps(data, ensure_ascii=False).encode("utf-8")


def destream_request(body: bytes) -> tuple[bytes, bool]:
    """Turn a streaming chat request into a non-streaming one. Returns
    (new_body, was_streaming). The platform's meter sits between us and the
    model and appears to sum the cumulative `usage` of every SSE chunk
    (cloud cc066e8e11f6: provider 50.6k tokens, platform 613k); one JSON
    response carries the usage exactly once."""
    try:
        data = json.loads(body)
    except (ValueError, UnicodeDecodeError):
        return body, False
    if not isinstance(data, dict) or not data.get("stream"):
        return body, False
    data["stream"] = False
    data.pop("stream_options", None)
    return json.dumps(data, ensure_ascii=False).encode("utf-8"), True


def to_sse(response_body: bytes) -> bytes:
    """Re-emit a non-streaming chat completion as the SSE the client asked
    for: one delta chunk with the whole message (content, reasoning,
    tool_calls), then a finish chunk carrying usage, then [DONE]."""
    try:
        data = json.loads(response_body)
    except (ValueError, UnicodeDecodeError):
        return response_body
    if not isinstance(data, dict) or "choices" not in data:
        return response_body  # error payloads pass through as-is
    base = {"id": data.get("id"), "object": "chat.completion.chunk", "created": data.get("created"),
            "model": data.get("model")}
    lines = []
    for choice in data.get("choices") or []:
        msg = choice.get("message") or {}
        delta = {"role": msg.get("role", "assistant")}
        for key in ("content", "reasoning_content"):
            if msg.get(key) is not None:
                delta[key] = msg[key]
        if msg.get("tool_calls"):
            delta["tool_calls"] = [dict(tc, index=i) for i, tc in enumerate(msg["tool_calls"])]
        lines.append(json.dumps(dict(base, choices=[{"index": choice.get("index", 0), "delta": delta,
                                                      "finish_reason": None}]), ensure_ascii=False))
        lines.append(json.dumps(dict(base, choices=[{"index": choice.get("index", 0), "delta": {},
                                                      "finish_reason": choice.get("finish_reason", "stop")}]),
                                ensure_ascii=False))
    lines.append(json.dumps(dict(base, choices=[], usage=data.get("usage") or {}), ensure_ascii=False))
    return "".join(f"data: {l}\n\n" for l in lines).encode("utf-8") + b"data: [DONE]\n\n"


def usage_record(response_body: bytes, elapsed_ms: int, mode: str) -> dict | None:
    usage = _usage_from_body(response_body)
    if not isinstance(usage, dict):
        return None
    rec = {"ts": time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime()), "elapsed_ms": elapsed_ms, "mode": mode}
    text = response_body.decode("utf-8", errors="replace")
    if text.lstrip().startswith("data:"):
        rec["sse_chunks"] = sum(1 for l in text.splitlines() if l.startswith("data:") and l.strip() != "data: [DONE]")
        rec["sse_usage_chunks"] = text.count('"usage"')
    else:
        rec["sse_chunks"] = 0
    for key in ("prompt_tokens", "completion_tokens", "total_tokens", "prompt_cache_hit_tokens",
                "prompt_cache_miss_tokens"):
        if key in usage:
            rec[key] = usage[key]
    details = usage.get("completion_tokens_details") or {}
    if isinstance(details, dict) and "reasoning_tokens" in details:
        rec["reasoning_tokens"] = details["reasoning_tokens"]
    # The ARC endpoint reports cache hits OpenAI-style (prompt_tokens_details.
    # cached_tokens), not DeepSeek-style; fold either into one field.
    pdetails = usage.get("prompt_tokens_details") or {}
    if "prompt_cache_hit_tokens" not in rec and isinstance(pdetails, dict) and "cached_tokens" in pdetails:
        rec["prompt_cache_hit_tokens"] = pdetails["cached_tokens"]
    return rec


class LlmProxy:
    def __init__(self, upstream_base: str, mode: str, log_path: Path | None = None, host: str = "127.0.0.1",
                 dump_dir: Path | None = None, dump_limit: int = 3, destream: bool = True, trim: bool = True,
                 extra_drop_tools: set[str] | None = None, min_max_tokens: int = 32768) -> None:
        self.upstream = upstream_base.rstrip("/")
        self.mode = mode
        self.routes = model_routes(configured_model_routes())
        self.phase = "implement"
        self.min_max_tokens = min_max_tokens
        self.destream = destream
        self.trim = trim
        # Tools removed from every request in addition to DROP_TOOLS (mutable:
        # the flow can take the shell away for one-turn tasks and give it back).
        self.extra_drop_tools: set[str] = set(extra_drop_tools or ())
        self.no_tools = False  # codegen turns: strip every tool schema
        self.system_override: str | None = None  # codegen turns: replace the kernel system prompt
        # Per-turn request cap (0 = unlimited); the flow calls begin_turn().
        self.turn_budget = 0
        self.turn_requests = 0
        self.budget_hits = 0
        # Run-wide billable usage (prompt + completion), for the flow's cost guard.
        self.total_requests = 0
        self.total_tokens = 0
        self.log_path = log_path
        self.dump_dir = dump_dir      # OCTOS_ARC_PROXY_DUMP=1: first N request bodies for prefix analysis
        self.dump_limit = dump_limit
        self._dumped = 0
        self._lock = threading.Lock()
        self._inflight: dict[tuple, Future] = {}
        proxy = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_):  # silence stderr noise
                pass

            def _forward(self, method: str) -> None:
                length = int(self.headers.get("Content-Length") or 0)
                body = self.rfile.read(length) if length else b""
                was_streaming = False
                if method == "POST" and self.path.rstrip("/").endswith("/chat/completions"):
                    body = inject_reasoning(body, proxy.mode)
                    body = ensure_max_tokens(body, proxy.min_max_tokens)
                    with proxy._lock:
                        used = proxy.turn_requests
                        proxy.turn_requests += 1
                    capped = enforce_turn_budget(body, used, proxy.turn_budget)
                    if capped is not body:
                        proxy.budget_hits += 1
                    body = capped
                    if proxy.trim or proxy.extra_drop_tools:
                        body = trim_request(body, (DROP_TOOLS if proxy.trim else set()) | proxy.extra_drop_tools)
                    if proxy.no_tools:
                        body = strip_all_tools(body)
                    if proxy.system_override:
                        body = replace_system_prompt(body, proxy.system_override)
                    if proxy.destream:
                        body, was_streaming = destream_request(body)
                    body = route_request(body, proxy.routes, proxy.phase)
                    proxy._dump(body)
                headers = {k: v for k, v in self.headers.items() if k.lower() not in HOP_HEADERS}
                headers["Content-Length"] = str(len(body))
                path = self.path
                if path.startswith("/v1") and proxy.upstream.endswith("/v1"):
                    path = path[3:]
                status, payload, resp_headers = proxy._request_upstream(method, path, body, headers)
                ctype = resp_headers.get("Content-Type", "application/json") if resp_headers else "application/json"
                if was_streaming and status == 200:
                    payload, ctype = to_sse(payload), "text/event-stream; charset=utf-8"
                try:
                    self.send_response(status)
                    self.send_header("Content-Type", ctype)
                    self.send_header("Content-Length", str(len(payload)))
                    self.end_headers()
                    self.wfile.write(payload)
                except (BrokenPipeError, ConnectionResetError):
                    pass  # A timed-out client may have retried while upstream was pending.

            def do_POST(self):
                self._forward("POST")

            def do_GET(self):
                self._forward("GET")

        self.server = ThreadingHTTPServer((host, 0), Handler)
        self.server.daemon_threads = True
        self.port = self.server.server_address[1]
        self.base_url = f"http://{host}:{self.port}/v1"
        self._thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def _request_upstream(self, method: str, path: str, body: bytes, headers: dict) -> tuple:
        # Only pending identical completions are shared. Include credentials and
        # all forwarded headers; never share across distinct requests or phases.
        key = (method, path, body, tuple(sorted((k.lower(), v) for k, v in headers.items())), self.phase) \
            if method == "POST" and path.rstrip("/").endswith("/chat/completions") else None
        with self._lock:
            future = self._inflight.get(key) if key is not None else None
            owner = future is None
            if owner:
                future = Future()
                if key is not None:
                    self._inflight[key] = future
        if not owner:
            return future.result()
        try:
            req = urllib.request.Request(self.upstream + path, data=body if body else None,
                                         headers=headers, method=method)
            t0 = time.time()
            try:
                with urllib.request.urlopen(req, timeout=600) as resp:
                    result = resp.status, resp.read(), resp.headers
            except urllib.error.HTTPError as exc:
                result = exc.code, exc.read(), exc.headers
            except Exception as exc:  # noqa: BLE001
                result = 502, json.dumps({"error": {"message": f"proxy: {exc}"}}).encode(), {}
            _, payload, _ = result
            self._log(payload, int((time.time() - t0) * 1000), body, len(body), len(payload))
            future.set_result(result)
            return result
        except BaseException as exc:
            future.set_exception(exc)
            raise
        finally:
            if key is not None:
                with self._lock:
                    self._inflight.pop(key, None)

    def begin_turn(self, budget: int) -> None:
        with self._lock:
            self.turn_budget = int(budget)
            self.turn_requests = 0

    def _dump(self, body: bytes) -> None:
        if not self.dump_dir or self._dumped >= self.dump_limit:
            return
        try:
            self.dump_dir.mkdir(parents=True, exist_ok=True)
            self._dumped += 1
            (self.dump_dir / f"request-{self._dumped:02d}.json").write_bytes(body)
        except OSError:
            pass

    def _log(self, payload: bytes, elapsed_ms: int, request_body: bytes = b"", req_bytes: int = 0,
             resp_bytes: int = 0) -> None:
        if not self.log_path:
            return
        rec = usage_record(payload, elapsed_ms, self.mode)
        if rec is None:
            return
        with self._lock:
            self.total_requests += 1
            self.total_tokens += int(rec.get("prompt_tokens") or 0) + int(rec.get("completion_tokens") or 0)
        rec["request_bytes"], rec["response_bytes"] = req_bytes, resp_bytes
        shape = request_shape(request_body)
        if shape:
            rec["request"] = shape
            rec["model"] = json.loads(request_body).get("model")
            rec["phase"] = self.phase
        with self._lock:
            try:
                with self.log_path.open("a", encoding="utf-8") as fh:
                    fh.write(json.dumps(rec) + "\n")
            except OSError:
                pass

    def start(self) -> "LlmProxy":
        self._thread.start()
        return self

    def stop(self) -> None:
        try:
            self.server.shutdown()
            self.server.server_close()
        except Exception:  # noqa: BLE001
            pass
