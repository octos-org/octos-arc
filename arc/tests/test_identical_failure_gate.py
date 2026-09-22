"""「失败相同」只有在上一轮**真的改过应用**时，才是关于修法的证据——**驱动真实的循环**来验。

先前我为这条判据写的测试，在测试里把那段分支**重新实现了一遍**再断言，
那是在验我自己的复刻，不是验代码：改坏 `acceptance_loop` 它也不会红。
这里改成按仓库里既有的方式真正调用 `flow.acceptance_loop(...)`。
"""
import time
import unittest
from unittest.mock import Mock

import main
from acceptance import RunSummary, TestOutcome

REPEATED = "Repeated attempts produced the same observed failure"


def _flow(wrote):
    flow = object.__new__(main.Flow)
    flow.runner = object()
    flow.repair_rounds = 2
    flow.min_repair_seconds = 0
    flow.node_timeout = 60
    flow.smoke_port = 43219
    flow.web_port = 3000
    flow.tests_dir = None
    flow.pending_corrections = []
    flow.head = lambda: "sha"
    flow.codegen_mode = lambda: False
    flow.wound_down = lambda: False
    flow.time_up = lambda: False
    flow.record_tests = Mock()
    flow.snapshot_sources = Mock()
    flow.sources_text = lambda: ""
    flow.corrections_text = lambda: ""
    flow.repair_requirements = lambda _n=None: ""
    flow.repair_test_location = lambda *_a: ""
    flow.slow_test_ms = lambda: 10_000
    flow.perf_text = lambda: ""
    flow.turn = Mock(return_value=(True, ""))
    flow.commit = Mock()
    flow.restore_app = Mock()
    flow.last_codegen_wrote = wrote
    return flow


def _same_failures(n):
    """连续 n 轮给出**完全相同**的失败，好让 `normalized == previous_failures` 成立。"""
    f = TestOutcome("behavior", False, "failed", 1, message="missing control")
    return [RunSummary(passed=0, total=1, results=[f]) for _ in range(n)]


class IdenticalFailureGateTests(unittest.TestCase):
    def test_no_correction_when_the_previous_repair_wrote_nothing(self):
        """上一轮什么都没落盘 → 代码逐字节没变 → 失败相同是必然的，不含信息。
        这时若还叫模型「复查你修法背后的假设」，是把它送去调试一个从来不是问题的推理。"""
        flow = _flow(wrote=False)
        flow.run_specs = Mock(side_effect=_same_failures(3))
        flow.acceptance_loop("node", ["a.spec.ts"], time.time() + 1000)
        joined = " ".join(flow.pending_corrections)
        self.assertNotIn(REPEATED, joined, flow.pending_corrections)
        self.assertFalse(getattr(flow, "codegen_blocked", False))

    def test_correction_still_fires_when_files_actually_moved(self):
        """原有行为必须保留：真的改了文件而失败没动，那才是修法有问题。"""
        flow = _flow(wrote=True)
        flow.run_specs = Mock(side_effect=_same_failures(3))
        flow.acceptance_loop("node", ["a.spec.ts"], time.time() + 1000)
        joined = " ".join(flow.pending_corrections)
        self.assertIn(REPEATED, joined)
        self.assertTrue(flow.codegen_blocked)

    def test_unknown_keeps_the_original_behaviour(self):
        """None = 未知（工具模式经工具写盘，不走块解析）。未知时不得改变判定，
        否则会悄悄改掉工具模式的行为。"""
        flow = _flow(wrote=None)
        flow.run_specs = Mock(side_effect=_same_failures(3))
        flow.acceptance_loop("node", ["a.spec.ts"], time.time() + 1000)
        self.assertIn(REPEATED, " ".join(flow.pending_corrections))

    def test_differing_failures_never_trigger_it(self):
        f1 = TestOutcome("behavior", False, "failed", 1, message="missing control")
        f2 = TestOutcome("behavior", False, "failed", 1, message="wrong total")
        flow = _flow(wrote=True)
        flow.run_specs = Mock(side_effect=[RunSummary(passed=0, total=1, results=[f1]),
                                           RunSummary(passed=0, total=1, results=[f2]),
                                           RunSummary(passed=0, total=1, results=[f1])])
        flow.acceptance_loop("node", ["a.spec.ts"], time.time() + 1000)
        self.assertNotIn(REPEATED, " ".join(flow.pending_corrections))


if __name__ == "__main__":
    unittest.main()
