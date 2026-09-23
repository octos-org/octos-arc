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


def atomic(node_id, deps=(), with_specs=False):
    return {"id": node_id, "type": "ATOMIC", "name": node_id, "with_specs": with_specs,
            "description": f"build {node_id}", "dependencies": list(deps)}


POLICY = dict(name="arc_build", repairs=5, repair_window=1800, node_timeout=1200, verify_timeout=900,
              max_iterations=40, run_timeout=3600, tools="read_file,write_file",
              reasoning="none", max_output_tokens=65536, node_budget=600,
              min_node_seconds=120, final_reserve_seconds=600, final_repairs=2,
              context_window=0, llm_timeout=900,
              node_max_output_tokens=32768, regression_every=4)


def build(nodes_spec):
    nodes = main.atomic_nodes(tree(nodes_spec))
    specs = {str(n["id"]): [] for n in nodes}
    if nodes_spec and nodes_spec[0].get("with_specs"):
        specs = {nid: [f"{nid}.spec.ts"] for nid in specs}
    return main.build_pipeline(nodes, specs, None, "/tmp/out", POLICY, [43100], 1e10)


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

    def edges(self, dot):
        """(src, dst, attrs, is_back): a back-edge closes a cycle to a node
        declared earlier in the file."""
        order = {m.group(1): i for i, m in enumerate(re.finditer(r"^\s{4}(\w+) \[", dot, re.M))}
        return [(s, d, a, order[d] <= order[s]) for s, d, a in EDGE.findall(dot)]

    def test_forward_edges_carry_no_label_and_no_weight(self):
        # A forward edge with a label or a non-default weight is routing the
        # DAG firing logic does not implement -> demotion to the legacy walk.
        dot = build([atomic("REQ-1"), atomic("REQ-2", deps=["REQ-1"])])
        for src, dst, attrs, back in self.edges(dot):
            self.assertNotIn("label=", attrs or "", f"{src}->{dst}")
            self.assertNotIn("weight=", attrs or "", f"{src}->{dst}")
            if not back:
                self.assertNotRegex((attrs or "").lower(), r"retry|back_edge|guard_back",
                                    f"forward edge {src}->{dst} must not look like a back-edge")

    def test_back_edge_condition_carries_a_retry_marker(self):
        # validate::has_back_edge_marker looks for retry/back_edge/guard_back in
        # the label or condition; without it the cycle is rejected outright.
        dot = build([atomic("REQ-1"), atomic("REQ-2", deps=["REQ-1"], with_specs=True)])
        backs = [a for _, _, a, back in self.edges(dot) if back]
        self.assertTrue(backs, "expected a failure back-edge")
        for cond in backs:
            self.assertRegex(cond.lower(), r"retry|back_edge|back-edge|guard_back")

    def test_repair_stops_on_the_verifier_marker(self):
        # Repairs are bounded by verify_node.py (attempts + deadline), not by
        # the scheduler's 10-run loop fuse.
        dot = build([atomic("REQ-1")])
        cond = [a for s, d, a, back in self.edges(dot) if back and d == "impl_n_REQ_1"][0]
        self.assertIn('outcome.status == \\"fail\\"', cond)
        self.assertIn(f'!outcome.contains(\\"{main.STOP}\\")', cond)
        self.assertIn("--attempts 6", dot)
        self.assertIn("--repair-window 1800", dot)

    def test_a_failed_requirement_does_not_prune_the_rest(self):
        # An unconditional edge out of a Fail is fail-closed: every later node
        # would be pruned. The edge on to the next requirement fires on both.
        dot = build([atomic("REQ-1"), atomic("REQ-2", deps=["REQ-1"])])
        fwd = [a for s, d, a, back in self.edges(dot) if s == "check_n_REQ_1" and d == "impl_n_REQ_2"]
        self.assertEqual(len(fwd), 1)
        self.assertIn('outcome.status == \\"pass\\"', fwd[0])
        self.assertIn('outcome.status == \\"fail\\"', fwd[0])
        self.assertIn('continue_on_error="true"', dot)

    def test_acceptance_runs_whatever_the_implement_node_ended_with(self):
        dot = build([atomic("REQ-1")])
        edge = [a for s, d, a, back in self.edges(dot) if s == "impl_n_REQ_1" and d == "check_n_REQ_1"][0]
        for status in ("pass", "fail", "error"):
            self.assertIn(f'outcome.status == \\"{status}\\"', edge)

    def test_worker_nodes_carry_reasoning_and_output_caps(self):
        # config.json's gateway section never reaches the profile runtime.
        dot = build([atomic("REQ-1")])
        line = next(l for l in dot.splitlines() if l.strip().startswith("impl_n_REQ_1 ["))
        self.assertIn('reasoning_effort="none"', line)
        self.assertIn('max_output_tokens="32768"', line)

    def test_every_fourth_check_is_a_regression_checkpoint(self):
        nodes = main.atomic_nodes(tree([atomic(f"REQ-{i}") for i in range(1, 10)]))
        specs = {str(n["id"]): [] for n in nodes}
        dot = main.build_pipeline(nodes, specs, None, "/tmp/out", POLICY, [43100], 1e10, "/tmp/map.json")
        checks = [l for l in dot.splitlines() if l.strip().startswith("check_n_REQ_")]
        with_regress = [l.split()[0] for l in checks if "--regress" in l]
        self.assertEqual(with_regress, ["check_n_REQ_4", "check_n_REQ_8"])

    def test_workspace_is_seeded_before_the_first_requirement(self):
        dot = build([atomic("REQ-1")])
        self.assertIn("start -> seed", dot)
        self.assertIn("seed -> impl_n_REQ_1", dot)
        self.assertIn("--seed", dot)

    def test_regression_pass_runs_every_spec_after_the_last_requirement(self):
        dot = build([atomic("REQ-1", with_specs=True), atomic("REQ-2", deps=["REQ-1"])])
        self.assertIn("check_n_REQ_2 -> check_all", dot)
        line = next(l for l in dot.splitlines() if l.strip().startswith("check_all ["))
        self.assertIn("REQ-1.spec.ts", line)
        self.assertIn("REQ-2.spec.ts", line)
        self.assertIn("fix_all -> check_all", dot)

    def test_uses_no_handler_the_dag_scheduler_refuses(self):
        dot = build([atomic("REQ-1"), atomic("REQ-2", deps=["REQ-1"])])
        for banned in ('handler="parallel"', 'handler="dynamic_parallel"',
                       "converge=", "suggested_next="):
            self.assertNotIn(banned, dot)

    def test_acceptance_node_is_a_shell_check_with_the_repair_budget(self):
        dot = build([atomic("REQ-1")])
        self.assertIn('handler="shell_check"', dot)
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


class CollectApp(unittest.TestCase):
    def test_delivers_the_best_full_suite_state_over_a_worse_final_one(self):
        import json, tempfile
        from pathlib import Path
        with tempfile.TemporaryDirectory() as tmp:
            data, out = Path(tmp) / "data", Path(tmp) / "out"
            run = data / "profiles" / "p" / "data" / "pipeline-runs" / "arc_build-1"
            for base, text in ((run, "broken"), (run / ".arc-best" / "app", "best")):
                (base / "frontend" / "src").mkdir(parents=True)
                (base / "frontend" / "src" / "index.html").write_text(text)
                (base / "backend").mkdir(parents=True)
            (run / ".arc-best" / "score.json").write_text(json.dumps({"passed": 6, "rc": 1}))
            (out / "frontend" / "src").mkdir(parents=True)
            (out / "frontend" / "src" / "stale.html").write_text("template")
            self.assertEqual(main.collect_app(data, out, "arc_build"), run)
            self.assertEqual((out / "frontend" / "src" / "index.html").read_text(), "best")
            self.assertFalse((out / "frontend" / "src" / "stale.html").exists())
