//! octos-cli library surface.
//!
//! The crate primarily exposes a binary (`octos`) but a few modules are also
//! surfaced here so integration tests can drive real code paths. Keep the
//! public API narrow — only items that integration tests or sibling crates
//! consume.

#[cfg(feature = "api")]
// keeps resolving unchanged.
#[cfg(feature = "api")]
pub mod api;
pub mod approvals_audit;
pub mod auth;
// Build-cache pool (outer-loop #3, design docs/build-cache-pool.md):
// per-repository reusable cargo target-dir slots with flock exclusivity,
// holder metadata for crash recovery, and a fail-closed space gate.
// Deliberately NOT `api`-gated: `octos cache …` commands (#5) and peer
// staging (#4) both need it in unfeatured builds.
pub mod build_cache;
/// task-return-unconsumed-steer-inputs: feature-independent shape of the
/// `turn/steer_dropped` return (the `api` module does the sending).
#[cfg_attr(not(feature = "api"), allow(dead_code))]
pub(crate) mod steer_return;
/// task-interrupt-breaks-progress-wait: the standalone-turn loop's next-step
/// race (interrupt vs progress), kept feature-independent so it is testable.
#[cfg_attr(not(feature = "api"), allow(dead_code))]
pub(crate) mod turn_loop;
/// task-turn-interrupt-steer-correlation-logs: session/turn-correlated
/// lifecycle logging for turn/interrupt and turn/steer (the `api` module
/// calls these; kept feature-independent so the shape is testable).
#[cfg_attr(not(feature = "api"), allow(dead_code))]
pub(crate) mod turn_trace;

pub mod commands;
pub mod config;
pub mod config_context;
pub mod config_layer;
#[path = "api/context_manager.rs"]
pub(crate) mod context_manager;
// Interactive-contract stores (pending approvals / user questions / diff
// previews / approval scopes). Deliberately NOT `api`-gated: they are plain
// in-memory registries over `octos_core::ui_protocol` types with no axum /
// AppState / WebSocket dependency, and `octos chat --peers` needs the SAME
// process-global `contract_stores()` the serve WS path uses so a peer's parked
// oneshot and the master's `peer_respond` meet in one registry.
//
// Most of the surface is still reached only from the `api` WS handlers, so an
// unfeatured build sees it as dead — an artifact of the consumer being absent,
// not of the code being unreachable (same rationale as `autonomy`).
#[cfg_attr(not(feature = "api"), allow(dead_code))]
pub(crate) mod contracts;
#[cfg(feature = "api")]
#[cfg_attr(not(feature = "api"), allow(dead_code))]
pub(crate) mod obs_events;
#[cfg(feature = "api")]
// Peer recovery is also used by gateway actors without `api`. The remaining
// staging and OUP transport helpers are intentionally dormant in that build.
#[cfg_attr(not(feature = "api"), allow(dead_code))]
pub mod profiles;
mod qos_catalog;
pub mod runtime;
pub mod session_actor;
pub mod skills_scope;
