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

/// The production clock: process-monotonic milliseconds for [`Throttle::tick`].
/// Lives beside the pure decision so every adapter's transport loop paces from
/// the same tick source; tests keep supplying plain integers.
pub fn monotonic_ms() -> i64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_millis() as i64
}

#[cfg(test)]
#[path = "../../tests/llm/throttle.rs"]
mod tests;
