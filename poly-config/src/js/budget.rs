//! A wall-clock budget for script execution.
//!
//! QuickJS calls the runtime's interrupt handler periodically while it runs.
//! Returning `true` from it aborts the running script. That is how an
//! accidental infinite loop in a configuration or layout is stopped without
//! hanging the compositor (`AGENTS.md` §2.7).
//!
//! The budget is armed before every evaluation and disarmed afterwards, so the
//! same handler can serve the configuration load, each `arrange` call and each
//! layout action.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Nanoseconds value meaning "no deadline"; armed budgets are far below it.
const DISARMED: u64 = u64::MAX;

/// A deadline measured from a fixed origin.
///
/// `origin` never changes, so the deadline can live in an atomic and the
/// interrupt handler only needs shared access. The handler may be required to
/// be `Send` when rquickjs is built with its `parallel` feature, which is why
/// this is shared through an [`Arc`] rather than an `Rc`.
#[derive(Debug)]
pub(crate) struct Budget {
    origin: Instant,
    /// Deadline as nanoseconds since `origin`, or [`DISARMED`].
    deadline: AtomicU64,
}

impl Budget {
    /// A budget that starts disarmed.
    pub fn new() -> Self {
        return Self {
            origin: Instant::now(),
            deadline: AtomicU64::new(DISARMED),
        };
    }

    /// Start a new window of `limit` from now.
    pub fn arm(&self, limit: Duration) {
        let now = self.now_nanos();
        let deadline = now.saturating_add(limit.as_nanos().min(u128::from(u64::MAX - 1)) as u64);
        self.deadline.store(deadline, Ordering::Relaxed);
    }

    /// Stop enforcing a deadline.
    pub fn disarm(&self) {
        self.deadline.store(DISARMED, Ordering::Relaxed);
    }

    /// Whether the armed window has run out.
    pub fn expired(&self) -> bool {
        let deadline = self.deadline.load(Ordering::Relaxed);
        if deadline == DISARMED {
            return false;
        }
        return self.now_nanos() >= deadline;
    }

    fn now_nanos(&self) -> u64 {
        let elapsed = self.origin.elapsed().as_nanos();
        return elapsed.min(u128::from(u64::MAX - 1)) as u64;
    }

    /// The QuickJS interrupt handler for this budget.
    pub fn handler(self: &Arc<Self>) -> impl FnMut() -> bool + 'static {
        let budget = self.clone();
        return move || return budget.expired();
    }
}

impl Default for Budget {
    fn default() -> Self {
        return Self::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disarmed_budget_never_expires() {
        let budget = Budget::new();
        assert!(!budget.expired());
        budget.arm(Duration::from_secs(60));
        budget.disarm();
        assert!(!budget.expired());
    }

    #[test]
    fn an_armed_budget_expires_after_its_window() {
        let budget = Budget::new();
        budget.arm(Duration::ZERO);
        assert!(budget.expired());
    }

    #[test]
    fn the_handler_reports_expiry() {
        let budget = Arc::new(Budget::new());
        let mut handler = budget.handler();
        assert!(!handler());
        budget.arm(Duration::ZERO);
        assert!(handler());
    }
}
