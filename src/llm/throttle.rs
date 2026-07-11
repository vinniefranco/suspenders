//! Pure pacing decision for streaming delta events.
//!
//! The UI accommodation (ADR-0002) extracted: at most one emit per interval
//! keeps the TUI responsive while a local server streams faster than a
//! terminal can usefully draw (~30fps). This module owns only the decision -
//! the caller supplies the clock (a monotonic tick in production, plain
//! integers in tests) and performs the emit.
//!
//! The first tick always emits; each emit re-arms the interval. A skip does
//! NOT re-arm: the next boundary is still measured from the last emit.

/// Whether an event may emit at a given tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Emit,
    Skip,
}

/// The emit-pacing state. `interval_ms` is the minimum gap between emits;
/// `last_at` is the tick of the last emit (`None` before the first).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Throttle {
    interval_ms: i64,
    last_at: Option<i64>,
}

impl Throttle {
    /// A throttle with the given minimum interval (must be positive).
    pub fn new(interval_ms: i64) -> Self {
        debug_assert!(interval_ms > 0, "interval_ms must be positive");
        Throttle {
            interval_ms,
            last_at: None,
        }
    }

    /// Decides whether an event at tick `now` may emit. Returns `Emit` with the
    /// interval re-armed, or `Skip` leaving the state unchanged.
    pub fn tick(&mut self, now: i64) -> Decision {
        match self.last_at {
            None => {
                self.last_at = Some(now);
                Decision::Emit
            }
            Some(last) => {
                if now - last >= self.interval_ms {
                    self.last_at = Some(now);
                    Decision::Emit
                } else {
                    Decision::Skip
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_tick_always_emits() {
        let mut t = Throttle::new(33);
        assert_eq!(t.tick(1_000), Decision::Emit);
    }

    #[test]
    fn ticks_inside_interval_skip_boundary_emits() {
        let mut t = Throttle::new(33);
        assert_eq!(t.tick(1_000), Decision::Emit);
        assert_eq!(t.tick(1_010), Decision::Skip);
        assert_eq!(t.tick(1_032), Decision::Skip);
        assert_eq!(t.tick(1_033), Decision::Emit);
    }

    #[test]
    fn a_skip_does_not_re_arm_the_interval() {
        let mut t = Throttle::new(33);
        assert_eq!(t.tick(1_000), Decision::Emit);
        assert_eq!(t.tick(1_020), Decision::Skip);
        // Still measured from the last EMIT at 1_000, not the skip at 1_020.
        assert_eq!(t.tick(1_033), Decision::Emit);
    }

    #[test]
    fn each_emit_re_arms_the_interval() {
        let mut t = Throttle::new(33);
        assert_eq!(t.tick(1_000), Decision::Emit);
        assert_eq!(t.tick(1_040), Decision::Emit);
        assert_eq!(t.tick(1_050), Decision::Skip);
    }
}
