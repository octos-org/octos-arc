"""Minimal Octos UI Protocol v1 stdio driver (mirrors octos-tui).

Spawns `octos serve --stdio --solo` and speaks NDJSON JSON-RPC over
stdin/stdout. No handshake: send session/open, then turn/start, then collect
notifications until turn/completed or turn/error. Approvals are auto-approved
so the agent never blocks unattended.
"""

from __future__ import annotations

import json, queue, subprocess, threading, time, uuid
from pathlib import Path
from typing import Callable


class OctosProtocolError(RuntimeError):
    pass


class OctosStdioSession:
    def __init__(self, octos_bin: str, cwd: Path, env: dict, data_dir: Path,
                 on_event: Callable[[str, dict], None] | None = None,
                 extra_args: list[str] | None = None) -> None:
        self.cwd = str(cwd)
        self.data_dir = Path(data_dir)
        self.on_event = on_event or (lambda method, params: None)
        cmd = [octos_bin, "serve", "--stdio", "--solo", "--data-dir", str(data_dir)]
        cmd.extend(extra_args or [])
        if env.get("OCTOS_DANGER_FULL_ACCESS") == "1":
            cmd.append("--danger-full-access")
        self.proc = subprocess.Popen(
            cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, text=True, cwd=self.cwd, env=env,
            errors="replace", bufsize=1,
        )
        self._pending: dict[str, queue.Queue] = {}
        self._pending_lock = threading.Lock()
        self._notifications: queue.Queue = queue.Queue()
        self._stderr_lines: list[str] = []
        self._id_counter = 0
        self._reader = threading.Thread(target=self._read_stdout, daemon=True)
        self._reader.start()
        self._stderr_reader = threading.Thread(target=self._read_stderr, daemon=True)
        self._stderr_reader.start()
        self.session_id = f"arc-bundle:{uuid.uuid4().hex[:8]}"

    # ------------------------------------------------------------ plumbing

    def _read_stderr(self) -> None:
        assert self.proc.stderr is not None
        for line in self.proc.stderr:
            line = line.rstrip()
            self._stderr_lines.append(line)
            if "[arc-mod]" in line:      # patched-core proof marker
                self.on_event("core/marker", {"line": line})

    def _read_stdout(self) -> None:
        assert self.proc.stdout is not None
        for line in self.proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                frame = json.loads(line)
            except json.JSONDecodeError:
                continue
            if "method" in frame:
                self._notifications.put(frame)
            elif "id" in frame:
                with self._pending_lock:
                    q = self._pending.pop(str(frame["id"]), None)
                if q is not None:
                    q.put(frame)

    def _send(self, method: str, params: dict | None, want_response: bool,
              timeout: float = 60.0) -> dict | None:
        self._id_counter += 1
        rid = f"py-{self._id_counter}"
        msg: dict = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            msg["params"] = params
        q: queue.Queue | None = None
        if want_response:
            msg["id"] = rid
            q = queue.Queue()
            with self._pending_lock:
                self._pending[rid] = q
        assert self.proc.stdin is not None
        try:
            self.proc.stdin.write(json.dumps(msg, ensure_ascii=False) + "\n")
            self.proc.stdin.flush()
        except (BrokenPipeError, OSError) as exc:
            raise OctosProtocolError(
                f"octos stdio write failed (process died?): {exc}; "
                f"stderr tail: {self.stderr_tail()}") from exc
        if q is None:
            return None
        try:
            frame = q.get(timeout=timeout)
        except queue.Empty as exc:
            raise OctosProtocolError(f"timeout waiting response to {method}") from exc
        if "error" in frame:
            raise OctosProtocolError(f"{method} failed: {frame['error']}")
        return frame.get("result")

    def stderr_tail(self, n: int = 10) -> str:
        return "\n".join(self._stderr_lines[-n:])

    # ------------------------------------------------------------ protocol

    def bootstrap_profile(self, provider: str, model: str, base_url: str | None,
                          api_key_env: str | None, timeout: float = 60.0) -> None:
        """Create a solo profile and select its LLM (serve mode has no
        config-file default profile). The id must be unique per call: the
        profile store persists across runs, so a fixed id collides with
        "local profile 'arc-2' already exists with different email"."""
        import os
        unique = f"arc-{os.getpid()}-{int(time.time() * 1000) % 10_000_000}"
        res = self._send("profile/local/create", {
            "requested_id": unique,
            "name": "ARC Bundle Agent",
            # legacy fields required by older releases (<= v2.0.1)
            "username": unique,
            "email": f"{unique}@solo.local",
        }, want_response=True, timeout=timeout)
        self.profile_id = res.get("profile_id") if isinstance(res, dict) else None
        if not self.profile_id:
            raise OctosProtocolError(f"profile/local/create gave no profile_id: {res}")
        api_type = "anthropic" if provider == "anthropic" else "openai"
        route: dict = {"api_type": api_type}
        if base_url:
            route["base_url"] = base_url
        if api_key_env:
            route["api_key_env"] = api_key_env
        self._send("profile/llm/upsert", {
            "profile_id": self.profile_id,
            "set_primary": True,
            "selection": {
                "family_id": provider,
                "model_id": model,
                "route": route,
            },
        }, want_response=True, timeout=timeout)

    def open(self, timeout: float = 120.0) -> None:
        params = {"session_id": self.session_id, "cwd": self.cwd}
        if getattr(self, "profile_id", None):
            params["profile_id"] = self.profile_id
        self._send("session/open", params, want_response=True, timeout=timeout)

    def run_turn(self, text: str, timeout: float = 1800.0) -> tuple[bool, str]:
        """Run one turn; stream events to on_event. Returns (ok, full_text)."""
        deadline = time.monotonic() + timeout
        if timeout <= 0:
            return False, "octos turn timed out"
        turn_id = str(uuid.uuid4())
        self._send("turn/start", {
            "session_id": self.session_id,
            "turn_id": turn_id,
            "input": [{"kind": "text", "text": text}],
        }, want_response=True, timeout=min(60.0, timeout))

        chunks: list[str] = []
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return False, "octos turn timed out"
            if self.proc.poll() is not None:
                return False, (f"octos process exited {self.proc.returncode}; "
                               f"stderr tail: {self.stderr_tail()}")
            try:
                frame = self._notifications.get(timeout=min(remaining, 5.0))
            except queue.Empty:
                continue
            method = frame.get("method", "")
            params = frame.get("params") or {}
            self.on_event(method, params)
            if method == "server/heartbeat":
                continue
            if method == "message/delta" and params.get("turn_id") == turn_id:
                chunks.append(str(params.get("text", "")))
            elif method == "approval/requested":   # never block unattended
                try:
                    self._send("approval/respond", {
                        "approval_id": params.get("approval_id"),
                        "decision": "approve",
                        "approval_scope": "request",
                    }, want_response=True, timeout=30.0)
                except OctosProtocolError:
                    pass
            elif method == "turn/completed" and params.get("turn_id") == turn_id:
                return True, "".join(chunks)
            elif method == "turn/error" and params.get("turn_id") == turn_id:
                return False, (f"{params.get('code', 'error')}: "
                               f"{params.get('message', '')}"[:1000])

    def close(self) -> None:
        try:
            self.proc.terminate()
            self.proc.wait(timeout=10)
        except Exception:
            try:
                self.proc.kill()
            except Exception:
                pass
