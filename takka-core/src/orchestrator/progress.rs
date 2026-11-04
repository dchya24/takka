//! Live tool-output throttling.
//!
//! [`DeltaThrottler`] caps how many `ToolDelta` events an orchestrator
//! surfaces for one tool: at most one delta per ~80ms AND a total char
//! budget per tool. A noisy stream (`tail -f`, `cargo build -v`) can't
//! drown the event channel or the terminal, while the full result still
//! reaches the final `ToolOutput` (and memory) unchanged.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Minimum gap between two surfaced deltas for the same tool.
const DELTA_INTERVAL: Duration = Duration::from_millis(80);

/// Throttled, budget-limited gate for per-tool live output.
pub struct DeltaThrottler {
    state: Mutex<ThrottleState>,
}

struct ThrottleState {
    last_emit: Instant,
    budget_remaining: usize,
}

impl DeltaThrottler {
    /// Create a throttler with a total live-output budget (chars).
    /// Once the budget is exhausted, `accept` returns `false` for the
    /// rest of the tool's run.
    pub fn new(budget: usize) -> Self {
        Self {
            state: Mutex::new(ThrottleState {
                last_emit: Instant::now() - DELTA_INTERVAL,
                budget_remaining: budget,
            }),
        }
    }

    /// Call for each incoming delta piece. Returns `true` when it should
    /// be surfaced to the event emitter as a `ToolDelta`.
    pub fn accept(&self, delta: &str) -> bool {
        let mut s = self.state.lock().unwrap();

        // Budget exhausted — the final chunk may still be surfaced so the
        // tail of the output isn't cut off mid-word.
        if s.budget_remaining == 0 {
            return false;
        }

        // Respect the min interval; coalesce bursts into the next window.
        if s.last_emit.elapsed() < DELTA_INTERVAL {
            return false;
        }

        s.budget_remaining = s.budget_remaining.saturating_sub(delta.len());
        s.last_emit = Instant::now();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_first_delta_immediately() {
        let t = DeltaThrottler::new(8_000);
        assert!(t.accept("hello"));
    }

    #[test]
    fn respects_min_interval() {
        let t = DeltaThrottler::new(8_000);
        assert!(t.accept("a"));
        // Immediately again → coalesced (interval not elapsed).
        assert!(!t.accept("b"));
    }

    #[test]
    fn exhausts_budget() {
        let t = DeltaThrottler::new(10);
        assert!(t.accept("12345")); // budget 10→5
        std::thread::sleep(Duration::from_millis(90));
        assert!(t.accept("12345")); // budget 5→0
        std::thread::sleep(Duration::from_millis(90));
        assert!(!t.accept("x")); // budget 0 → reject
    }
}
