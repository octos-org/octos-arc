import unittest
from scoreboard import board_table, summary_table, cny, render

class OfficialRankingTests(unittest.TestCase):
    def test_should_keep_low_cost_leader_ahead_of_our_entry(self):
        board = [{'username': 'leader', 'total_token_cost': 0.000072, 'avg_runtime_seconds': 0},
                 {'username': 'octos', 'total_token_cost': 0.005154, 'avg_runtime_seconds': 22}]
        summary = summary_table({'smoke': board}, [], ['octos'])
        self.assertIn('2/2', summary)
        self.assertNotIn('预生成', summary)
        self.assertNotIn('真实', summary)
        table = board_table('smoke', board, ['octos'], 10)
        self.assertIn('**octos**', table)
        self.assertNotIn('预生成', table)
        self.assertIn('2/2', render({'smoke': board}, [], ['octos'], 10))

    def test_should_preserve_tiny_costs_instead_of_rounding_to_zero(self):
        self.assertEqual(cny(0.000024), '¥0.000024')
        self.assertEqual(cny(0.000072), '¥0.000072')
