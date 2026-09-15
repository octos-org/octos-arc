//! Typed retry-bucket state machine for the agent loop (M6.2, issue #489).
//!
//! This is the decision layer layered on top of M6.1's `HarnessError` taxonomy.
//! Whereas `classify_loop_error` turns raw `eyre::Report` into a typed variant
//! with a primary [`RecoveryHint`], [`LoopRetryState`] decides whether the
//! *next* loop iteration should continue, compact context, rotate the
//! credential/provider lane, escalate, or fire a single free grace call past
//! hard budget. Every decision is deterministic.
//!
//! Design goals (per issue #489):
//!   1. Each [`HarnessError`] variant maps to exactly one typed counter with a
//!      bounded limit. Exhausting a bucket never silently loops.
//!   2. The state survives context compaction via `serde` round-trip.
//!   3. A single "budget-grace-call" may fire past `max_iterations` iff the
//!      loop produced at least one productive tool call since the last grace.
//!   4. The existing shell-spiral recovery (formerly the free-standing
//!      `recover_shell_retry` helper) routes through the same machine via
//!      [`LoopRetryState::observe_shell_spiral`], so behavior matches byte-for-byte.
//!   5. Every observation emits a typed `HarnessEventPayload::Retry` event and
//!      increments `octos_loop_retry_total{variant, decision}`.
//!
//! Invariants enforced in unit tests (see `tests/loop_retry_state.rs`):
//!   - `should_escalate_after_invalid_tool_call_limit`
//!   - `should_compact_on_context_overflow_decision`
//!   - `should_fire_grace_call_at_budget_exhaustion_with_productive_history`
//!   - `should_not_fire_grace_call_without_productive_history`
//!   - `should_serde_round_trip_loop_retry_state`
//!   - `should_preserve_shell_spiral_recovery_behavior`

use std::fmt;

use metrics::counter;
use serde::{Deserialize, Serialize};

use crate::harness_errors::{HarnessError, RecoveryHint};
use crate::harness_events::{
    HARNESS_EVENT_SCHEMA_V1, HarnessEvent, HarnessEventPayload, HarnessRetryEvent,
};

/// Prometheus counter name for loop-level retry decisions. Labels:
/// `{variant, decision}` — both are stable snake_case identifiers.
pub const OCTOS_LOOP_RETRY_TOTAL: &str = "octos_loop_retry_total";

// ── Default per-bucket limits ───────────────────────────────────────────────
//
// Limits are intentionally conservative; each bucket has to trigger `Exhausted`
// strictly before an unbounded runaway sets in. The numbers are tuned so that
// transient failures (network blips, rate-limit bursts) get a few reasonable
// retries while structural problems (auth, malformed tool calls, invalid
// schemas) escalate quickly.

const DEFAULT_RATE_LIMIT_LIMIT: u32 = 5;
const DEFAULT_CONTEXT_OVERFLOW_LIMIT: u32 = 2;
const DEFAULT_AUTHENTICATION_LIMIT: u32 = 1;
/// Quota errors are operator-action — auto-retry will keep failing until
/// the operator tops up. Cap at 1 like Authentication.
const DEFAULT_QUOTA_LIMIT: u32 = 1;

/// `#[serde(default = "...")]` helper for `LoopRetryLimits::quota`. Lets
/// legacy retry-state JSON (pre-quota field) deserialize cleanly with the
/// canonical default instead of `0`, which would disable the bucket.
fn default_quota_limit() -> u32 {
    DEFAULT_QUOTA_LIMIT
}
const DEFAULT_INVALID_REQUEST_LIMIT: u32 = 2;
const DEFAULT_CONTENT_FILTERED_LIMIT: u32 = 1;
const DEFAULT_PROVIDER_UNAVAILABLE_LIMIT: u32 = 4;
const DEFAULT_NETWORK_LIMIT: u32 = 4;
const DEFAULT_TIMEOUT_LIMIT: u32 = 3;
const DEFAULT_TOOL_EXECUTION_LIMIT: u32 = 5;
const DEFAULT_INTERNAL_LIMIT: u32 = 1;
/// Hook-deny (policy) errors escalate immediately — retrying a policy
/// decision can never succeed (#2249).
const DEFAULT_POLICY_LIMIT: u32 = 1;
const DEFAULT_SHELL_SPIRAL_LIMIT: u32 = 1;

/// `#[serde(default = "...")]` helper for `LoopRetryLimits::policy`. Lets
/// legacy retry-state JSON (pre-policy field) deserialize cleanly with the
/// canonical default instead of `0`, which would disable the bucket.
fn default_policy_limit() -> u32 {
    DEFAULT_POLICY_LIMIT
}

/// Per-bucket hard limits. Tuned for M6.2 defaults, exposed so integration
/// tests and operators can override them if needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopRetryLimits {
    pub rate_limited: u32,
    pub context_overflow: u32,
    pub authentication: u32,
    /// Added in codex round-3 (Quota variant). `serde(default)` keeps
    /// legacy retry-state sidecar JSON (pre-quota) deserializable.
    #[serde(default = "default_quota_limit")]
    pub quota: u32,
    pub invalid_request: u32,
    pub content_filtered: u32,
    pub provider_unavailable: u32,
    pub network: u32,
    pub timeout: u32,
    pub tool_execution: u32,
    pub internal: u32,
    /// Added with the PolicyDeny variant (#2249). `serde(default)` keeps
    /// legacy retry-state sidecar JSON (pre-policy) deserializable.
    #[serde(default = "default_policy_limit")]
    pub policy: u32,
    pub shell_spiral: u32,
}

impl Default for LoopRetryLimits {
    fn default() -> Self {
        Self {
            rate_limited: DEFAULT_RATE_LIMIT_LIMIT,
            context_overflow: DEFAULT_CONTEXT_OVERFLOW_LIMIT,
            authentication: DEFAULT_AUTHENTICATION_LIMIT,
            quota: DEFAULT_QUOTA_LIMIT,
            invalid_request: DEFAULT_INVALID_REQUEST_LIMIT,
            content_filtered: DEFAULT_CONTENT_FILTERED_LIMIT,
            provider_unavailable: DEFAULT_PROVIDER_UNAVAILABLE_LIMIT,
            network: DEFAULT_NETWORK_LIMIT,
            timeout: DEFAULT_TIMEOUT_LIMIT,
            tool_execution: DEFAULT_TOOL_EXECUTION_LIMIT,
            internal: DEFAULT_INTERNAL_LIMIT,
            policy: DEFAULT_POLICY_LIMIT,
            shell_spiral: DEFAULT_SHELL_SPIRAL_LIMIT,
        }
    }
}

/// The decision the retry layer returns to the agent loop after a failure
/// observation. Each decision has a stable snake_case name used in metrics and
/// structured events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopDecision {
    /// Retry without reshaping the prompt: the underlying failure is expected
    /// to clear on its own (rate limit burst, flaky network, slow tool).
    Continue,
    /// Swap provider/credential lane before the next call. Used for provider
    /// outages (5xx, stream aborts) where the current lane is sick but the
    /// task itself is still valid.
    RotateAndRetry,
    /// Compact the conversation (drop old messages, summarize) and retry. This
    /// is the only viable recovery for `ContextOverflow`.
    CompactAndRetry,
    /// Not retryable here — caller should surface the error and stop. Used
    /// for structural failures (auth, invalid request, content filter,
    /// delegation depth, tool faults, bugs).
    Escalate,
    /// Bucket exhausted: the same failure happened more times than the
    /// configured limit. Caller must treat this as a hard stop to avoid
    /// silent infinite loops (invariant #2 from #489).
    Exhausted,
    /// One free iteration past the hard iteration budget because the loop
    /// produced at least one productive tool call since the last grace. Once
    /// fired, cannot fire again until another productive call is recorded.
    Grace,
}

impl LoopDecision {
    /// Stable snake_case identifier used in metrics labels and structured
    /// event `message` fields. Never returns operator-supplied text.
    pub fn as_str(self) -> &'static str {
        match self {
            LoopDecision::Continue => "continue",
            LoopDecision::RotateAndRetry => "rotate_and_retry",
            LoopDecision::CompactAndRetry => "compact_and_retry",
            LoopDecision::Escalate => "escalate",
            LoopDecision::Exhausted => "exhausted",
            LoopDecision::Grace => "grace",
        }
    }
}

impl fmt::Display for LoopDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Stable snake_case identifier for the shell-spiral bucket. The spiral is
/// not a `HarnessError` variant but flows through the same state machine
/// so operators see one coherent retry surface.
pub const SHELL_SPIRAL_VARIANT: &str = "shell_spiral";

/// Per-variant counters. Each counter is bumped exactly once per observation
/// and the corresponding limit from [`LoopRetryLimits`] is checked immediately
/// so the caller never silently exceeds a bucket.
///
/// Fields are `pub` for direct reads and test/guard-path mutations, but the
/// delta-merge (`saturating_add_turn_delta`) relies on every bucket being
/// append-only: only ever increment, never decrement or reset (see its doc
/// comment).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopRetryCounters {
    pub rate_limited: u32,
    pub context_overflow: u32,
    pub authentication: u32,
    /// Added in codex round-3 (Quota variant). `serde(default)` keeps
    /// legacy retry-state sidecar JSON (pre-quota) deserializable; a
    /// missing field deserializes to `0`.
    #[serde(default)]
    pub quota: u32,
    pub invalid_request: u32,
    pub content_filtered: u32,
    pub provider_unavailable: u32,
    pub network: u32,
    pub timeout: u32,
    pub tool_execution: u32,
    pub internal: u32,
    /// Added with the PolicyDeny variant (#2249). `serde(default)` keeps
    /// legacy retry-state sidecar JSON (pre-policy) deserializable; a
    /// missing field deserializes to `0`.
    #[serde(default)]
    pub policy: u32,
    pub shell_spiral: u32,
}

impl LoopRetryCounters {
    /// Add another turn's per-bucket increments (`turn` relative to the
    /// `base` it loaded from) onto `self` (#1655). Buckets only ever
    /// increment, so `saturating_sub` yields the turn's own increments and
    /// two overlapping turns both land in the merged state.
    ///
    /// Monotonicity is a load-bearing convention here (#2221): if a future
    /// change ever decrements a bucket, `saturating_sub` clamps the negative
    /// delta to zero and the decrement is silently dropped from the merge.
    /// Bucket mutations must therefore only ever increment — all built-in
    /// mutations go through `observe*` / `observe_shell_spiral`, which do.
    fn saturating_add_turn_delta(&mut self, base: &Self, turn: &Self) {
        self.rate_limited = self
            .rate_limited
            .saturating_add(turn.rate_limited.saturating_sub(base.rate_limited));
        self.context_overflow = self
            .context_overflow
            .saturating_add(turn.context_overflow.saturating_sub(base.context_overflow));
        self.authentication = self
            .authentication
            .saturating_add(turn.authentication.saturating_sub(base.authentication));
        self.quota = self
            .quota
            .saturating_add(turn.quota.saturating_sub(base.quota));
        self.invalid_request = self
            .invalid_request
            .saturating_add(turn.invalid_request.saturating_sub(base.invalid_request));
        self.content_filtered = self
            .content_filtered
            .saturating_add(turn.content_filtered.saturating_sub(base.content_filtered));
        self.provider_unavailable = self.provider_unavailable.saturating_add(
            turn.provider_unavailable
                .saturating_sub(base.provider_unavailable),
        );
        self.network = self
            .network
            .saturating_add(turn.network.saturating_sub(base.network));
        self.timeout = self
            .timeout
            .saturating_add(turn.timeout.saturating_sub(base.timeout));
        self.tool_execution = self
            .tool_execution
            .saturating_add(turn.tool_execution.saturating_sub(base.tool_execution));
        self.internal = self
            .internal
            .saturating_add(turn.internal.saturating_sub(base.internal));
        self.policy = self
            .policy
            .saturating_add(turn.policy.saturating_sub(base.policy));
        self.shell_spiral = self
            .shell_spiral
            .saturating_add(turn.shell_spiral.saturating_sub(base.shell_spiral));
    }
}

/// Loop-level retry state machine. Owns one bounded counter per
/// [`HarnessError`] variant plus the shell-spiral synthetic bucket, and
/// tracks grace-call eligibility.
///
/// The state is entirely `serde`-serializable so that the compaction path
/// can round-trip it through the session ledger; see
/// `should_serde_round_trip_loop_retry_state`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopRetryState {
    #[serde(default)]
    pub counters: LoopRetryCounters,
    #[serde(default)]
    pub limits: LoopRetryLimits,
    /// Count of productive tool calls (success=true, non-error) recorded since
    /// the last grace call. Must be ≥ 1 for the next grace call to fire.
    #[serde(default)]
    pub productive_tool_calls_since_last_grace: u32,
    /// Number of grace calls fired so far. Useful for metrics and debugging;
    /// the decision logic only cares about productive_tool_calls_since_last_grace.
    #[serde(default)]
    pub grace_calls_fired: u32,
}

impl LoopRetryState {
    /// Construct a fresh state with the default per-bucket limits.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a state with explicit limits — useful for tests that need to
    /// drive a bucket to exhaustion quickly without relying on default tuning.
    pub fn with_limits(limits: LoopRetryLimits) -> Self {
        Self {
            counters: LoopRetryCounters::default(),
            limits,
            productive_tool_calls_since_last_grace: 0,
            grace_calls_fired: 0,
        }
    }

    /// Record a productive tool call (one whose `ToolResult.success` was true
    /// and produced meaningful output). Used to gate the grace-call pathway so
    /// that a stalled loop with no productive history does not get extra
    /// iterations past budget.
    pub fn record_productive_tool_call(&mut self) {
        self.productive_tool_calls_since_last_grace = self
            .productive_tool_calls_since_last_grace
            .saturating_add(1);
    }

    /// Classify a failure, bump the matching counter, and return the next
    /// decision. The decision is determined purely by the variant and the
    /// counter vs. limit comparison — it never depends on the error message,
    /// so the result is deterministic for the same variant.
    ///
    /// This is the canonical entry point for the retry layer; callers should
    /// pair it with [`Self::emit_event`] to make the decision observable.
    pub fn observe(&mut self, error: &HarnessError) -> LoopDecision {
        let (count, limit) = self.bump_counter(error);
        let decision = if count > limit {
            LoopDecision::Exhausted
        } else {
            decide_for_variant(error)
        };
        Self::record_metric(error.variant_name(), decision);
        decision
    }

    /// Observe a shell-spiral event (existing `recover_shell_retry` behavior).
    /// The state machine owns the counter so operators see one coherent retry
    /// ledger; the actual spiral detection lives in `loop_runner.rs`.
    ///
    /// Returns [`LoopDecision::Escalate`] on the first spiral hit and
    /// [`LoopDecision::Exhausted`] if the spiral limit is exceeded — either
    /// way the caller must stop retrying shell and surface the latest output.
    pub fn observe_shell_spiral(&mut self) -> LoopDecision {
        self.counters.shell_spiral = self.counters.shell_spiral.saturating_add(1);
        let decision = if self.counters.shell_spiral > self.limits.shell_spiral {
            LoopDecision::Exhausted
        } else {
            LoopDecision::Escalate
        };
        Self::record_metric(SHELL_SPIRAL_VARIANT, decision);
        decision
    }

    /// Resolve the decision at hard-budget exhaustion. Returns
    /// [`LoopDecision::Grace`] at most once for this retry state, and only
    /// when there has been at least one productive tool call before the first
    /// budget hit; otherwise returns [`LoopDecision::Escalate`].
    ///
    /// The single grace call is deliberately global to the loop, not one per
    /// productive tool call. Otherwise an agent that keeps making productive
    /// reads after `max_iterations` can run indefinitely.
    pub fn observe_budget_exhaustion(&mut self) -> LoopDecision {
        let decision =
            if self.grace_calls_fired == 0 && self.productive_tool_calls_since_last_grace >= 1 {
                self.productive_tool_calls_since_last_grace = 0;
                self.grace_calls_fired = self.grace_calls_fired.saturating_add(1);
                LoopDecision::Grace
            } else {
                LoopDecision::Escalate
            };
        Self::record_metric("budget_exhaustion", decision);
        decision
    }

    /// Snapshot of the current counters — exposed for metrics export and
    /// debugging. Mutations must go through `observe*` or
    /// `record_productive_tool_call`.
    pub fn counters(&self) -> LoopRetryCounters {
        self.counters
    }

    /// Merge the delta one turn applied on top of `base` (the state that
    /// turn loaded) into `self`, which a concurrent turn may have advanced
    /// since the load (#1655). Bucket counters and `grace_calls_fired` are
    /// monotonic, so the turn's increments add onto whatever `self` holds
    /// now — a later write-back can no longer roll a bucket back below a
    /// count an earlier turn already reached.
    /// `productive_tool_calls_since_last_grace` is NOT monotonic (a grace
    /// call resets it to zero), so its delta is applied with signed
    /// saturation. `limits` is static configuration, taken from `turn` —
    /// identical to the legacy whole-state write-back in every real flow.
    ///
    /// When `self == base` (no concurrent writer) the result is exactly
    /// `turn`, keeping single-agent behaviour byte-identical.
    pub fn merge_turn_delta(&mut self, base: &Self, turn: &Self) {
        self.counters
            .saturating_add_turn_delta(&base.counters, &turn.counters);
        self.grace_calls_fired = self.grace_calls_fired.saturating_add(
            turn.grace_calls_fired
                .saturating_sub(base.grace_calls_fired),
        );
        let productive_delta = i64::from(turn.productive_tool_calls_since_last_grace)
            - i64::from(base.productive_tool_calls_since_last_grace);
        self.productive_tool_calls_since_last_grace =
            (i64::from(self.productive_tool_calls_since_last_grace) + productive_delta)
                .clamp(0, i64::from(u32::MAX)) as u32;
        self.limits = turn.limits;
    }

    /// Emit a structured `HarnessEventPayload::Retry` event carrying the
    /// variant + decision pair. Returns the constructed event so the caller
    /// can also write it to the local harness event sink without rebuilding it.
    #[allow(clippy::too_many_arguments)]
    pub fn emit_event(
        &self,
        variant: &str,
        decision: LoopDecision,
        session_id: impl Into<String>,
        task_id: impl Into<String>,
        workflow: Option<&str>,
        phase: Option<&str>,
        attempt: Option<u32>,
    ) -> HarnessEvent {
        HarnessEvent {
            schema: HARNESS_EVENT_SCHEMA_V1.to_string(),
            payload: HarnessEventPayload::Retry {
                data: HarnessRetryEvent {
                    session_id: session_id.into(),
                    task_id: task_id.into(),
                    workflow: workflow.map(ToOwned::to_owned),
                    phase: phase.map(ToOwned::to_owned),
                    attempt,
                    message: Some(
                        format!("variant={variant} decision={} ", decision.as_str())
                            .trim_end()
                            .to_string(),
                    ),
                    extra: {
                        let mut extra = std::collections::HashMap::new();
                        extra.insert("variant".to_string(), serde_json::Value::from(variant));
                        extra.insert(
                            "decision".to_string(),
                            serde_json::Value::from(decision.as_str()),
                        );
                        extra
                    },
                },
            },
        }
    }

    fn bump_counter(&mut self, error: &HarnessError) -> (u32, u32) {
        let (counter_ref, limit) = match error {
            HarnessError::RateLimited { .. } => {
                (&mut self.counters.rate_limited, self.limits.rate_limited)
            }
            HarnessError::ContextOverflow { .. } => (
                &mut self.counters.context_overflow,
                self.limits.context_overflow,
            ),
            HarnessError::Authentication { .. } => (
                &mut self.counters.authentication,
                self.limits.authentication,
            ),
            HarnessError::Quota { .. } => (&mut self.counters.quota, self.limits.quota),
            HarnessError::InvalidRequest { .. } => (
                &mut self.counters.invalid_request,
                self.limits.invalid_request,
            ),
            HarnessError::ContentFiltered { .. } => (
                &mut self.counters.content_filtered,
                self.limits.content_filtered,
            ),
            HarnessError::ProviderUnavailable { .. } => (
                &mut self.counters.provider_unavailable,
                self.limits.provider_unavailable,
            ),
            HarnessError::Network { .. } => (&mut self.counters.network, self.limits.network),
            HarnessError::Timeout { .. } => (&mut self.counters.timeout, self.limits.timeout),
            HarnessError::ToolExecution { .. } => (
                &mut self.counters.tool_execution,
                self.limits.tool_execution,
            ),
            HarnessError::Internal { .. } => (&mut self.counters.internal, self.limits.internal),
            HarnessError::PolicyDeny { .. } => (&mut self.counters.policy, self.limits.policy),
        };
        *counter_ref = counter_ref.saturating_add(1);
        (*counter_ref, limit)
    }

    fn record_metric(variant: &str, decision: LoopDecision) {
        counter!(
            OCTOS_LOOP_RETRY_TOTAL,
            "variant" => variant.to_string(),
            "decision" => decision.as_str().to_string(),
        )
        .increment(1);
    }
}

/// Map a `HarnessError` variant to the canonical loop decision, ignoring
/// bucket exhaustion. The caller decides whether to return [`LoopDecision::Exhausted`]
/// based on the counter/limit comparison.
fn decide_for_variant(error: &HarnessError) -> LoopDecision {
    match error.recovery_hint() {
        // Transient — retry without reshaping context.
        RecoveryHint::BackoffRetry => LoopDecision::Continue,
        // Provider outage — swap lanes.
        RecoveryHint::SwitchProvider => LoopDecision::RotateAndRetry,
        // Conversation too large — only compaction unblocks it.
        RecoveryHint::CompactContext => LoopDecision::CompactAndRetry,
        // Non-retryable, surface to operator.
        RecoveryHint::FailFast => LoopDecision::Escalate,
        // Expected policy behaviour (hook deny) — not a fault; surface for
        // audit, never retry (#2249).
        RecoveryHint::Expected => LoopDecision::Escalate,
        // Internal invariant violation — bug, not recoverable.
        RecoveryHint::Bug => LoopDecision::Escalate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #27b — integration: quota exhaustion routes to `RotateAndRetry`
    /// (the loop advances the provider chain to the fallback slot and
    /// retries), while a TRUE 401 escalates (red line — a wrong key fails
    /// on every lane). Pinned against the live 2026-08-26/27 k3 quota burn.
    #[test]
    fn quota_rotates_lane_but_auth_escalates() {
        let quota = HarnessError::Quota {
            message: "402 quota will reset when the current 7-day window ends".into(),
        };
        let mut state = LoopRetryState::default();
        let decision = state.observe(&quota);
        assert!(
            matches!(decision, LoopDecision::RotateAndRetry),
            "quota must rotate to the fallback lane and retry (#27b), got {decision:?}"
        );

        let auth = HarnessError::Authentication {
            message: "401 invalid api key".into(),
        };
        let mut state = LoopRetryState::default();
        let decision = state.observe(&auth);
        assert!(
            matches!(decision, LoopDecision::Escalate),
            "a true 401 must escalate (FailFast red line), got {decision:?}"
        );
    }

    fn context_overflow() -> HarnessError {
        HarnessError::ContextOverflow {
            limit: Some(200_000),
            used: Some(201_000),
            message: "context exceeded".into(),
        }
    }

    #[test]
    fn observe_context_overflow_returns_compact_then_exhausts() {
        let mut state = LoopRetryState::with_limits(LoopRetryLimits {
            context_overflow: 1,
            ..Default::default()
        });
        assert_eq!(
            state.observe(&context_overflow()),
            LoopDecision::CompactAndRetry
        );
        assert_eq!(state.observe(&context_overflow()), LoopDecision::Exhausted);
    }

    #[test]
    fn grace_call_fires_with_productive_history() {
        let mut state = LoopRetryState::new();
        state.record_productive_tool_call();
        assert_eq!(state.observe_budget_exhaustion(), LoopDecision::Grace);
        assert_eq!(state.grace_calls_fired, 1);
        assert_eq!(state.productive_tool_calls_since_last_grace, 0);
    }

    #[test]
    fn shell_spiral_escalates_on_first_hit_then_exhausts() {
        let mut state = LoopRetryState::with_limits(LoopRetryLimits {
            shell_spiral: 1,
            ..Default::default()
        });
        assert_eq!(state.observe_shell_spiral(), LoopDecision::Escalate);
        assert_eq!(state.observe_shell_spiral(), LoopDecision::Exhausted);
    }

    #[test]
    fn legacy_retry_state_json_without_quota_field_deserializes() {
        // Codex round-6: `LoopRetryLimits` and `LoopRetryCounters` gained a
        // `quota` field in round-3. Without `#[serde(default)]`, retry-state
        // sidecar JSON written by pre-round-3 builds would fail to
        // deserialize, and `load_retry_state` would silently reset every
        // counter to zero. This test pins the backward-compat behavior.
        let legacy_json = r#"{
            "counters": {
                "rate_limited": 3,
                "context_overflow": 1,
                "authentication": 0,
                "invalid_request": 0,
                "content_filtered": 0,
                "provider_unavailable": 2,
                "network": 1,
                "timeout": 0,
                "tool_execution": 0,
                "plugin_spawn": 0,
                "plugin_timeout": 0,
                "plugin_protocol": 0,
                "delegate_depth_exceeded": 0,
                "internal": 0,
                "shell_spiral": 0
            },
            "limits": {
                "rate_limited": 5,
                "context_overflow": 2,
                "authentication": 1,
                "invalid_request": 2,
                "content_filtered": 1,
                "provider_unavailable": 4,
                "network": 4,
                "timeout": 3,
                "tool_execution": 5,
                "plugin_spawn": 2,
                "plugin_timeout": 3,
                "plugin_protocol": 2,
                "delegate_depth_exceeded": 1,
                "internal": 1,
                "shell_spiral": 1
            },
            "productive_tool_calls_since_last_grace": 4,
            "grace_calls_fired": 0
        }"#;

        let state: LoopRetryState =
            serde_json::from_str(legacy_json).expect("legacy JSON should deserialize");

        // Existing counters preserved.
        assert_eq!(state.counters.rate_limited, 3);
        assert_eq!(state.counters.provider_unavailable, 2);
        // Missing quota counter defaults to 0 (a clean Default::default()).
        assert_eq!(state.counters.quota, 0);
        // Missing quota limit defaults to the canonical DEFAULT_QUOTA_LIMIT
        // (1) — not 0, which would disable the bucket entirely.
        assert_eq!(state.limits.quota, DEFAULT_QUOTA_LIMIT);
        // Productive history preserved so grace-call gating survives.
        assert_eq!(state.productive_tool_calls_since_last_grace, 4);
    }

    #[test]
    fn decisions_have_stable_snake_case_labels() {
        // These strings appear as Prometheus labels and in structured events;
        // changing them is a breaking change for dashboards and integrations.
        assert_eq!(LoopDecision::Continue.as_str(), "continue");
        assert_eq!(LoopDecision::RotateAndRetry.as_str(), "rotate_and_retry");
        assert_eq!(LoopDecision::CompactAndRetry.as_str(), "compact_and_retry");
        assert_eq!(LoopDecision::Escalate.as_str(), "escalate");
        assert_eq!(LoopDecision::Exhausted.as_str(), "exhausted");
        assert_eq!(LoopDecision::Grace.as_str(), "grace");
    }
}
