"""Performance work on this harness kept needing the same numbers by hand:
which mode the prompt tokens went to, how much of each request was tool
schemas, and how many turns approached the cut-off. `cost_profile` puts them
in the run summary so the next person does not recompute them."""
import unittest

from metrics import cost_profile


def rec(prompt, completion, tools=0, tools_chars=0, reasoning=0, cache=0, elapsed_ms=1000):
    return {"prompt_tokens": prompt, "completion_tokens": completion, "reasoning_tokens": reasoning,
            "prompt_cache_hit_tokens": cache, "elapsed_ms": elapsed_ms,
            "request": {"tools": tools, "tools_chars": tools_chars}}


class CostProfileTests(unittest.TestCase):
    def test_should_split_prompt_tokens_by_mode(self):
        """Two of bookstack's 34 nodes escalated to tool mode and took 201 of
        the run's 232 requests and 7.39M of its 7.83M prompt tokens. A summary
        that reports only the total hides where the money went."""
        p = cost_profile([rec(100, 50), rec(100, 50), rec(9000, 10, tools=67, tools_chars=73005)])
        self.assertEqual(p["codegen"]["requests"], 2)
        self.assertEqual(p["codegen"]["prompt_tokens"], 200)
        self.assertEqual(p["tool_mode"]["requests"], 1)
        self.assertEqual(p["tool_mode"]["prompt_tokens"], 9000)
        self.assertEqual(p["tool_mode"]["avg_tools_chars"], 73005)

    def test_should_report_the_reasoning_share_of_output(self):
        """A GLM turn once spent 32607 of its 32768 output tokens on reasoning
        and returned a truncated fragment; the share is the early warning."""
        p = cost_profile([rec(10, 1000, reasoning=900)])
        self.assertEqual(p["reasoning_pct"], 90)

    def test_should_count_turns_near_the_cut_off(self):
        """Turns cut at the 1200s cap lose their partial work outright, so the
        count belongs next to the totals rather than in a separate analysis."""
        p = cost_profile([rec(1, 1, elapsed_ms=1_195_000), rec(1, 1, elapsed_ms=5_000)])
        self.assertEqual(p["turns_near_cutoff"], 1)
        self.assertEqual(p["max_turn_s"], 1195)

    def test_should_be_empty_for_a_run_with_no_recorded_requests(self):
        p = cost_profile([])
        self.assertEqual(p["codegen"]["requests"], 0)
        self.assertEqual(p["tool_mode"]["requests"], 0)
        self.assertEqual(p["reasoning_pct"], 0)


if __name__ == "__main__":
    unittest.main()
