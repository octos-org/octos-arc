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

CANONICAL_FILE_BLOCK = re.compile(
    r"<<<FILE\s+(?P<path>[^\n>]+?)\s*>>>\r?\n(?P<body>.*?)(?:\r?\n)?<<<END FILE>>>", re.S)

# Weaker (cheaper) models drift on the delimiter without getting the block wrong.
# qwen2.5-coder:7b emitted `<<<FILE frontend/src/index.html>` — a single `>` — and
# closed with a correct `<<<END FILE>>>`; the body in between was a complete, correct
# page. The strict pattern matched nothing, so the harness logged "reply contained no
# file blocks" and threw away 25s and a working implementation. The path pattern
# already excludes `>`, so accepting one-or-more closes the near miss without making
# the marker ambiguous against code or markdown.
FILE_BLOCK = re.compile(
    r"<<<FILE\s+(?P<path>[^\n>]+?)\s*>+\r?\n(?P<body>.*?)(?:\r?\n)?<<<END\s+FILE\s*>+", re.S)

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


def delimiter_drift(text: str) -> list[str]:
    """Paths whose block only parsed because of the tolerance above.

    Worth logging rather than swallowing silently: how far a model drifts off the
    marker protocol is a property of that model, and it is what tells us whether
    the format instructions need to be firmer for the cheap tier."""
    def paths(pattern: re.Pattern[str]) -> set[str]:
        return {m.group("path").strip().strip("`'\"") for m in pattern.finditer(text or "")}
    return sorted(paths(FILE_BLOCK) - paths(CANONICAL_FILE_BLOCK))


def unparsed_reply_digest(text: str, limit: int = 160) -> str:
    """Why a reply yielded no file blocks, in one bounded line.

    The paid model logged 36 "reply contained no file blocks" against 571
    successful writes across the five long submission-C runs -- about 6% of
    codegen turns thrown away -- and the cause cannot be established after the
    fact: the run log keeps the harness's verdict but not the reply.

    The turn line above it does print a slice of the reply, but only its *tail*.
    That is the wrong end. The local 7B's near-miss was in the *opening* marker
    (`<<<FILE path>` instead of `>>>`) while its tail closed correctly, so the
    tail slice looked perfectly healthy and the real defect stayed invisible.

    So report what actually separates the candidate causes: whether an opening
    marker appeared at all and exactly how it was written, how many closing
    markers there were, and a bounded slice from *both* ends -- a truncated
    reply loses its close at the end, a reply that opens with prose or a fence
    fails at the start.
    """
    t = text or ""
    opens = re.findall(r"<<<\s*FILE[^\n]{0,120}", t)
    closes = len(re.findall(r"<<<\s*END\s*FILE\s*>*", t))
    bits = [f"len={len(t)}", f"open={opens[0]!r}" if opens else "open=absent"]
    if len(opens) > 1:
        bits.append(f"opens={len(opens)}")
    bits.append(f"close={closes}")
    bits.append(f"head={t[:limit]!r}")
    if len(t) > limit:
        bits.append(f"tail={t[-limit:]!r}")
    return " ".join(bits)


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


def js_parses(path: Path) -> bool | None:
    """`node --check`; None when node is unavailable."""
    import shutil, subprocess
    node = shutil.which("node")
    if not node:
        return None
    try:
        return subprocess.run([node, "--check", str(path)], capture_output=True, timeout=20).returncode == 0
    except (OSError, subprocess.SubprocessError):
        return None


def repair_flattened_js(path: Path) -> bool:
    """Partially flattened blocks (some lines carry literal \\n between statements;
    local s12 crashed at startup) are only rewritten when the unescaped version
    parses and the original does not."""
    text = path.read_text(encoding="utf-8", errors="replace")
    if "\\n" not in text or js_parses(path) is not False:
        return False
    fixed = "\n".join(line.replace("\\n", "\n") if line.count("\\n") >= 2 and not line.lstrip().startswith(("res.", "return"))
                      else line for line in text.split("\n"))
    if fixed == text:
        return False
    backup = path.read_bytes()
    path.write_text(fixed, encoding="utf-8")
    if js_parses(path):
        return True
    path.write_bytes(backup)
    return False


def dedupe_nav_links(root: Path) -> list[str]:
    """Compatibility hook: source-level href matching cannot prove redundancy.

    Repeated destinations may be required navigation, calls to action, or
    conditional content. Let acceptance failures drive explicit app repairs.
    """
    return []


def unchanged_rewrites(root: Path, files: dict[str, str]) -> list[str]:
    """Paths the model returned exactly as they already were on disk.

    Measured on the deepseek batch: 282 codegen turns wrote 688 files, 2.44 per
    turn, two thirds of turns writing 2 or more and ten of them writing 8 to 10.
    The count alone cannot say how much of that was work. A file handed back
    unchanged still cost a whole file's worth of output tokens, and turn time is
    output tokens over generation rate -- ~12,800 output tokens per turn at
    ~69 tokens/s on keep -- so this is the number that says how much of a turn
    was spent producing nothing.

    Normalised the way `write_files` will normalise, so the comparison is against
    what would actually land rather than against the raw reply.
    """
    same = []
    for rel, body in files.items():
        dest = root / rel
        if not dest.exists():
            continue
        if dest.suffix.lower() in (".html", ".htm"):
            body = ensure_charset(body)
        try:
            if dest.read_text(encoding="utf-8") == body:
                same.append(rel)
        except (OSError, UnicodeDecodeError):
            continue          # 读不出来就别猜，当作有变化
    return sorted(same)


def write_files(root: Path, files: dict[str, str]) -> list[str]:
    written = []
    for rel, body in files.items():
        dest = root / rel
        dest.parent.mkdir(parents=True, exist_ok=True)
        if dest.suffix.lower() in (".html", ".htm"):
            body = ensure_charset(body)
        dest.write_text(body, encoding="utf-8")
        if dest.suffix.lower() in (".js", ".cjs", ".mjs"):
            repair_flattened_js(dest)
        written.append(rel)
    return written
