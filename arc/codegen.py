"""Single-response code generation for one-node tasks (A5).

With tools stripped at the proxy, the model answers ONE request with the
whole application as delimited file blocks; the harness writes them, then
the normal acceptance loop runs. Two tool-protocol round trips (write, then
final answer) become one request, and no tool schemas travel with it.

Format (chosen so it never collides with code or markdown fences):

    <<<FILE backend/server.js>>>
    ...file contents...
    <<<END FILE>>>
"""

from __future__ import annotations

import re
from pathlib import Path

FILE_BLOCK = re.compile(r"<<<FILE\s+(?P<path>[^\n>]+?)\s*>>>\r?\n(?P<body>.*?)(?:\r?\n)?<<<END FILE>>>", re.S)

FORMAT_INSTRUCTIONS = """\
Format, one block per file, nothing else:
<<<FILE relative/path>>>
contents
<<<END FILE>>>
"""


def parse_file_blocks(text: str) -> dict[str, str]:
    """Extract path -> contents; a later block for the same path wins.
    Paths are normalised and confined to the project (no absolute, no `..`)."""
    files: dict[str, str] = {}
    for m in FILE_BLOCK.finditer(text or ""):
        raw = m.group("path").strip().strip("`'\"")
        parts = [p for p in raw.replace("\\", "/").split("/") if p not in ("", ".")]
        if not parts or ".." in parts or raw.startswith("/"):
            continue
        body = m.group("body")
        # tolerate a stray fence the model wrapped around the body
        stripped = body.strip("\n")
        if stripped.startswith("```") and stripped.rstrip().endswith("```"):
            inner = stripped.split("\n", 1)[1] if "\n" in stripped else ""
            body = inner.rsplit("```", 1)[0]
        files["/".join(parts)] = body.rstrip("\n") + "\n"
    return files


CHARSET_META = '<meta charset="utf-8">'


def ensure_charset(text: str) -> str:
    """Pages without a charset declaration were decoded as Latin-1 by Chromium
    (the servers send `text/html` without charset), so every Chinese string the
    specs look for turned into mojibake (local s5/s10: 0/6). Inject the meta tag."""
    if re.search(r"<meta[^>]+charset", text, re.IGNORECASE):
        return text
    m = re.search(r"<head[^>]*>", text, re.IGNORECASE)
    if m:
        return text[:m.end()] + CHARSET_META + text[m.end():]
    m = re.search(r"<html[^>]*>", text, re.IGNORECASE)
    if m:
        return text[:m.end()] + "<head>" + CHARSET_META + "</head>" + text[m.end():]
    return CHARSET_META + "\n" + text


def unescape_flattened(text: str) -> str:
    """A file block occasionally arrives with its newlines JSON-escaped (one long
    line full of literal \\n; local s12: server.js failed to parse at startup).
    Restore it when the block is clearly flattened; leave normal files alone."""
    real = text.count("\n")
    literal = text.count("\\n")
    if literal >= 10 and literal > 5 * max(real, 1):
        return text.replace("\\r\\n", "\n").replace("\\n", "\n").replace("\\t", "\t")
    return text


def write_files(root: Path, files: dict[str, str]) -> list[str]:
    written = []
    for rel, body in files.items():
        dest = root / rel
        dest.parent.mkdir(parents=True, exist_ok=True)
        body = unescape_flattened(body)
        if dest.suffix.lower() in (".html", ".htm"):
            body = ensure_charset(body)
        dest.write_text(body, encoding="utf-8")
        written.append(rel)
    return written
