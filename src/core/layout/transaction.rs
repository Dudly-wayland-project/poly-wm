// Ported from niri (GPL-3.0-or-later): src/utils/transaction.rs

//! A configure transaction shared by the layout engine.
//!
//! Sending a size configure and moving the window immediately makes the
//! old-size buffer jump to its new place before the client has drawn the new
//! size. The transaction blocks the commits of all windows in a reflow until
//! the last one has acknowledged its configure, so the new layout appears in
//! one step (niri's design, `docs/plan.md` §9).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::wayland::compositor::{Blocker, BlockerState};

/// A client that does not answer its configure must not freeze the layout.
const TIME_LIMIT: Duration = Duration::from_millis(300);

/// One reflow's commit transaction.
#[derive(Debug, Clone)]
pub struct Transaction {
    inner: Arc<Inner>,
}

/// A cheap handle that reports when a transaction finishes, without keeping it
/// alive.
#[derive(Debug, Clone)]
pub struct TransactionWatcher {
    inner: Weak<Inner>,
}

#[derive(Debug)]
struct Inner {
    completed: AtomicBool,
}

/// A commit blocker for one surface.
#[derive(Debug, Clone)]
pub struct TransactionBlocker {
    inner: Weak<Inner>,
}

impl Transaction {
    /// Create a transaction; the creator's clone counts as one participant.
    pub fn new() -> Self {
        return Self {
            inner: Arc::new(Inner {
                completed: AtomicBool::new(false),
            }),
        };
    }

    /// A blocker to attach to a surface that committed too early.
    pub fn blocker(&self) -> TransactionBlocker {
        return TransactionBlocker {
            inner: Arc::downgrade(&self.inner),
        };
    }

    /// A watcher that does not extend the transaction's lifetime.
    pub fn watcher(&self) -> TransactionWatcher {
        return TransactionWatcher {
            inner: Arc::downgrade(&self.inner),
        };
    }

    /// Whether this is the last participant.
    pub fn is_last(&self) -> bool {
        return Arc::strong_count(&self.inner) == 1;
    }

    /// Whether the transaction already completed (all acks or the deadline).
    pub fn is_completed(&self) -> bool {
        return self.inner.completed.load(Ordering::Relaxed);
    }

    /// Complete the transaction after `TIME_LIMIT`, even if a client is stuck.
    pub fn register_deadline_timer<T: 'static>(&self, handle: &LoopHandle<'static, T>) {
        let inner = Arc::downgrade(&self.inner);
        if let Err(err) = handle.insert_source(Timer::from_duration(TIME_LIMIT), move |_, _, _| {
            if let Some(inner) = inner.upgrade() {
                inner.completed.store(true, Ordering::Relaxed);
            }
            return TimeoutAction::Drop;
        }) {
            tracing::warn!("error arming the layout transaction deadline: {err:?}");
        }
    }
}

impl Default for Transaction {
    fn default() -> Self {
        return Self::new();
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        if self.is_last() {
            self.inner.completed.store(true, Ordering::Relaxed);
        }
    }
}

impl TransactionWatcher {
    /// Whether the watched transaction completed. A dropped transaction counts
    /// as completed, which is the common case: no one holds it anymore.
    pub fn is_completed(&self) -> bool {
        return self
            .inner
            .upgrade()
            .is_none_or(|inner| return inner.completed.load(Ordering::Relaxed));
    }
}

impl Blocker for TransactionBlocker {
    fn state(&self) -> BlockerState {
        if self
            .inner
            .upgrade()
            .is_none_or(|inner| return inner.completed.load(Ordering::Relaxed))
        {
            return BlockerState::Released;
        }
        return BlockerState::Pending;
    }
}
