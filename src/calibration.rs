//! Hold-duration calibration: a guided bisection that finds the shortest hold a game
//! reliably recognizes.
//!
//! ARPAuto cannot observe the game, so the *user* judges every test run ("recognized" or
//! "not recognized"). The calibrator only chooses the next duration to try:
//!
//! * no recognized duration yet: double the hold until something is recognized;
//! * otherwise bisect between the longest unrecognized and the shortest recognized hold
//!   until they are at most [`RESOLUTION_MS`] apart.
//!
//! The suggestion adds a safety margin to the shortest recognized value.

pub const RESOLUTION_MS: u32 = 5;
pub const MAX_TEST_MS: u32 = 5_000;
pub const MIN_TEST_MS: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Test this hold duration next.
    Next(u32),
    /// Converged.
    Done { shortest: u32, suggested: u32 },
    /// Not recognized even at the maximum test duration.
    NeverRecognized,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Calibrator {
    /// Longest hold reported as *not* recognized (0 = none yet).
    lo: u32,
    /// Shortest hold reported as recognized.
    hi: Option<u32>,
    current: u32,
    history: Vec<(u32, bool)>,
    finished: Option<Outcome>,
}

fn round_to_resolution(v: u32) -> u32 {
    (v.div_ceil(RESOLUTION_MS) * RESOLUTION_MS).max(MIN_TEST_MS)
}

/// Shortest recognized hold plus a margin of 10 % (at least 5 ms), rounded up to 5 ms.
pub fn suggest(shortest: u32) -> u32 {
    round_to_resolution(shortest + (shortest / 10).max(5)).min(MAX_TEST_MS)
}

impl Calibrator {
    pub fn new(start_ms: u32) -> Self {
        Calibrator { lo: 0, hi: None, current: start_ms.clamp(MIN_TEST_MS, MAX_TEST_MS), history: Vec::new(), finished: None }
    }

    /// Hold duration to test now.
    pub fn current(&self) -> u32 {
        self.current
    }

    pub fn history(&self) -> &[(u32, bool)] {
        &self.history
    }

    pub fn finished(&self) -> Option<Outcome> {
        self.finished
    }

    /// Record the user's verdict for the current duration and get the next step.
    pub fn record(&mut self, recognized: bool) -> Outcome {
        if let Some(done) = self.finished {
            return done;
        }
        let tested = self.current;
        self.history.push((tested, recognized));
        if recognized {
            self.hi = Some(self.hi.map_or(tested, |h| h.min(tested)));
        } else {
            self.lo = self.lo.max(tested);
        }
        let outcome = match self.hi {
            None if tested >= MAX_TEST_MS => Outcome::NeverRecognized,
            None => Outcome::Next((self.lo.max(tested) * 2).clamp(MIN_TEST_MS + 1, MAX_TEST_MS)),
            Some(h) if h <= MIN_TEST_MS || h - self.lo <= RESOLUTION_MS => Outcome::Done { shortest: h, suggested: suggest(h) },
            Some(h) => {
                let mid = round_to_resolution((self.lo + h) / 2);
                let mid = if mid <= self.lo || mid >= h { self.lo + (h - self.lo) / 2 } else { mid };
                Outcome::Next(mid.max(MIN_TEST_MS))
            }
        };
        match outcome {
            Outcome::Next(v) => self.current = v,
            other => self.finished = Some(other),
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Simulate a game that recognizes holds of at least `threshold` ms.
    fn run(start: u32, threshold: u32) -> (Outcome, usize) {
        let mut c = Calibrator::new(start);
        for steps in 1..=40 {
            match c.record(c.current() >= threshold) {
                Outcome::Next(_) => {}
                done => return (done, steps),
            }
        }
        panic!("did not converge");
    }

    #[test]
    fn converges_to_threshold_from_above() {
        for threshold in [1, 7, 33, 73, 120, 199] {
            let (outcome, steps) = run(200, threshold);
            match outcome {
                Outcome::Done { shortest, suggested } => {
                    assert!(shortest >= threshold, "threshold {threshold}: shortest {shortest}");
                    assert!(shortest - threshold <= RESOLUTION_MS, "threshold {threshold}: shortest {shortest}");
                    assert!(suggested > shortest);
                }
                other => panic!("threshold {threshold}: {other:?}"),
            }
            assert!(steps <= 9, "threshold {threshold} took {steps} tests");
        }
    }

    #[test]
    fn grows_when_start_is_too_short() {
        let (outcome, _) = run(20, 300);
        assert!(matches!(outcome, Outcome::Done { shortest, .. } if (300..=305).contains(&shortest)), "{outcome:?}");
    }

    #[test]
    fn reports_never_recognized() {
        let (outcome, _) = run(1000, MAX_TEST_MS + 1);
        assert_eq!(outcome, Outcome::NeverRecognized);
    }

    #[test]
    fn tests_stay_strictly_between_known_bounds() {
        let mut c = Calibrator::new(100);
        assert_eq!(c.record(true), Outcome::Next(50));
        assert_eq!(c.record(false), Outcome::Next(75));
        assert_eq!(c.record(false), Outcome::Next(90));
        assert_eq!(c.record(true), Outcome::Next(85));
        assert_eq!(c.record(true), Outcome::Next(80));
        assert_eq!(c.record(false), Outcome::Done { shortest: 85, suggested: 95 });
        assert_eq!(c.history(), [(100, true), (50, false), (75, false), (90, true), (85, true), (80, false)]);
    }

    #[test]
    fn suggestion_has_margin() {
        assert_eq!(suggest(10), 15);
        assert_eq!(suggest(73), 80);
        assert_eq!(suggest(200), 220);
        assert!(suggest(MAX_TEST_MS) <= MAX_TEST_MS);
    }

    #[test]
    fn finished_calibrator_stays_finished() {
        let mut c = Calibrator::new(5);
        let done = c.record(true);
        assert!(matches!(done, Outcome::Done { shortest: 5, .. }));
        assert_eq!(c.record(false), done);
    }
}
