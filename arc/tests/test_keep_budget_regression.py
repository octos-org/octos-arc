"""把 keep @ D 真实丢掉的那六个节点，钉成一条回归测试。

2026-09-19，keep 官方 26/32。六个失败节点的失败方式完全相同，而且是**结构性**的：

    implement 轮 900s（0.6 × 节点上限 1500 的上限）
  + 验收循环   613s
  ─────────────────
    合计       1513s  >  节点上限 1500s   → 余额 13s < 一次修复所需 → 修复停在 round 1

同一时刻，整个运行还有 17,760 秒（37%）预算没用——被单节点硬上限锁住。
这条测试用它们**真实的**(第几个节点, 当时已耗时)去算预算，要求现在的参数够用。
三个参数一起守：`node_budget_cap`、`banked_surplus`、`repair_needs`。
任何一个被改回去，这里就会红。
"""
import unittest

import main

# (节点名, 第几个节点, 该节点开始时全程已耗时秒数) —— 取自运行 45e9c8f401d1 的事件时间戳
LOST_NODES = [
    ("REQ-2.3.1", 4, 1824),
    ("REQ-2.3.2", 5, 3342),
    ("REQ-2.3.3", 6, 4855),
    ("REQ-2.6.1", 12, 8020),
    ("REQ-2.7.1", 14, 9818),
    ("REQ-2.7.2", 15, 11331),
]
NODES = 32
RUN_BUDGET = 1500 * NODES          # OCTOS_TIME_BUDGET = max(3600, 1500 × 节点数)
OBSERVED_LOOP = 613               # 实测验收循环耗时
NODE_TIMEOUT = 1200               # implement 轮的硬上限（OCTOS_NODE_TIMEOUT 默认）
NEEDED = NODE_TIMEOUT + OBSERVED_LOOP   # 1813：implement 顶满 + 走完一轮验收


class _Flow:
    """只带预算计算需要的那几样。"""
    banked_surplus = main.Flow.banked_surplus

    def __init__(self, used, cap):
        self.budget = RUN_BUDGET
        self._used = used
        self.node_budget_cap = cap

    def remaining(self):
        return self.budget - self._used

    def node_budget(self, index, total):
        try:
            surplus = float(self.banked_surplus(index, total))
        except Exception:
            surplus = 0.0
        return min(self.node_budget_cap, max(240, self.remaining() / (total - index + 1) + surplus))


class KeepBudgetRegressionTests(unittest.TestCase):
    def test_current_parameters_give_all_six_enough(self):
        cap = int(main.os.environ.get("OCTOS_NODE_TIME_BUDGET", "3000"))
        short = []
        for name, idx, used in LOST_NODES:
            got = _Flow(used, cap).node_budget(idx, NODES)
            if got < NEEDED:
                short.append((name, round(got)))
        self.assertEqual(short, [], f"这些节点仍然拿不到 {NEEDED}s：{short}")

    def test_the_old_parameters_would_have_failed_all_six(self):
        """证明这条测试测的是真东西：回到旧参数（上限 1500、不动用结余），六个全部不够。"""
        short = []
        for name, idx, used in LOST_NODES:
            f = _Flow(used, 1500)
            old = min(1500, max(240, f.remaining() / (NODES - idx + 1)))   # 无结余
            if old < NEEDED:
                short.append(name)
        self.assertEqual(len(short), 6, "旧参数本应六个全部不够，否则这条测试没有意义")

    def test_raising_the_cap_alone_is_not_enough(self):
        """只抬上限救得了 3 个，救不了开局那 3 个——这是我核对过才发现的，
        当时差点把『全都救回来』写进结论。"""
        saved = 0
        for name, idx, used in LOST_NODES:
            f = _Flow(used, 3000)
            cap_only = min(3000, max(240, f.remaining() / (NODES - idx + 1)))   # 无结余
            saved += cap_only >= NEEDED
        self.assertEqual(saved, 3, "只抬上限应当只救 3 个")

    def test_surplus_is_what_saves_the_early_ones(self):
        """开局那三个靠的是『动用已省下的结余』，不是上限。"""
        for name, idx, used in LOST_NODES[:3]:
            f = _Flow(used, 3000)
            share = f.remaining() / (NODES - idx + 1)
            self.assertLess(share, NEEDED, f"{name}: 光靠份额不够")
            self.assertGreaterEqual(f.node_budget(idx, NODES), NEEDED, f"{name}: 加上结余后才够")

    def test_an_overspending_run_is_not_given_more(self):
        """进度落后时结余为 0——这条改动不能让一个已经超支的运行继续超支。"""
        f = _Flow(RUN_BUDGET - 1000, 3000)      # 只剩 1000s，严重超支
        self.assertEqual(f.banked_surplus(10, NODES), 0.0)
