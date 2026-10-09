//! De-duplicates tick sources that fire many times per frame (ProcessEvent, sub-stepped
//! physics) down to one update per presented frame.

use std::time::{Duration, Instant};

const PRESENT_TIMEOUT: Duration = Duration::from_millis(250);
const FALLBACK_PERIOD: Duration = Duration::from_millis(16);

#[derive(Debug)]
pub struct FrameGate {
    use_present: bool,
    last_counter: u64,
    last_change: Option<Instant>,
    last_tick: Option<Instant>,
}

impl FrameGate {
    /// `use_present`: follow the overlay's Present counter (frame_source = "present_counter");
    /// otherwise, or while no Present is observed, tick every 16 ms.
    pub fn new(use_present: bool) -> Self {
        Self { use_present, last_counter: 0, last_change: None, last_tick: None }
    }

    pub fn should_tick(&mut self, counter: u64, now: Instant) -> bool {
        if self.use_present {
            if counter != self.last_counter {
                self.last_counter = counter;
                self.last_change = Some(now);
                self.last_tick = Some(now);
                return true;
            }
            if self.last_change.is_some_and(|t| now.duration_since(t) < PRESENT_TIMEOUT) {
                return false;
            }
        }
        if self.last_tick.is_none_or(|t| now.duration_since(t) >= FALLBACK_PERIOD) {
            self.last_tick = Some(now);
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_present_counter() {
        let t0 = Instant::now();
        let mut g = FrameGate::new(true);
        assert!(g.should_tick(1, t0));
        assert!(!g.should_tick(1, t0 + Duration::from_millis(5)));
        assert!(!g.should_tick(1, t0 + Duration::from_millis(100)), "present alive: wait for next frame");
        assert!(g.should_tick(2, t0 + Duration::from_millis(101)));
    }

    #[test]
    fn falls_back_to_timer_without_present() {
        let t0 = Instant::now();
        let mut g = FrameGate::new(true);
        assert!(g.should_tick(0, t0), "first call ticks");
        assert!(!g.should_tick(0, t0 + Duration::from_millis(10)));
        assert!(g.should_tick(0, t0 + Duration::from_millis(17)));
        // Present seen, then stalls (overlay gone): timer resumes after the timeout.
        assert!(g.should_tick(5, t0 + Duration::from_millis(20)));
        assert!(!g.should_tick(5, t0 + Duration::from_millis(200)));
        assert!(g.should_tick(5, t0 + Duration::from_millis(300)));
    }

    #[test]
    fn timer_mode_ignores_counter() {
        let t0 = Instant::now();
        let mut g = FrameGate::new(false);
        assert!(g.should_tick(1, t0));
        assert!(!g.should_tick(2, t0 + Duration::from_millis(1)));
        assert!(g.should_tick(3, t0 + Duration::from_millis(16)));
    }
}
