//! Monotonic, deadline-based scheduling for the execution worker.
//!
//! The scheduler keeps a *cursor*: the instant at which the previous input event was
//! supposed to happen. Each wait advances the cursor by its configured duration, so small
//! wake-up overshoots (tens of microseconds) do not accumulate into drift.
//!
//! When an event is emitted noticeably late (more than `absorb`, e.g. because the machine
//! stalled), the cursor is re-anchored to the real emission time instead of trying to catch
//! up. That means overdue actions are never burst out back-to-back, and a key hold is never
//! shortened by more than `absorb` relative to the configured duration.

use std::time::{Duration, Instant};

/// Lateness below this is absorbed into the schedule; above it the cursor re-anchors.
pub const DEFAULT_ABSORB: Duration = Duration::from_millis(1);

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TimingStats {
    /// Input events emitted.
    pub events: u64,
    /// Largest observed lateness of an emitted event versus its deadline.
    pub max_late: Duration,
    /// Number of times the schedule had to be re-anchored (lateness > absorb).
    pub reanchors: u64,
}

#[derive(Debug)]
pub struct Scheduler {
    cursor: Instant,
    absorb: Duration,
    stats: TimingStats,
}

impl Scheduler {
    pub fn new(start: Instant) -> Self {
        Self::with_absorb(start, DEFAULT_ABSORB)
    }

    pub fn with_absorb(start: Instant, absorb: Duration) -> Self {
        Scheduler { cursor: start, absorb, stats: TimingStats::default() }
    }

    /// Restart the schedule from `now` (after a pause or a start delay).
    pub fn reset(&mut self, now: Instant) {
        self.cursor = now;
    }

    /// Advance the cursor by `d` and return the new deadline.
    pub fn advance(&mut self, d: Duration) -> Instant {
        self.cursor += d;
        self.cursor
    }

    pub fn cursor(&self) -> Instant {
        self.cursor
    }

    /// Record that an event scheduled at the cursor was emitted at `at`.
    pub fn mark_emitted(&mut self, at: Instant) {
        self.stats.events += 1;
        let late = at.saturating_duration_since(self.cursor);
        if late > self.stats.max_late {
            self.stats.max_late = late;
        }
        if late > self.absorb {
            self.cursor = at;
            self.stats.reanchors += 1;
        }
    }

    pub fn stats(&self) -> TimingStats {
        self.stats
    }
}

/// Reduce the kernel timer slack for the calling thread so that timed waits wake up within
/// a few microseconds of their deadline instead of the default 50 µs. Best effort.
pub fn tighten_timer_slack() {
    // SAFETY: PR_SET_TIMERSLACK takes a plain integer argument and only affects this thread.
    let rc = unsafe { libc::prctl(libc::PR_SET_TIMERSLACK, 1 as libc::c_ulong, 0, 0, 0) };
    if rc != 0 {
        tracing::debug!("PR_SET_TIMERSLACK failed: {}", std::io::Error::last_os_error());
    }
}

/// Convert user-facing milliseconds to a `Duration`.
pub fn ms(v: u32) -> Duration {
    Duration::from_millis(u64::from(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_overshoot_is_absorbed_without_drift() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(t0);
        for i in 1..=100u64 {
            let deadline = s.advance(Duration::from_millis(10));
            // wake 100 µs late every time
            s.mark_emitted(deadline + Duration::from_micros(100));
            assert_eq!(s.cursor(), t0 + Duration::from_millis(10 * i));
        }
        assert_eq!(s.stats().reanchors, 0);
        assert_eq!(s.stats().max_late, Duration::from_micros(100));
    }

    #[test]
    fn large_stall_reanchors_instead_of_bursting() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(t0);
        let d1 = s.advance(Duration::from_millis(10));
        // stalled for 200 ms
        let actual = d1 + Duration::from_millis(200);
        s.mark_emitted(actual);
        assert_eq!(s.cursor(), actual);
        // next deadline is a full interval after the *actual* emission, not overdue
        let d2 = s.advance(Duration::from_millis(10));
        assert_eq!(d2, actual + Duration::from_millis(10));
        assert_eq!(s.stats().reanchors, 1);
    }

    #[test]
    fn early_emission_does_not_move_cursor() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(t0);
        let d = s.advance(Duration::from_millis(5));
        s.mark_emitted(t0);
        assert_eq!(s.cursor(), d);
        assert_eq!(s.stats().max_late, Duration::ZERO);
    }
}
