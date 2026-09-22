"""`unchanged_correction` 的**接线**测试——判据函数早有单测，接线没有。

这个区分不是形式主义：给「路由模型不存在」写接线测试时，正是它抓出了两个
只有执行才会现形的运行期错误（`llm_proxy` 里既没有 `import re` 也没有 `log`）。
读代码读不出那种东西。

而且今天本机跑的两次真实运行**都没碰到这条分支**：它们的回复整份没有文件块，
于是 `files` 为空，走的是另一条路。所以这条改动此前没有任何端到端证据。
"""
import tempfile
import unittest
from contextlib import contextmanager
from pathlib import Path

import main


class _Driver:
    @contextmanager
    def without_tools(self):
        yield


class _Proxy:
    def __init__(self):
        self.no_tools = False
        self.system_override = None
        self.mode = "passthrough"


class UnchangedCorrectionWiringTests(unittest.TestCase):
    def _flow(self, tmp: Path, reply: str):
        flow = main.Flow.__new__(main.Flow)
        flow.llm_proxy = _Proxy()
        flow.driver = _Driver()
        flow.output_dir = tmp
        flow.pending_corrections = []
        flow.codegen_quoted = None          # drop_unseen_rewrites: None = 不拦
        flow.base_reasoning_mode = "passthrough"
        flow.last_codegen_wrote = None
        flow.codegen_reasoning = lambda _chars: None
        flow.turn = lambda *a, **k: (True, reply)
        return flow

    @staticmethod
    def _block(path: str, body: str) -> str:
        return f"<<<FILE {path}>>>\n{body}<<<END FILE>>>\n"

    def test_all_identical_appends_the_correction(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "a.js").write_text("const a = 1;\n", encoding="utf-8")
            (root / "b.js").write_text("const b = 2;\n", encoding="utf-8")
            reply = self._block("a.js", "const a = 1;\n") + self._block("b.js", "const b = 2;\n")
            flow = self._flow(root, reply)
            ok, _ = flow.codegen_turn("p", 10, "REQ-1 repair 1/3")
            self.assertTrue(ok, "原样吐回仍算轮次完成，不该变成失败")
            self.assertEqual(len(flow.pending_corrections), 1, flow.pending_corrections)
            self.assertIn("byte-for-byte identical", flow.pending_corrections[0])
            self.assertTrue(flow.last_codegen_wrote, "文件确实写了（内容相同），标记应为 True")

    def test_one_real_change_stays_quiet(self):
        """改了一个、原样吐回一个 = 有进展。此时再加更正，会把一个正在推进的节点推去怀疑自己。"""
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "a.js").write_text("const a = 1;\n", encoding="utf-8")
            (root / "b.js").write_text("const b = 2;\n", encoding="utf-8")
            reply = self._block("a.js", "const a = 1;\n") + self._block("b.js", "const b = 99;\n")
            flow = self._flow(root, reply)
            ok, _ = flow.codegen_turn("p", 10, "REQ-1 repair 1/3")
            self.assertTrue(ok)
            self.assertEqual(flow.pending_corrections, [])

    def test_new_file_is_not_an_unchanged_rewrite(self):
        """盘上没有的文件是新建，不该被算成「原样吐回」。"""
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            flow = self._flow(root, self._block("fresh.js", "const c = 3;\n"))
            ok, _ = flow.codegen_turn("p", 10, "REQ-1 implement")
            self.assertTrue(ok)
            self.assertEqual(flow.pending_corrections, [])
            self.assertTrue((root / "fresh.js").is_file())

    def test_no_blocks_marks_nothing_written(self):
        """另一条分支：整份没有文件块时，last_codegen_wrote 必须是 False——
        『失败相同是否算证据』那个判据就靠它。"""
        with tempfile.TemporaryDirectory() as tmp:
            flow = self._flow(Path(tmp), "```json\n{}\n```")
            ok, text = flow.codegen_turn("p", 10, "REQ-1 repair 1/3")
            self.assertFalse(ok)
            self.assertIs(flow.last_codegen_wrote, False)
            self.assertIn("no <<<FILE>>> blocks", text)


if __name__ == "__main__":
    unittest.main()
