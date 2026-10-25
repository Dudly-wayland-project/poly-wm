// Ported from niri (GPL-3.0-or-later): src/input/scroll_tracker.rs

//! Accumulates continuous scroll amounts into discrete ticks.
//!
//! A scroll binding should fire once per wheel *tick*; touchpads and
//! high-resolution wheels report fractions of a tick at a time, so the
//! remainders are carried over. A direction change starts from zero, otherwise
//! a reversal would need to eat the accumulated overshoot first.

/// Turns scroll amounts into ticks of `tick` units.
pub struct ScrollTracker {
    /// How much scrolling makes one tick.
    tick: f64,
    /// The last amount, for direction changes.
    last: f64,
    /// The current remainder.
    acc: f64,
}

impl ScrollTracker {
    /// A tracker for the given tick size (a wheel tick is 120 units).
    pub fn new(tick: i8) -> Self {
        return Self {
            tick: f64::from(tick),
            last: 0.0,
            acc: 0.0,
        };
    }

    /// Add an amount, returning the whole ticks it completed.
    pub fn accumulate(&mut self, amount: f64) -> i8 {
        let changed_direction =
            (self.last > 0.0 && amount < 0.0) || (self.last < 0.0 && amount > 0.0);
        if changed_direction {
            self.acc = 0.0;
        }

        self.last = amount;
        self.acc += amount;

        let mut ticks = 0;
        if self.acc.abs() >= self.tick {
            let clamped = self.acc.clamp(-127.0 * self.tick, 127.0 * self.tick);
            ticks = (clamped as i16 / self.tick as i16) as i8;
            self.acc %= self.tick;
        }

        return ticks;
    }

    /// Forget the remainder: no scroll binding is armed right now.
    pub fn reset(&mut self) {
        self.last = 0.0;
        self.acc = 0.0;
    }
}
