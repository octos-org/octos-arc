"""The generated .dot must stay DAG-schedulable.

Only the kernel's DAG scheduler (OCTOS_PIPELINE_DAG=1) does back-edge retries
and hands a failing node's output back to its target — which IS the repair
round. `graph_is_dag_schedulable` silently demotes a graph to the legacy
single-path walk if it uses a feature the scheduler cannot route, and a demoted
graph would never repair anything. These tests pin the invariants that keep the
graph eligible, so a future prompt/attr tweak cannot quietly lose the loop.
"""
import re
import unittest

import main


def tree(children):
    return {"id": "ROOT", "name": "T", "type": "FOLDER", "children": children}


def atomic(node_id, deps=()):
    return {"id": node_id, "type": "ATOMIC", "name": node_id,
            "description": f"build {node_id}", "dependencies": list(deps)}


POLICY = dict(name="arc_build", repairs=5, node_timeout=1200, verify_timeout=900,
              max_iterations=40, run_timeout=3600, tools="read_file,write_file",
              reasoning="none", max_output_tokens=65536)


def build(nodes_spec):
    nodes = main.atomic_nodes(tree(nodes_spec))
    specs = {str(n["id"]): [] for n in nodes}
    return main.build_pipeline(nodes, specs, None, "/tmp/out", POLICY, [43100])


VALID_TOKEN = re.compile(r"[A-Za-z0-9_.:-]+")


def template_refs(dot):
    """Mirror validate.rs::extract_template_refs + is_template_ref_token."""
    refs, rest = [], dot
    while "{" in rest:
        rest = rest.split("{", 1)[1]
        if "}" not in rest:
            break
        body, rest = rest.split("}", 1)
        body = body.strip()
        if body and VALID_TOKEN.fullmatch(body):
            refs.append(body)
    return refs


EDGE = re.compile(r"^\s{4}(\w+) -> (\w+)(?:\s*\[(.*)\])?$", re.M)


class PipelineDot(unittest.TestCase):
    def test_has_literal_start_node_so_rule_1_survives_the_back_edge(self):
        # find_start_node() does NOT discount back-edges: the back-edge gives
        # the first implement node an incoming edge, so without a node named
        # `start` validation fails with "no start node found".
        dot = build([atomic("REQ-1")])
        self.assertIn('start [handler="noop"', dot)

    def test_forward_edges_carry_no_label_and_no_weight(self):
        # A forward edge with a label or a non-default weight is routing the
        # DAG firing logic does not implement -> demotion to the legacy walk.
        dot = build([atomic("REQ-1"), atomic("REQ-2", deps=["REQ-1"])])
        for src, dst, attrs in EDGE.findall(dot):
            if "condition=" in (attrs or ""):
                continue  # back-edge, checked separately
            self.assertEqual(attrs or "", "", f"forward edge {src}->{dst} must be bare")

    def test_back_edge_condition_carries_a_retry_marker(self):
        # validate::has_back_edge_marker looks for retry/back_edge/guard_back in
        # the label or condition; without it the cycle is rejected outright.
        dot = build([atomic("REQ-1")])
        conds = [a for _, _, a in EDGE.findall(dot) if a and "condition=" in a]
        self.assertTrue(conds, "expected a failure back-edge")
        for cond in conds:
            self.assertRegex(cond.lower(), r"retry|back_edge|back-edge|guard_back")
            self.assertIn('outcome.status == \\"fail\\"', cond)

    def test_uses_no_handler_the_dag_scheduler_refuses(self):
        dot = build([atomic("REQ-1"), atomic("REQ-2", deps=["REQ-1"])])
        for banned in ('handler="parallel"', 'handler="dynamic_parallel"',
                       "converge=", "suggested_next="):
            self.assertNotIn(banned, dot)

    def test_acceptance_node_is_a_shell_check_with_the_repair_budget(self):
        dot = build([atomic("REQ-1")])
        self.assertIn('handler="shell_check"', dot)
        self.assertIn('max_retries="5"', dot)
        self.assertIn("verify_node.py", dot)

    def test_nodes_are_chained_in_dependency_order(self):
        dot = build([atomic("REQ-2", deps=["REQ-1"]), atomic("REQ-1")])
        self.assertLess(dot.index("impl_n_REQ_1 "), dot.index("impl_n_REQ_2 "))
        # REQ-2's implement node hangs off REQ-1's acceptance node.
        self.assertIn("check_n_REQ_1 -> impl_n_REQ_2", dot)

    def test_quoted_spec_braces_are_not_parsed_as_template_variables(self):
        # A Playwright excerpt contains `async ({ page }) => {`. validate.rs
        # reads `{ page }` as a template variable and rejects the whole graph
        # as unbound -- observed killing a real run before any node executed.
        self.assertEqual(main.untemplate("async ({ page }) => {"),
                         "async ({{ page }}) => {{")
        self.assertEqual(template_refs("prompt=\"" + main.untemplate("({ page })") + "\""), [])
        # ...while a genuine, intentionally-bound variable still reads as one.
        self.assertEqual(template_refs("prompt=\"use {input} here\""), ["input"])

    def test_node_ids_are_sanitised_into_legal_dot_identifiers(self):
        dot = build([atomic("REQ-1.2")])
        self.assertIn("impl_n_REQ_1_2", dot)
        self.assertNotIn("impl_n_REQ-1.2", dot)


if __name__ == "__main__":
    unittest.main()
