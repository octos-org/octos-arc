//! `octos arc`: the ARC-Bench harness.
//!
//! Two entry points live here. `octos arc <REQUIREMENTS> --mode create|evolve`
//! is the older runner (`runner.rs`, drives `octos chat`). `octos arc run
//! --spec runner-spec.json --policy arc-policy.toml` is the harness that
//! absorbed the strategy layer of the Python adapter (`arc/*.py`): requirement
//! tree, single-request codegen, local Playwright acceptance, repair loop,
//! budgets and a unified event stream the adapter translates into ARC's
//! runner-events / traceability records.

pub mod acceptance;
pub mod budget;
pub mod codegen;
pub mod driver;
pub mod envs;
pub mod events;
pub mod flow;
pub mod git;
pub mod guard;
pub mod llm;
pub mod pin;
pub mod plan;
pub mod policy;
mod process;
pub mod prompts;
pub mod reap;
pub mod routing;
pub mod run;
mod runner;
pub mod spec;
pub mod tree;
mod workspace;

pub use run::{RunCommand, RunnerSpec, execute_run};
pub use runner::{
    ArcCommand, ArcSubcommand, DenyProtectedCommand, Mode, execute, execute_deny_protected,
};
