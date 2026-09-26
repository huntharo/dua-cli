//! Throughput-based worker admission control.
use std::time::Duration;

/// Configuration for reducing filesystem concurrency from the requested thread count.
///
/// The walk constructor's `threads` argument supplies the initial count, capped by `max_threads`.
/// At an accepted count `n` with aggregate throughput `T_n` (entries/second), probe `n - 1`.
/// Keep the reduction only when `T_n - T_(n-1) < loss_threshold * T_n / n`, then use the
/// accepted candidate's throughput as the next reference. Equality rejects the reduction.
/// The first rejection restores `n` and holds it for the rest of the traversal. Never go below
/// one. A restarted [`crate::Walk`] begins a new search; a streaming pool shares one search
/// across all roots, including gaps between submissions.
///
/// Each candidate window starts only after retired workers acknowledge a job boundary.
/// Their in-flight work is excluded. Zero-entry baseline windows wait for work; zero-entry
/// candidate windows conservatively restore the accepted count and stop searching.
/// Rates still include consumer backpressure, idle time, and changes in directory shape,
/// cache state, or storage latency. Short/noisy walks may not yield a useful choice, and this
/// greedy search does not promise an optimal count. All initial workers are allocated upfront.
#[derive(Clone, Copy, Debug)]
pub struct AdaptiveThreads {
    /// Cap on the constructor's initial thread count. Defaults to no additional cap.
    /// Zero is normalized to one. No workers above the initial count are allocated.
    pub max_threads: usize,
    /// Initial measurement duration. Defaults to 250 ms; zero becomes one millisecond.
    pub baseline_interval: Duration,
    /// Candidate measurement duration, after retirement. Defaults to 250 ms; zero becomes 1 ms.
    pub adjustment_interval: Duration,
    /// Allowed fraction of projected loss `T_n / n`. Defaults to `0.20` (20%).
    /// Values outside `0.0..=1.0`, including NaN, use the default. Acceptance is strictly less.
    pub loss_threshold: f64,
}

impl Default for AdaptiveThreads {
    fn default() -> Self {
        Self {
            max_threads: usize::MAX,
            baseline_interval: Duration::from_millis(250),
            adjustment_interval: Duration::from_millis(250),
            loss_threshold: 0.20,
        }
    }
}

impl AdaptiveThreads {
    pub(crate) fn normalized(mut self) -> Self {
        self.max_threads = self.max_threads.max(1);
        self.baseline_interval = self.baseline_interval.max(Duration::from_millis(1));
        self.adjustment_interval = self.adjustment_interval.max(Duration::from_millis(1));
        if !(0.0..=1.0).contains(&self.loss_threshold) {
            self.loss_threshold = Self::default().loss_threshold;
        }
        self
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Baseline,
    Probe { previous_rate: f64 },
    Hold,
}

pub(crate) struct Controller {
    config: AdaptiveThreads,
    phase: Phase,
    active: usize,
}

impl Controller {
    pub(crate) fn new(config: AdaptiveThreads, initial: usize) -> Self {
        let config = config.normalized();
        let active = initial.max(1).min(config.max_threads);
        Self {
            config,
            phase: if active == 1 {
                Phase::Hold
            } else {
                Phase::Baseline
            },
            active,
        }
    }

    pub(crate) fn holding(&self) -> bool {
        matches!(self.phase, Phase::Hold)
    }

    pub(crate) fn interval(&self) -> Duration {
        if matches!(self.phase, Phase::Baseline) {
            self.config.baseline_interval
        } else {
            self.config.adjustment_interval
        }
    }

    fn probe(&mut self, rate: f64) {
        if self.active > 1 {
            self.phase = Phase::Probe {
                previous_rate: rate,
            };
            self.active -= 1;
        } else {
            self.phase = Phase::Hold;
        }
    }

    pub(crate) fn sample(&mut self, entries: usize, elapsed: Duration) -> usize {
        if elapsed.is_zero() {
            return self.active;
        }
        let rate = entries as f64 / elapsed.as_secs_f64();
        match self.phase {
            Phase::Baseline if entries > 0 => self.probe(rate),
            Phase::Probe { previous_rate } => {
                let previous = self.active + 1;
                let loss = previous_rate - rate;
                let allowed = self.config.loss_threshold * (previous_rate / previous as f64);
                if entries > 0 && loss < allowed {
                    self.probe(rate);
                } else {
                    self.active = previous;
                    self.phase = Phase::Hold;
                }
            }
            Phase::Baseline | Phase::Hold => {}
        }
        self.active
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn controller(initial: usize) -> Controller {
        Controller::new(AdaptiveThreads::default(), initial)
    }
    fn sample(c: &mut Controller, rate: usize) -> usize {
        c.sample(rate, Duration::from_secs(1))
    }
    #[test]
    fn projected_loss_uses_accepted_count_and_strict_boundary() {
        for (candidate, expected) in [(1581, 14), (1580, 16), (1579, 16)] {
            let mut c = controller(16);
            assert_eq!(sample(&mut c, 1600), 15);
            // Allowed loss: .20 * 1600 / 16 = 20.
            assert_eq!(sample(&mut c, candidate), expected);
        }
    }
    #[test]
    fn descent_is_one_at_a_time_and_refreshes_reference() {
        let mut c = controller(4);
        assert_eq!(sample(&mut c, 1000), 3);
        assert_eq!(sample(&mut c, 960), 2); // loss 40 < 50
        assert_eq!(sample(&mut c, 895), 3); // loss 65 >= .20 * 960 / 3 = 64
        assert!(c.holding());
        for rate in [0, 100, 2000] {
            assert_eq!(sample(&mut c, rate), 3);
        }
    }
    #[test]
    fn improvements_and_plateaus_can_descend_to_one() {
        let mut c = controller(4);
        assert_eq!(sample(&mut c, 100), 3);
        assert_eq!(sample(&mut c, 120), 2);
        assert_eq!(sample(&mut c, 120), 1);
        assert!(!c.holding()); // one is still a candidate
        assert_eq!(sample(&mut c, 120), 1);
        assert!(c.holding());
        assert_eq!(sample(&mut c, 1), 1);
    }
    #[test]
    fn first_failure_restores_initial_count_permanently() {
        let mut c = controller(16);
        assert_eq!(sample(&mut c, 1600), 15);
        assert_eq!(sample(&mut c, 1500), 16);
        assert!(c.holding());
        assert_eq!(sample(&mut c, 9999), 16);
    }
    #[test]
    fn empty_or_invalid_samples_do_not_infer_efficiency() {
        let mut c = controller(3);
        assert_eq!(sample(&mut c, 0), 3);
        assert_eq!(c.sample(100, Duration::ZERO), 3);
        assert_eq!(sample(&mut c, 100), 2);
        assert_eq!(c.sample(100, Duration::ZERO), 2);
        assert_eq!(sample(&mut c, 0), 3);
        assert!(c.holding());
        for initial in [0, 1] {
            let mut c = controller(initial);
            assert!(c.holding());
            assert_eq!(sample(&mut c, 100), 1);
        }
    }
    #[test]
    fn count_threshold_and_durations_are_configurable() {
        let mut c = Controller::new(
            AdaptiveThreads {
                max_threads: 3,
                baseline_interval: Duration::from_millis(50),
                adjustment_interval: Duration::from_millis(100),
                loss_threshold: 0.5,
            },
            16,
        );
        assert_eq!(c.interval(), Duration::from_millis(50));
        assert_eq!(c.sample(300, Duration::from_secs(2)), 2);
        assert_eq!(c.interval(), Duration::from_millis(100));
        assert_eq!(c.sample(126, Duration::from_secs(1)), 1); // 24 < 25
        assert_eq!(c.sample(180, Duration::from_secs(2)), 2); // 36 >= 31.5
    }
    #[test]
    fn zero_threshold_requires_an_improvement() {
        let mut c = Controller::new(
            AdaptiveThreads {
                loss_threshold: 0.0,
                ..AdaptiveThreads::default()
            },
            3,
        );
        assert_eq!(sample(&mut c, 100), 2);
        assert_eq!(sample(&mut c, 101), 1);
        assert_eq!(sample(&mut c, 101), 2);
    }
    #[test]
    fn invalid_configuration_is_bounded() {
        for threshold in [f64::NAN, f64::INFINITY, -0.1, 1.1] {
            let cfg = AdaptiveThreads {
                max_threads: 0,
                baseline_interval: Duration::ZERO,
                adjustment_interval: Duration::ZERO,
                loss_threshold: threshold,
            }
            .normalized();
            assert_eq!(cfg.max_threads, 1);
            assert!((cfg.loss_threshold - 0.2).abs() < f64::EPSILON);
            assert_eq!(cfg.baseline_interval, Duration::from_millis(1));
            assert_eq!(cfg.adjustment_interval, Duration::from_millis(1));
        }
    }
}
