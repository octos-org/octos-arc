"""单节点时间上限：它的作用不该是「把快节点省下的余额锁住」。

keep @ D 的实测（2026-09-19）——全程预算 48,000s 只用掉 30,240s，**剩 37% 没用**，
而六个失败节点各自只超出单节点上限 13–18 秒就被砍断：

    implement 900s（0.6 × 1500 的上限）+ 验收循环 613s = 1513s > 1500s
    → `left < min_repair_seconds(300)` → 修复停在 round 1 → 判负

六个节点在 37% 的余额面前，因为差十几秒而失败。
"""
import unittest


def node_budget(cap, remaining, nodes_left):
    """复刻 main.py 里那一行的**形状**（这里测的是取值关系，不是那一行本身）。"""
    return min(cap, max(240, remaining / nodes_left))


class NodeBudgetCapTests(unittest.TestCase):
    def test_default_is_high_enough_for_implement_plus_two_repairs(self):
        """implement 被 node_timeout(1200) 卡住，剩下的要够两轮修复（实测每轮约 600s）。"""
        import main
        import inspect
        src = inspect.getsource(main.Flow.__init__)
        self.assertIn('"OCTOS_NODE_TIME_BUDGET", "3000"', src)
        cap, node_timeout, repair = 3000, 1200, 600
        self.assertGreaterEqual(cap - node_timeout, repair * 2,
                                "implement 顶满之后，余额要够两轮修复")

    def test_old_default_could_not_afford_a_second_repair(self):
        """把旧值代进去，重现 keep 上那六个节点的算术。"""
        cap, implement, loop, min_repair = 1500, 900, 613, 300
        self.assertGreater(implement + loop, cap, "1513 > 1500，超时")
        self.assertLess(cap - implement - loop, min_repair, "剩余不足一次修复所需")

    def test_the_adaptive_term_still_protects_later_nodes(self):
        """抬高上限**不等于**放任：超支后 remaining/nodes_left 会自动把份额压下去。"""
        self.assertAlmostEqual(node_budget(3000, 48000, 32), 1500)       # 开局：份额本来就是 1500
        self.assertAlmostEqual(node_budget(3000, 40000, 10), 3000)       # 中段有余额 → 用到上限
        self.assertAlmostEqual(node_budget(3000, 3000, 10), 300)         # 超支后自动缩到 300
        self.assertAlmostEqual(node_budget(3000, 100, 10), 240)          # 地板 240 仍在

    def test_cap_is_still_a_cap(self):
        """不是取消上限：余额再多，单个节点也拿不到超过 cap。"""
        self.assertAlmostEqual(node_budget(3000, 900000, 2), 3000)
