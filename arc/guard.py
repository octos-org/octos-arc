"""Guard rules (A7): watch one turn's tool events and the final message and
produce short corrective sentences for the next prompt.

Detects: completion claims with no verification command; the same tool error
three times in a row; writes into protected paths (specs, requirements, .arc).
"""

from __future__ import annotations

import re

_VERIFY = re.compile(r"\b(npm run build|npm start|npm run start|node \S+\.js|curl\b|playwright|wget\b|node --check)", re.I)
_CLAIM = re.compile(r"\b(implemented|complete[d]?|done|verified|passes|passing|finished|working)\b|✅", re.I)
_WRITE_TOOLS = {"write_file", "edit_file", "apply_patch", "create_file", "append_file"}
_SHELL_TOOLS = {"bash", "shell", "exec", "run_command"}
_REDIRECT = re.compile(r"(?:>>?|tee\s+(?:-a\s+)?|cp\s+\S+\s+|mv\s+\S+\s+|sed\s+-i\S*\s+(?:'[^']*'|\S+)\s+)\s*(\S+)")


class TurnMonitor:
    def __init__(self, protected_prefixes: list[str], repeat_threshold: int = 3,
                 expect_verification: bool = True, allowed_prefixes: list[str] | None = None) -> None:
        self.protected = [p for p in protected_prefixes if p]
        self.allowed = [p for p in (allowed_prefixes or []) if p]
        self.repeat_threshold = repeat_threshold
        self.expect_verification = expect_verification
        self.wrote_files = False
        self.verified = False
        self.tool_calls = 0
        self.errors_in_a_row = 0
        self._last_error = None
        self._max_repeat = 0
        self._repeated_error = ""
        self.protected_writes: list[str] = []
        self.written_paths: list[str] = []
        self._pending: dict[str, tuple[str, dict]] = {}
        self._final_text = ""

    # -- events -----------------------------------------------------------
    def observe(self, method: str, params: dict) -> None:
        if method == "tool/started":
            self.tool_calls += 1
            name = str(params.get("tool_name") or "")
            args = params.get("arguments") or {}
            self._pending[str(params.get("tool_call_id"))] = (name, args)
            if name in _WRITE_TOOLS:
                self.wrote_files = True
                self._note_path(str(args.get("path") or args.get("file_path") or ""))
            elif name in _SHELL_TOOLS:
                cmd = str(args.get("cmd") or args.get("command") or "")
                if _VERIFY.search(cmd):
                    self.verified = True
                if re.search(r"\b(cat|echo|printf|tee|cp|mv|sed)\b.*(>|tee|-i)", cmd) or re.search(r"\b(cp|mv)\s", cmd):
                    self.wrote_files = True
                    for m in _REDIRECT.finditer(cmd):
                        self._note_path(m.group(1).strip("'\""))
        elif method == "tool/completed":
            ok = bool(params.get("success", True))
            preview = str(params.get("output_preview") or "")[:300]
            if not ok:
                key = re.sub(r"\d+", "#", preview)
                if key == self._last_error:
                    self.errors_in_a_row += 1
                else:
                    self._last_error, self.errors_in_a_row = key, 1
                if self.errors_in_a_row > self._max_repeat:
                    self._max_repeat, self._repeated_error = self.errors_in_a_row, preview
            else:
                self._last_error, self.errors_in_a_row = None, 0

    def _note_path(self, path: str) -> None:
        if not path:
            return
        self.written_paths.append(path)
        if any(path.startswith(a) or f"/{a}" in path for a in self.allowed):
            return
        for prefix in self.protected:
            if path.startswith(prefix) or f"/{prefix}" in path:
                self.protected_writes.append(path)
                break

    def finish(self, final_text: str) -> None:
        self._final_text = final_text or ""

    # -- verdicts ---------------------------------------------------------
    def corrections(self) -> list[str]:
        out: list[str] = []
        if self.expect_verification and self.wrote_files and not self.verified and _CLAIM.search(self._final_text):
            out.append("Your previous turn claimed completion without running any build, start or "
                       "request command. Use the supplied isolated verification command before claiming success. "
                       "If no verification entry is supplied, build and exercise the app in a disposable copy "
                       "so validation does not change the delivered application's persistent data.")
        if self._max_repeat >= self.repeat_threshold:
            out.append(f"You hit the same error {self._max_repeat} times in a row "
                       f"({self._repeated_error[:160]!r}). Stop repeating the command; diagnose the "
                       "root cause (read the file / port / path involved) and change approach.")
        if self.protected_writes:
            out.append("You modified protected files that must never change: "
                       + ", ".join(sorted(set(self.protected_writes))[:5])
                       + ". Revert nothing yourself; only touch frontend/ and backend/ from now on.")
        return out
