//! Mode selection for a run (`Flow.codegen_mode`, `minimal_mode`, skeleton
//! and design gates, reasoning auto level, contract keyword gates).

use eyre::{Result, bail};
use serde_json::Value;

use crate::llm::ReasoningMode;
use crate::policy::Policy;

#[derive(Debug, Clone)]
pub struct RunPlan {
    pub n_nodes: usize,
    pub nodes_to_implement: usize,
    pub evolution: bool,
    /// One-request codegen turns for the whole tree.
    pub codegen: bool,
    /// Minimal self-verification text / no shell in tool turns.
    pub minimal_verify: bool,
    pub wants_skeleton: bool,
    pub design_enabled: bool,
    pub design_inline: bool,
    pub base_reasoning: ReasoningMode,
    pub implement_reasoning: ReasoningMode,
    pub needs_session: bool,
    pub needs_data: bool,
    pub time_budget_seconds: u64,
}

fn parse_mode(text: &str) -> Result<ReasoningMode> {
    match ReasoningMode::parse(text) {
        Some(mode) => Ok(mode),
        None => bail!("unknown reasoning mode {text:?}"),
    }
}

impl RunPlan {
    pub fn new(
        policy: &Policy,
        tree: &Value,
        n_nodes: usize,
        nodes_to_implement: usize,
        evolution: bool,
    ) -> Result<Self> {
        let text = serde_json::to_string(tree)
            .unwrap_or_default()
            .to_lowercase();
        let minimal_verify = policy.mode.verify_mode == "minimal"
            || (policy.mode.verify_mode != "full" && n_nodes <= policy.mode.small_task_nodes);
        let design_enabled = policy.mode.design_turn && n_nodes >= policy.mode.design_min_nodes;
        let codegen = policy.mode.codegen && n_nodes <= policy.mode.codegen_max_nodes;
        let mut plan = Self {
            n_nodes,
            nodes_to_implement,
            evolution,
            codegen,
            minimal_verify,
            // Round 35: in codegen mode the harness manifests replace the skeleton turn.
            wants_skeleton: !evolution
                && (n_nodes >= policy.mode.skeleton_min_nodes || policy.mode.skeleton_always)
                && (!codegen || policy.mode.skeleton_always),
            design_enabled,
            design_inline: design_enabled && policy.mode.design_mode == "inline",
            base_reasoning: ReasoningMode::Low,
            implement_reasoning: ReasoningMode::Low,
            needs_session: policy
                .prompts
                .session_keywords
                .iter()
                .any(|k| text.contains(&k.to_lowercase())),
            needs_data: policy
                .prompts
                .data_keywords
                .iter()
                .any(|k| text.contains(&k.to_lowercase())),
            time_budget_seconds: policy.time_budget_for(n_nodes),
        };
        plan.set_nodes_to_implement(policy, nodes_to_implement)?;
        Ok(plan)
    }

    /// Reasoning auto level: thinking off for one-node builds/evolutions,
    /// low otherwise; the implement override applies to small tasks only.
    pub fn set_nodes_to_implement(
        &mut self,
        policy: &Policy,
        nodes_to_implement: usize,
    ) -> Result<()> {
        self.nodes_to_implement = nodes_to_implement;
        self.base_reasoning = if policy.reasoning.mode == "auto" {
            if nodes_to_implement <= 1 {
                ReasoningMode::Disabled
            } else {
                ReasoningMode::Low
            }
        } else {
            parse_mode(&policy.reasoning.mode)?
        };
        self.implement_reasoning =
            if !policy.reasoning.implement_override.is_empty() && self.minimal_verify {
                parse_mode(&policy.reasoning.implement_override)?
            } else {
                self.base_reasoning
            };
        Ok(())
    }

    /// Reasoning for a turn label (`Flow.turn`: the implement override only
    /// applies to implement / skeleton turns).
    pub fn reasoning_for(&self, label: &str) -> ReasoningMode {
        if label.ends_with(" implement") || label.starts_with("skeleton") {
            self.implement_reasoning
        } else {
            self.base_reasoning
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn should_pick_codegen_and_no_reasoning_for_one_node_trees() {
        let policy = Policy::default();
        let tree = json!({"id": "ROOT", "children": [{"id": "REQ-1", "description": "a counter"}]});
        let plan = RunPlan::new(&policy, &tree, 1, 1, false).unwrap();
        assert!(
            plan.codegen && plan.minimal_verify && !plan.wants_skeleton && !plan.design_enabled
        );
        assert_eq!(plan.base_reasoning, ReasoningMode::Disabled);
        assert_eq!(
            plan.reasoning_for("REQ-1 implement"),
            ReasoningMode::Disabled
        );
        assert!(!plan.needs_session && !plan.needs_data);
        assert_eq!(plan.time_budget_seconds, 3600);
    }

    #[test]
    fn should_pick_low_reasoning_and_keyword_contracts_for_two_node_trees() {
        let mut policy = Policy::default();
        policy.reasoning.implement_override = "none".into();
        let tree = json!({"id": "ROOT", "children": [{"id": "REQ-1", "description": "登录 with a dropdown"}, {"id": "REQ-2"}]});
        let plan = RunPlan::new(&policy, &tree, 2, 2, false).unwrap();
        assert!(plan.codegen && plan.minimal_verify);
        assert_eq!(plan.base_reasoning, ReasoningMode::Low);
        assert_eq!(
            plan.reasoning_for("REQ-1 implement"),
            ReasoningMode::Disabled
        );
        assert_eq!(plan.reasoning_for("REQ-1 repair 1/5"), ReasoningMode::Low);
        assert!(plan.needs_session && plan.needs_data);
    }

    #[test]
    fn should_use_tool_mode_skeleton_and_inline_design_for_large_trees() {
        let policy = Policy::default();
        let tree = json!({"id": "ROOT"});
        let plan = RunPlan::new(&policy, &tree, 32, 32, false).unwrap();
        // Round 35: every tree size takes codegen; the manifests replace the skeleton turn.
        assert!(
            plan.codegen
                && !plan.minimal_verify
                && !plan.wants_skeleton
                && plan.design_enabled
                && plan.design_inline
        );
        let mut tool_policy = Policy::default();
        tool_policy.mode.codegen_max_nodes = 2;
        let tool = RunPlan::new(&tool_policy, &tree, 32, 32, false).unwrap();
        assert!(!tool.codegen && tool.wants_skeleton);
        assert_eq!(plan.time_budget_seconds, 48000);
        let evolution = RunPlan::new(&policy, &tree, 32, 1, true).unwrap();
        assert!(!evolution.wants_skeleton);
        assert_eq!(evolution.base_reasoning, ReasoningMode::Disabled);
    }
}
