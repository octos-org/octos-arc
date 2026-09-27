"""A requirement given up on is rolled back to the last state a check passed."""
import tempfile
import unittest
from pathlib import Path

import verify_node


class RestoreGood(unittest.TestCase):
    def setUp(self):
        self.out = Path(tempfile.mkdtemp())
        good = self.out / ".arc-good" / "app"
        for part, text in (("frontend", "good ui"), ("backend", "good api")):
            (good / part).mkdir(parents=True)
            (good / part / "f.txt").write_text(text)
            (self.out / part).mkdir()
            (self.out / part / "f.txt").write_text("half-done edit")
            (self.out / part / "extra.txt").write_text("new file")

    def test_gives_back_the_last_passing_state(self):
        verify_node.restore_good(self.out, "REQ-3.1")
        self.assertEqual((self.out / "frontend" / "f.txt").read_text(), "good ui")
        self.assertEqual((self.out / "backend" / "f.txt").read_text(), "good api")
        self.assertFalse((self.out / "frontend" / "extra.txt").exists())

    def test_final_pass_is_left_to_the_best_snapshot(self):
        verify_node.restore_good(self.out, "ALL")
        self.assertEqual((self.out / "frontend" / "f.txt").read_text(), "half-done edit")

    def test_no_snapshot_no_change(self):
        empty = Path(tempfile.mkdtemp())
        (empty / "frontend").mkdir()
        (empty / "frontend" / "f.txt").write_text("x")
        verify_node.restore_good(empty, "REQ-1")
        self.assertEqual((empty / "frontend" / "f.txt").read_text(), "x")


if __name__ == "__main__":
    unittest.main()
