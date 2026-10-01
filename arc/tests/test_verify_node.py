"""The page audit catches the UI defects requirements imply, on HTML we wrote
for this test -- never on evaluation material."""
import tempfile
import unittest
from pathlib import Path

import verify_node


class AuditPages(unittest.TestCase):
    def audit(self, html: str) -> list[str]:
        with tempfile.TemporaryDirectory() as tmp:
            dist = Path(tmp)
            (dist / "index.html").write_text(html)
            return verify_node.audit_pages(dist)

    def test_duplicate_named_entry_on_one_page_is_flagged(self):
        problems = self.audit('<a href="/signin">Sign in</a><p><a href="/signin">Sign in</a></p>')
        self.assertTrue(any("Sign in" in p and "links named" in p for p in problems))

    def test_single_named_entry_passes(self):
        self.assertEqual(self.audit('<a href="/signin">Sign in</a><a href="/up">Sign up</a>'), [])

    def test_input_without_label_is_flagged(self):
        problems = self.audit('<input type="text" id="u">')
        self.assertTrue(any("no visible label" in p for p in problems))

    def test_input_with_label_for_aria_or_wrapping_passes(self):
        self.assertEqual(self.audit('<label for="u">User</label><input id="u">'), [])
        self.assertEqual(self.audit('<input aria-label="User">'), [])
        self.assertEqual(self.audit('<label>User <input type="text"></label>'), [])

    def test_password_and_checkbox_need_labels_too(self):
        self.assertTrue(self.audit('<input type="checkbox">'))
        self.assertFalse(self.audit('<label for="t">Terms</label><input type="checkbox" id="t">'))

    def test_unnamed_button_is_flagged(self):
        self.assertTrue(self.audit('<button></button>'))
        self.assertEqual(self.audit('<button>Go</button>'), [])
        self.assertEqual(self.audit('<button aria-label="Go"></button>'), [])

    def test_hidden_and_submit_inputs_are_exempt(self):
        self.assertEqual(self.audit('<input type="hidden"><input type="submit" value="Go">'), [])


class CrashPattern(unittest.TestCase):
    def test_crash_signatures(self):
        self.assertTrue(verify_node.CRASH_RE.search("Error [ERR_HTTP_HEADERS_SENT]: nope"))
        self.assertTrue(verify_node.CRASH_RE.search("Traceback (most recent call last):"))
        self.assertFalse(verify_node.CRASH_RE.search("backend listening on port 3000"))


class E2eFileList(unittest.TestCase):
    """A grouped implement node's several self-checks run against ONE shared
    boot via --e2e-list; a single (ungrouped) node still uses --e2e alone."""

    def test_e2e_list_wins_and_splits_on_comma(self):
        self.assertEqual(
            verify_node.parse_e2e_files("checks/ignored.mjs", "checks/a.mjs,checks/b.mjs"),
            ["checks/a.mjs", "checks/b.mjs"])

    def test_single_e2e_used_when_no_list(self):
        self.assertEqual(verify_node.parse_e2e_files("checks/a.mjs", None), ["checks/a.mjs"])

    def test_neither_given_is_empty(self):
        self.assertEqual(verify_node.parse_e2e_files(None, None), [])

    def test_e2e_list_drops_blanks_from_stray_commas(self):
        self.assertEqual(verify_node.parse_e2e_files(None, "checks/a.mjs,,checks/b.mjs,"),
                         ["checks/a.mjs", "checks/b.mjs"])


if __name__ == "__main__":
    unittest.main()
