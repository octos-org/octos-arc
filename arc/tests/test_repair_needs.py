"""「还够不够再来一轮修复」要按这个模型自己的表现算，不是按一个猜的常数。

原先是固定的 `min_repair_seconds`（默认 300）。意图对——代码注释写明「只剩几分钟才开始的
修复轮同样会超时，不如保住最好的状态」——但数字是猜的。keep @ D 实测一轮修复约 600 秒，
是那个常数的两倍。猜小了不是保守而是**反过来**：只剩 300–600 秒时会放行一轮注定被砍断的
修复，什么都留不下，时间也没了。
"""
import unittest

import main


class _Flow:
    min_repair_seconds = 300
    repair_needs = main.Flow.repair_needs

    def __init__(self, durations=None, blocked=False):
        self.codegen_blocked = blocked
        if durations is not None:
            # 传 list 走兼容路径，传 dict 走按模式分桶
            self.repair_durations = durations


class RepairNeedsTests(unittest.TestCase):
    def test_falls_back_to_the_floor_before_any_measurement(self):
        """还没跑过修复轮时，行为与原先一致。"""
        self.assertEqual(_Flow().repair_needs(), 300.0)
        self.assertEqual(_Flow([]).repair_needs(), 300.0)

    def test_uses_the_measured_median_with_headroom(self):
        """keep 实测约 600 秒 → 判断门槛应抬到 600 以上，而不是停在 300。"""
        f = _Flow([600, 610, 590])
        self.assertAlmostEqual(f.repair_needs(), 600 * 1.1)
        self.assertGreater(f.repair_needs(), 300, "这正是原先放行注定失败的修复的区间")

    def test_never_goes_below_the_configured_floor(self):
        """模型很快时也不会把门槛降到下限以下——这条改动只会让判断更严，不会更松。"""
        f = _Flow([10, 12, 11])
        self.assertEqual(f.repair_needs(), 300.0)

    def test_even_count_uses_the_average_of_the_middle_two(self):
        f = _Flow([400, 800])
        self.assertAlmostEqual(f.repair_needs(), 600 * 1.1)

    def test_ignores_junk_values(self):
        f = _Flow([0, None, -5, 600, 600])
        self.assertAlmostEqual(f.repair_needs(), 600 * 1.1)

    def test_only_recent_turns_are_kept(self):
        """模型和题目会变；很久以前的耗时不该继续影响现在的决定。
        记录点保留最近 12 轮。"""
        import inspect
        src = inspect.getsource(main.Flow.turn)
        self.assertIn("[-12:]", src)
        self.assertIn('"repair" in label or "rewrite" in label', src)

    def test_the_gate_uses_it(self):
        import inspect
        src = inspect.getsource(main.Flow.acceptance_loop)
        self.assertIn("left < self.repair_needs()", src)
        self.assertNotIn("left < self.min_repair_seconds", src)


class RepairNeedsByModeTests(unittest.TestCase):
    """必须按模式分桶。本仓库自己的记录是「工具模式单节点慢 19.6 倍（392s vs 20s）」，
    混在一个中位数里，一轮工具模式修复就会把门槛抬到天上，
    于是后面的节点连一轮**便宜的** codegen 修复都不敢起——正好是反效果。
    """

    def test_codegen_estimate_is_not_polluted_by_a_tool_mode_turn(self):
        f = _Flow({"codegen": [600, 600], "tool": [7000, 7000]}, blocked=False)
        self.assertAlmostEqual(f.repair_needs(), 600 * 1.1)

    def test_tool_mode_uses_its_own_much_larger_estimate(self):
        f = _Flow({"codegen": [600, 600], "tool": [7000, 7000]}, blocked=True)
        self.assertAlmostEqual(f.repair_needs(), 7000 * 1.1)

    def test_falls_back_to_the_floor_when_that_mode_has_no_history(self):
        """刚转入工具模式、还没跑过工具模式修复时，用下限而不是拿 codegen 的数去套。"""
        f = _Flow({"codegen": [600, 600]}, blocked=True)
        self.assertEqual(f.repair_needs(), 300.0)

    def test_old_list_shape_still_works(self):
        """兼容旧形态，免得半途升级的运行炸掉。"""
        self.assertAlmostEqual(_Flow([600, 600]).repair_needs(), 600 * 1.1)

    def test_the_recorder_buckets_by_mode(self):
        import inspect
        src = inspect.getsource(main.Flow.turn)
        self.assertIn('mode = "tool" if getattr(self, "codegen_blocked", False) else "codegen"', src)
        self.assertIn("seen[mode] = (seen.get(mode, []) + [elapsed])[-12:]", src)
