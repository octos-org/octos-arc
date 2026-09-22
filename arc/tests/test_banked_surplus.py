"""只动用**已经省下来**的预算，不向未来借钱。

keep @ D 的实测（2026-09-19）：全程只用掉预算的 63%，而丢掉的六个节点里有三个在开局附近，
那时 `remaining / nodes_left` 恰好是 1500，离它们需要的 1813 差 287 秒，修复停在第一轮。
把 `node_budget_cap` 从 1500 抬到 3000 救不了这三个——第 4 个节点的份额只有 1592，
上限根本没顶到。要救它们，只能让节点用上前面**已经省下**的时间。
"""
import unittest

import main


class _Flow:
    """只带 banked_surplus 需要的那两样：budget 和 remaining()。"""

    def __init__(self, budget, used):
        self.budget = budget
        self._used = used

    def remaining(self):
        return self.budget - self._used

    banked_surplus = main.Flow.banked_surplus


class BankedSurplusTests(unittest.TestCase):
    def test_ahead_of_schedule_releases_half_the_surplus(self):
        """keep 的第 4 个节点：32 题、48000s 预算，已跑 3 个用掉 1824s。
        按进度本该用掉 4500s，结余 2676s，放出一半 = 1338s。"""
        f = _Flow(48000, 1824)
        self.assertAlmostEqual(f.banked_surplus(4, 32), (48000 * 3 / 32 - 1824) / 2)
        share = f.remaining() / (32 - 4 + 1)
        self.assertLess(share, 1813, "光靠份额不够——这正是那三个节点输掉的原因")
        self.assertGreaterEqual(min(3000, share + f.banked_surplus(4, 32)), 1813,
                                "加上已省下的结余之后才够")

    def test_behind_schedule_changes_nothing(self):
        """超支时结余为 0，行为逐字不变——不能让一个已经超支的运行继续超支。"""
        f = _Flow(48000, 20000)          # 第 4 个节点就用掉 20000s，远超进度
        self.assertEqual(f.banked_surplus(4, 32), 0.0)

    def test_first_node_has_nothing_banked(self):
        """开局还没跑过任何节点，谈不上结余。"""
        f = _Flow(48000, 0)
        self.assertEqual(f.banked_surplus(1, 32), 0.0)

    def test_only_half_is_released(self):
        """留一半给后面，免得开局几个难节点把余额吃光、把尾巴饿到 240 秒地板。"""
        f = _Flow(1000, 0)
        self.assertAlmostEqual(f.banked_surplus(6, 10), (1000 * 5 / 10) / 2)

    def test_degenerate_inputs_are_safe(self):
        """预算或节点数缺失时返回 0，绝不让一个诊断性的加数把节点预算算炸。"""
        self.assertEqual(_Flow(0, 0).banked_surplus(4, 32), 0.0)
        self.assertEqual(_Flow(48000, 0).banked_surplus(4, 0), 0.0)

    def test_the_cap_and_floor_still_apply(self):
        """四个约束是叠加的：份额 + 结余，再被 cap 封顶、被 240 地板托住。"""
        f = _Flow(48000, 0)
        raw = f.remaining() / 1 + f.banked_surplus(32, 32)
        self.assertGreater(raw, 3000)
        self.assertEqual(min(3000, max(240, raw)), 3000, "cap 仍然封得住")


class BudgetIsParetoSafeTests(unittest.TestCase):
    """这组改动**不可能**让任何节点拿到比旧参数更少的预算。

    代数上显然：`surplus >= 0` 且上限只升不降，所以
        新 = min(3000, max(240, share + surplus)) >= min(1500, max(240, share)) = 旧
    但「显然」是我这一段里被推翻过好几次的词，所以跑一遍模拟把它钉住——
    尤其要排除「抬高开局的份额会把尾巴饿到 240 秒地板」这个真实担忧。
    """

    @staticmethod
    def _old(remaining, nodes_left):
        return min(1500, max(240, remaining / nodes_left))

    @staticmethod
    def _new(flow, index, total):
        try:
            surplus = float(flow.banked_surplus(index, total))
        except Exception:
            surplus = 0.0
        return min(flow.node_budget_cap, max(240, flow.remaining() / (total - index + 1) + surplus))

    def _simulate(self, total, greed):
        """greed = 每个节点实际用掉它拿到预算的比例。1.0 是最坏情况。"""
        budget = 1500 * total
        f = _Flow(budget, 0)
            
        f.node_budget_cap = 3000
        worst_new, floors = float("inf"), 0
        for i in range(1, total + 1):
            new = self._new(f, i, total)
            old = self._old(f.remaining(), total - i + 1)
            self.assertGreaterEqual(round(new, 6), round(old, 6),
                                    f"节点 {i}：新参数给的 {new:.0f}s 少于旧参数的 {old:.0f}s")
            worst_new = min(worst_new, new)
            floors += new <= 240.0001
            f._used += new * greed
        return worst_new, floors

    def test_never_less_than_the_old_parameters_at_any_greed(self):
        for total in (32, 66, 125):
            for greed in (0.0, 0.4, 1.0):
                worst, floors = self._simulate(total, greed)
                self.assertGreaterEqual(worst, 1500,
                                        f"{total} 节点 / 用量 {greed}：最小节点预算跌到 {worst:.0f}s")
                self.assertEqual(floors, 0,
                                 f"{total} 节点 / 用量 {greed}：有 {floors} 个节点被饿到 240s 地板")

    def test_the_worry_that_motivated_this_test(self):
        """担忧原文：开局几个难节点吃光余额，把尾巴饿到地板。
        最坏情况（每个节点用光）下并没有发生——因为结余只在**领先进度**时才放，
        而领先意味着份额本来就不低于公平份额。"""
        worst, floors = self._simulate(66, 1.0)
        self.assertEqual(floors, 0)
        self.assertGreaterEqual(worst, 1500)
