"""group_nodes() merges true siblings (independent of each other, deps already
satisfied by an earlier group) into one implement+accept round -- fewer
sequential turns on a requirement tree with real branching, not just a chain.
Safety invariant: a node never joins a group containing something it depends
on; it only joins once that dependency's group has already closed.
"""
import re
import unittest

import main


def tree(children):
    return {"id": "ROOT", "name": "T", "type": "FOLDER", "children": children}


def atomic(node_id, deps=(), desc_chars=20):
    return {"id": node_id, "type": "ATOMIC", "name": node_id,
            "description": "x" * desc_chars, "dependencies": list(deps)}


GROUP_POLICY = dict(name="arc_build", repairs=5, repair_window=1800, node_timeout=1200,
                     verify_timeout=900, max_iterations=40, run_timeout=3600,
                     tools="read_file,write_file", reasoning="none", max_output_tokens=65536,
                     node_budget=600, min_node_seconds=120, final_reserve_seconds=600,
                     final_repairs=2, context_window=0, llm_timeout=900,
                     node_max_output_tokens=32768,
                     group_requirements=1, group_max_chars=2600, group_max_members=3)


def build(nodes_spec, pol=GROUP_POLICY):
    nodes = main.atomic_nodes(tree(nodes_spec))
    return main.build_pipeline(nodes, "/tmp/out", pol, [43100], 1e10)


EDGE = re.compile(r"^\s{4}(\w+) -> (\w+)(?:\s*\[(.*)\])?$", re.M)


def edges(dot):
    order = {m.group(1): i for i, m in enumerate(re.finditer(r"^\s{4}(\w+) \[", dot, re.M))}
    return [(s, d, a, order[d] <= order[s]) for s, d, a in EDGE.findall(dot)]


class GroupNodes(unittest.TestCase):
    def test_independent_siblings_merge(self):
        # B and C both depend only on A (already closed by the time either is
        # considered) and are independent of each other -- true siblings.
        nodes = main.atomic_nodes(tree([atomic("A"), atomic("B", deps=["A"]), atomic("C", deps=["A"])]))
        groups = main.group_nodes(nodes, max_chars=2600, max_members=3)
        ids = [[str(n["id"]) for n in g] for g in groups]
        self.assertEqual(ids, [["A"], ["B", "C"]])

    def test_node_never_joins_a_group_containing_its_own_dependency(self):
        # B depends on A: merging them would ask one turn to build B on top of
        # A before A is known to pass its own check. Must stay 2 rounds.
        nodes = main.atomic_nodes(tree([atomic("A"), atomic("B", deps=["A"])]))
        groups = main.group_nodes(nodes, max_chars=999999, max_members=99)
        ids = [[str(n["id"]) for n in g] for g in groups]
        self.assertEqual(ids, [["A"], ["B"]])

    def test_zero_dependency_roots_can_still_share_a_group(self):
        nodes = main.atomic_nodes(tree([atomic("A"), atomic("B")]))
        groups = main.group_nodes(nodes, max_chars=2600, max_members=3)
        self.assertEqual([str(n["id"]) for n in groups[0]], ["A", "B"])

    def test_char_cap_splits_an_otherwise_mergeable_pair(self):
        nodes = main.atomic_nodes(tree([atomic("A", desc_chars=2000), atomic("B", desc_chars=2000)]))
        groups = main.group_nodes(nodes, max_chars=2600, max_members=3)
        self.assertEqual(len(groups), 2)

    def test_member_cap_splits_a_larger_sibling_cluster(self):
        nodes = main.atomic_nodes(tree([atomic("A"), atomic("B", deps=["A"]),
                                        atomic("C", deps=["A"]), atomic("D", deps=["A"])]))
        groups = main.group_nodes(nodes, max_chars=999999, max_members=2)
        sizes = sorted(len(g) for g in groups)
        self.assertEqual(sizes, [1, 1, 2])  # A alone, {B,C} merged, D alone (member cap = 2)

    def test_disabled_is_one_node_per_requirement(self):
        nodes = main.atomic_nodes(tree([atomic("A"), atomic("B", deps=["A"]), atomic("C", deps=["A"])]))
        groups = main.group_nodes(nodes, max_chars=0, max_members=1)
        self.assertEqual([len(g) for g in groups], [1, 1, 1])


class GroupedPipelineDot(unittest.TestCase):
    def test_sibling_group_produces_one_impl_and_one_check_node(self):
        dot = build([atomic("A"), atomic("B", deps=["A"]), atomic("C", deps=["A"])])
        # tag "B+C" -> sanitize() turns "+" into "_", giving one combined node.
        self.assertIn('    impl_n_B_C [', dot)
        self.assertIn('    check_n_B_C [', dot)
        self.assertNotIn("impl_n_B [", dot)
        self.assertNotIn("impl_n_C [", dot)

    def test_grouped_check_uses_e2e_list_not_e2e(self):
        dot = build([atomic("A"), atomic("B", deps=["A"]), atomic("C", deps=["A"])])
        check_line = next(l for l in dot.splitlines() if l.strip().startswith("check_n_B_C ["))
        self.assertIn("--e2e-list", check_line)
        self.assertIn("checks/B.mjs,checks/C.mjs", check_line)
        self.assertNotIn(" --e2e ", check_line)

    def test_singleton_group_keeps_single_e2e_flag(self):
        dot = build([atomic("A")])
        check_line = next(l for l in dot.splitlines() if l.strip().startswith("check_n_A ["))
        self.assertIn("--e2e checks/A.mjs", check_line)
        self.assertNotIn("--e2e-list", check_line)

    def test_grouped_prompt_is_each_members_own_full_body_back_to_back(self):
        # No bespoke group template: a group's prompt is N complete
        # single-requirement bodies, so neither member loses a rule the
        # single-node prompt states (nothing to keep in sync between two
        # templates).
        dot = build([atomic("A"), atomic("B", deps=["A"]), atomic("C", deps=["A"])])
        self.assertIn("checks/B.mjs", dot)
        self.assertIn("checks/C.mjs", dot)
        self.assertIn("Implement requirement B of this web application", dot)
        self.assertIn("Implement requirement C of this web application", dot)
        self.assertIn("Implement ALL together", dot)

    def test_graph_stays_dag_schedulable_and_quote_escaped(self):
        dot = build([atomic(f"REQ-{i}") for i in range(1, 13)])
        order = {m.group(1): i for i, m in enumerate(re.finditer(r"^\s{4}(\w+) \[", dot, re.M))}
        self.assertIn("start", order)
        for src, dst, attrs, back in edges(dot):
            if back:
                self.assertRegex((attrs or "").lower(), r"retry|back_edge|guard_back")
            else:
                self.assertNotIn("label=", attrs or "")
                self.assertNotIn("weight=", attrs or "")
            m = re.search(r'condition="(.*)"$', attrs or "")
            if m:
                self.assertNotIn('"', m.group(1).replace('\\"', ""),
                                 f"{src}->{dst} condition has an unescaped quote")
        node_decls = re.findall(r"^    \w+ \[", dot, re.M)
        self.assertLessEqual(len(node_decls), 40)

    def test_fewer_rounds_than_one_node_per_requirement(self):
        spec = [atomic("A"), atomic("B", deps=["A"]), atomic("C", deps=["A"]),
                atomic("D", deps=["A"]), atomic("E", deps=["B"])]
        grouped = build(spec)
        ungrouped_pol = dict(GROUP_POLICY, group_requirements=0)
        ungrouped = build(spec, ungrouped_pol)
        impl_count = lambda dot: len(re.findall(r"^    impl_\w+ \[", dot, re.M))  # noqa: E731
        self.assertLess(impl_count(grouped), impl_count(ungrouped))


if __name__ == "__main__":
    unittest.main()
