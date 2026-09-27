"""Helper modules over the inline limit are cut to what the spec reaches.

Cutting at 12000 chars left ctrip/12306 nodes with the first half of a ~25k
helpers.ts: the selectors their specs call were simply not in the prompt.
"""
import re
import unittest
from pathlib import Path

import main

TESTS = main.BUNDLE_DIR / "public-tests"
_IMPORTED = re.compile(r"import\s*\{([^}]*)\}\s*from\s*['\"]\./helpers['\"]")


def declared(text: str, name: str) -> bool:
    return any(m.group(1) == name for line in text.splitlines() if (m := main._DECL.match(line)))


class SliceModule(unittest.TestCase):
    def test_small_module_is_untouched(self):
        text = "export function a() {}\n" * 10
        self.assertEqual(main.slice_module(text, "a()"), text)

    def test_keeps_transitive_uses_and_drops_the_rest(self):
        text = ("import { x } from 'y';\nexport function used() { return inner(); }\n"
                "function inner() { return 1; }\nexport function unused() {}\n") + "// pad\n" * 3000
        out = main.slice_module(text, "used()")
        self.assertIn("import { x }", out)
        self.assertTrue(declared(out, "used") and declared(out, "inner"))
        self.assertFalse(declared(out, "unused"))

    def test_every_imported_helper_survives_on_real_tasks(self):
        for task in ("arc-bench-web--12306", "arc-bench-web--ctrip", "arc-bench-web--stackoverflow"):
            helpers = (TESTS / task / "helpers.ts").read_text(encoding="utf-8")
            for spec in sorted((TESTS / task).glob("*.spec.ts")):
                text = spec.read_text(encoding="utf-8")
                out = main.slice_module(helpers, text)
                self.assertLess(len(out), len(helpers), spec.name)
                m = _IMPORTED.search(text)
                for name in (n.strip().split(" as ")[0] for n in (m.group(1).split(",") if m else []) if n.strip()):
                    self.assertTrue(declared(out, name), f"{spec.name}: {name}")


if __name__ == "__main__":
    unittest.main()
