"""The bundle's own public specs are the fallback when the runner mounts none.

Submission 4224afbed824 (2026-09-26): all six official runs logged
`tests at None`, so every node was built and checked blind. The bundle now
ships public-tests/<task>/ and locate_tests picks the directory whose spec ids
cover this task's requirement tree.
"""
import os
import unittest
from pathlib import Path
from unittest import mock

import main


def tree_for(task: str) -> dict:
    """The real requirement tree, so the spec-name parsing is checked against
    real node ids (`REQ-1.1.spec.ts` must count for node `REQ-1.1`)."""
    return main.load_tree(main.BUNDLE_DIR / "tasks" / task)


class LocateTests(unittest.TestCase):
    def setUp(self):
        patcher = mock.patch.dict(os.environ, {}, clear=False)
        patcher.start()
        self.addCleanup(patcher.stop)
        for name in ("ARCBENCH_TESTS_DIR", "OCTOS_ARC_LOCAL_TESTS", "OCTOS_ARC_BUNDLED_TESTS"):
            os.environ.pop(name, None)

    def test_picks_the_matching_bundled_task(self):
        for task in ("arc-bench-web--keep", "arc-bench-web--bookstack", "arc-bench-web--12306"):
            found = main.locate_tests(tree_for(task))
            self.assertEqual(found, (main.BUNDLE_DIR / "public-tests" / task).resolve(), task)

    def test_unknown_tree_stays_blind(self):
        tree = {"id": "ROOT", "type": "FOLDER", "children": [{"id": "XYZ-9.9", "type": "ATOMIC"}]}
        self.assertIsNone(main.locate_tests(tree))

    def test_bundled_fallback_can_be_switched_off(self):
        os.environ["OCTOS_ARC_BUNDLED_TESTS"] = "0"
        self.assertIsNone(main.locate_tests(tree_for("arc-bench-web--keep")))

    def test_runner_mount_still_wins(self):
        mount = main.BUNDLE_DIR / "public-tests" / "arc-bench-web--bookstack"
        os.environ["ARCBENCH_TESTS_DIR"] = str(mount)
        self.assertEqual(main.locate_tests(tree_for("arc-bench-web--keep")), Path(mount).resolve())


if __name__ == "__main__":
    unittest.main()
