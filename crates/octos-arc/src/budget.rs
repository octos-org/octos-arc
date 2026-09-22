//! Wall-clock budgets (`Flow.remaining/time_up`, `node_cycle`'s node budget).

use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct Global {
    started: Instant,
    budget: Duration,
}

impl Global {
    pub fn new(seconds: u64) -> Self {
        Self {
            started: Instant::now(),
            budget: Duration::from_secs(seconds),
        }
    }

    pub fn budget_seconds(&self) -> u64 {
        self.budget.as_secs()
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Seconds left; negative once the budget is exhausted.
    pub fn remaining(&self) -> f64 {
        self.budget.as_secs_f64() - self.started.elapsed().as_secs_f64()
    }

    pub fn time_up(&self) -> bool {
        self.remaining() <= 0.0
    }
}

/// Node budget = min(cap, max(floor, remaining / nodes_left)).
pub fn node_budget_seconds(cap: u64, floor: u64, remaining: f64, nodes_left: usize) -> f64 {
    let share = remaining / nodes_left.max(1) as f64;
    (cap as f64).min((floor as f64).max(share))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_share_remaining_time_between_nodes_within_bounds() {
        assert_eq!(node_budget_seconds(1500, 240, 6000.0, 2), 1500.0);
        assert_eq!(node_budget_seconds(1500, 240, 1000.0, 4), 250.0);
        assert_eq!(node_budget_seconds(1500, 240, 100.0, 4), 240.0);
        assert_eq!(node_budget_seconds(1500, 240, -50.0, 0), 240.0);
        let global = Global::new(10);
        assert!(!global.time_up());
        assert!(global.remaining() <= 10.0);
    }
}
