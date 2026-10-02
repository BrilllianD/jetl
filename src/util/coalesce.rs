//! One-at-a-time background requests: a request made while another is in
//! flight is remembered and started when that one finishes, so bursts (a
//! build writing many files) cost at most two runs instead of a pile-up.

use std::time::{Duration, Instant};

/// A request in flight longer than this is assumed lost (its task panicked
/// or its result was dropped) and no longer holds new requests back.
const LOST_AFTER: Duration = Duration::from_secs(10);

#[derive(Debug, Default)]
pub struct Coalesce {
    inflight: Option<Instant>,
    pending: bool,
    /// Requests made so far, started or held back.
    pub requests: u64,
}

impl Coalesce {
    /// Ask for a run; true means start it now, false that it runs after
    /// the current one.
    pub fn request(&mut self) -> bool {
        self.requests += 1;
        if self.inflight.is_some_and(|t| t.elapsed() < LOST_AFTER) {
            self.pending = true;
            false
        } else {
            self.inflight = Some(Instant::now());
            true
        }
    }

    /// The running request finished; true means start the held-back one
    /// now (it counts as in flight).
    pub fn done(&mut self) -> bool {
        self.inflight = None;
        if std::mem::take(&mut self.pending) {
            self.inflight = Some(Instant::now());
            true
        } else {
            false
        }
    }

    /// The running request made progress (a long run reporting in
    /// batches); it is not lost yet.
    pub fn touch(&mut self) {
        if self.inflight.is_some() {
            self.inflight = Some(Instant::now());
        }
    }

    /// Forget the request in flight (its result will be dropped).
    pub fn reset(&mut self) {
        self.inflight = None;
        self.pending = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_runs_once_more_after_the_current_one() {
        let mut c = Coalesce::default();
        assert!(c.request());
        assert!(!c.request());
        assert!(!c.request());
        assert!(c.done(), "the held-back request starts");
        assert!(!c.done(), "nothing else was asked for");
        assert!(c.request());
        c.reset();
        assert!(c.request(), "reset forgets the request in flight");
        assert_eq!(c.requests, 5);
    }

    #[test]
    fn a_lost_request_does_not_block_forever() {
        let mut c = Coalesce { inflight: Some(Instant::now() - LOST_AFTER), ..Default::default() };
        assert!(c.request());
    }

    #[test]
    fn progress_keeps_a_long_request_alive() {
        let mut c = Coalesce { inflight: Some(Instant::now() - LOST_AFTER), ..Default::default() };
        c.touch();
        assert!(!c.request(), "still running");
        let mut idle = Coalesce::default();
        idle.touch();
        assert!(idle.request(), "touch does not start anything");
    }
}
