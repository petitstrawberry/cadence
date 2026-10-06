//! Nonblocking hardware-tail hold for an output without a hardware drain ACK.
use std::{
    cell::Cell,
    time::{Duration, Instant},
};

pub struct TailHold {
    duration: Duration,
    empty_since: Cell<Option<Instant>>,
}

impl TailHold {
    pub fn new(duration: Duration) -> Self {
        Self {
            duration,
            empty_since: Cell::new(None),
        }
    }

    pub fn finished_at(&self, now: Instant, empty: bool) -> bool {
        if !empty {
            self.empty_since.set(None);
            return false;
        }
        let since = match self.empty_since.get() {
            Some(since) => since,
            None => {
                self.empty_since.set(Some(now));
                now
            }
        };
        now.saturating_duration_since(since) >= self.duration
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardware_hold_starts_when_the_client_ring_first_becomes_empty() {
        let hold = TailHold::new(Duration::from_millis(240));
        let start = Instant::now();
        assert!(!hold.finished_at(start, false));
        assert!(!hold.finished_at(start + Duration::from_secs(2), true));
        assert!(!hold.finished_at(start + Duration::from_millis(2239), true));
        assert!(hold.finished_at(start + Duration::from_millis(2240), true));
    }

    #[test]
    fn more_pending_audio_restarts_the_hardware_hold() {
        let hold = TailHold::new(Duration::from_millis(240));
        let start = Instant::now();
        assert!(!hold.finished_at(start, true));
        assert!(!hold.finished_at(start + Duration::from_millis(200), false));
        assert!(!hold.finished_at(start + Duration::from_millis(300), true));
        assert!(!hold.finished_at(start + Duration::from_millis(539), true));
        assert!(hold.finished_at(start + Duration::from_millis(540), true));
    }
}
