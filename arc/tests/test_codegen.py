import tempfile
import unittest
from pathlib import Path

from codegen import parse_file_blocks, write_files


class ParseTests(unittest.TestCase):
    def test_should_extract_blocks_and_confine_paths(self):
        text = ("Here you go.\n<<<FILE backend/server.js>>>\nconst x = 1;\n<<<END FILE>>>\n"
                "<<<FILE frontend/src/index.html >>>\n<p>hi</p>\n<<<END FILE>>>\n"
                "<<<FILE ../etc/passwd>>>\nno\n<<<END FILE>>>\n<<<FILE /abs/x>>>\nno\n<<<END FILE>>>\nDone.")
        files = parse_file_blocks(text)
        self.assertEqual(sorted(files), ["backend/server.js", "frontend/src/index.html"])
        self.assertEqual(files["backend/server.js"], "const x = 1;\n")

    def test_should_strip_a_stray_fence_and_keep_marker_like_code(self):
        text = "<<<FILE a.js>>>\n```js\nif (a <<< b) {}\n```\n<<<END FILE>>>"
        self.assertEqual(parse_file_blocks(text)["a.js"], "if (a <<< b) {}\n")

    def test_should_return_empty_when_no_blocks(self):
        self.assertEqual(parse_file_blocks("just prose"), {})

    def test_should_write_files_under_root(self):
        with tempfile.TemporaryDirectory() as tmp:
            written = write_files(Path(tmp), {"backend/server.js": "x\n"})
            self.assertEqual(written, ["backend/server.js"])
            self.assertEqual((Path(tmp) / "backend" / "server.js").read_text(), "x\n")


class EnsureCharsetTests(unittest.TestCase):
    def test_should_inject_meta_charset_when_missing(self):
        from codegen import ensure_charset
        self.assertEqual(ensure_charset("<html><head><title>x</title></head><body>账户</body></html>"),
                         '<html><head><meta charset="utf-8"><title>x</title></head><body>账户</body></html>')
        self.assertEqual(ensure_charset("<html><body>x</body></html>"),
                         '<html><head><meta charset="utf-8"></head><body>x</body></html>')
        self.assertEqual(ensure_charset("<p>x</p>"), '<meta charset="utf-8">\n<p>x</p>')

    def test_should_keep_existing_charset(self):
        from codegen import ensure_charset
        page = '<html><head><meta charset="UTF-8"></head></html>'
        self.assertEqual(ensure_charset(page), page)

    def test_should_apply_to_written_html_files(self):
        import tempfile
        from pathlib import Path
        from codegen import write_files
        root = Path(tempfile.mkdtemp())
        write_files(root, {"frontend/src/index.html": "<html><head></head><body></body></html>", "backend/server.js": "x"})
        self.assertIn('<meta charset="utf-8">', (root / "frontend/src/index.html").read_text())
        self.assertEqual((root / "backend/server.js").read_text(), "x")


class UnescapeFlattenedTests(unittest.TestCase):
    def test_should_restore_newlines_in_a_flattened_block(self):
        from codegen import unescape_flattened
        flat = "const a = 1;\\n" * 12 + "x"
        out = unescape_flattened(flat)
        self.assertEqual(out.count("\n"), 12)
        self.assertNotIn("\\n", out)

    def test_should_leave_normal_files_with_string_escapes_alone(self):
        from codegen import unescape_flattened
        normal = "res.end('a\\nb');\n" * 20
        self.assertEqual(unescape_flattened(normal), normal)


class RepairFlattenedJsTests(unittest.TestCase):
    def test_should_unescape_only_when_it_makes_the_file_parse(self):
        import shutil, tempfile
        from pathlib import Path
        from codegen import repair_flattened_js
        if not shutil.which("node"):
            self.skipTest("node not on PATH")
        p = Path(tempfile.mkdtemp()) / "server.js"
        p.write_text("const a = 1;\nfunction f() {\n  if (a) x = 1;\\n  if (!a) x = 2;\\n  return x;\n}\n")
        self.assertTrue(repair_flattened_js(p))
        self.assertNotIn("\\n", p.read_text())
        good = "const s = 'a\\nb';\nconsole.log(s);\n"
        p.write_text(good)
        self.assertFalse(repair_flattened_js(p))
        self.assertEqual(p.read_text(), good)


class DedupeNavLinksTests(unittest.TestCase):
    def test_should_strip_static_nav_links_only_when_server_fills_placeholder(self):
        import tempfile
        from pathlib import Path
        from codegen import dedupe_nav_links
        root = Path(tempfile.mkdtemp())
        (root / "frontend/src").mkdir(parents=True); (root / "backend").mkdir()
        page = '<body><!--NAV-->\n<a href="/register">Register</a>\n<a href="/about">About</a>\n<a href="/help">Help</a></body>'
        (root / "frontend/src/index.html").write_text(page)
        (root / "backend/server.js").write_text("x")
        self.assertEqual(dedupe_nav_links(root), [])
        (root / "backend/server.js").write_text("""const nav = '<a href="/register">R</a> <a href="/about">A</a>'; html.replace('<!--NAV-->', nav)""")
        self.assertEqual(dedupe_nav_links(root), ["index.html"])
        out = (root / "frontend/src/index.html").read_text()
        self.assertNotIn('href="/register"', out)
        self.assertNotIn('href="/about"', out)
        self.assertIn('href="/help"', out)  # not rendered by the server: kept
        self.assertIn("<!--NAV-->", out)
        self.assertIn("<!--NAV-->", out)
