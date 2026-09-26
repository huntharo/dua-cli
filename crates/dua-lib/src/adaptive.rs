//! Throughput-based worker admission control.
use std::time::Duration;

/// Configuration for automatically adjusting the number of active filesystem workers.
///
/// The pool starts with one active worker and measures entries delivered per second. It probes
/// successively doubled worker counts, retaining an increase only when its marginal throughput
/// per added worker reaches `efficiency_threshold` times the original single-worker throughput.
/// Once growth stops, periodic probes alternate between fewer and more workers. Workload changes
/// can therefore move the selected count in either direction. Probes temporarily use their target
/// count for one adjustment interval. All worker threads are allocated upfront, up to `max_threads`.
///
/// Rates include consumer backpressure and idle time. Changes in tree shape, cache state, or
/// storage latency can therefore produce noisy decisions. Downshifts wait for in-flight jobs
/// to finish; those jobs can also influence the next measurement. Short walks may finish before
/// the baseline interval, and explicit fixed workers can be faster for a known workload.
#[derive(Clone, Copy, Debug)]
pub struct AdaptiveThreads {
    /// Maximum allocated worker count. Zero is normalized to one.
    pub max_threads: usize,
    /// Initial single-worker measurement interval. Zero is normalized to one millisecond.
    pub baseline_interval: Duration,
    /// Duration of subsequent measurements and probes. Zero becomes one millisecond.
    pub adjustment_interval: Duration,
    /// Required marginal gain per added worker, relative to the initial single-worker rate.
    /// Values outside `0.0..=1.0`, including NaN, use the default of `0.60`.
    pub efficiency_threshold: f64,
}

impl Default for AdaptiveThreads {
    fn default() -> Self {
        Self {
            max_threads: std::thread::available_parallelism().map_or(1, usize::from),
            baseline_interval: Duration::from_secs(10),
            adjustment_interval: Duration::from_secs(10),
            efficiency_threshold: 0.60,
        }
    }
}

impl AdaptiveThreads {
    pub(crate) fn normalized(mut self) -> Self {
        self.max_threads = self.max_threads.max(1);
        self.baseline_interval = self.baseline_interval.max(Duration::from_millis(1));
        self.adjustment_interval = self.adjustment_interval.max(Duration::from_millis(1));
        if !(0.0..=1.0).contains(&self.efficiency_threshold) {
            self.efficiency_threshold = 0.60;
        }
        self
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Baseline,
    Up { previous: usize, rate: f64 },
    Down { previous: usize, rate: f64 },
    Hold { down: bool },
}

pub(crate) struct Controller {
    config: AdaptiveThreads,
    phase: Phase,
    baseline: f64,
    active: usize,
}

impl Controller {
    pub(crate) fn new(config: AdaptiveThreads) -> Self {
        Self {
            config: config.normalized(),
            phase: Phase::Baseline,
            baseline: 0.0,
            active: 1,
        }
    }

    pub(crate) fn interval(&self) -> Duration {
        if matches!(self.phase, Phase::Baseline) {
            self.config.baseline_interval
        } else {
            self.config.adjustment_interval
        }
    }

    fn probe_up(&mut self, rate: f64) {
        let next = self.active.saturating_mul(2).min(self.config.max_threads);
        if next > self.active {
            self.phase = Phase::Up {
                previous: self.active,
                rate,
            };
            self.active = next;
        } else {
            self.phase = Phase::Hold { down: true };
        }
    }

    pub(crate) fn sample(&mut self, entries: usize, elapsed: Duration) -> usize {
        if elapsed.is_zero() {
            return self.active;
        }
        let rate = entries as f64 / elapsed.as_secs_f64();
        let required = self.baseline * self.config.efficiency_threshold;
        match self.phase {
            Phase::Baseline => {
                // An idle streaming pool has no usable baseline; wait for a nonempty window.
                if entries > 0 {
                    self.baseline = rate;
                    self.probe_up(rate);
                }
            }
            Phase::Up {
                previous,
                rate: previous_rate,
            } => {
                let marginal = (rate - previous_rate) / (self.active - previous) as f64;
                if marginal >= required {
                    self.probe_up(rate);
                } else {
                    self.active = previous;
                    self.phase = Phase::Hold { down: true };
                }
            }
            Phase::Down {
                previous,
                rate: previous_rate,
            } => {
                let marginal = (previous_rate - rate) / (previous - self.active) as f64;
                if marginal >= required {
                    self.active = previous;
                }
                self.phase = Phase::Hold { down: false };
            }
            Phase::Hold { down } => {
                if entries == 0 {
                    return self.active;
                }
                if down && self.active > 1 {
                    self.phase = Phase::Down {
                        previous: self.active,
                        rate,
                    };
                    self.active = (self.active / 2).max(1);
                } else {
                    self.probe_up(rate);
                }
            }
        }
        self.active
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn controller(max_threads: usize) -> Controller {
        Controller::new(AdaptiveThreads {
            max_threads,
            ..AdaptiveThreads::default()
        })
    }
    fn sample(c: &mut Controller, rate: usize) -> usize {
        c.sample(rate, Duration::from_secs(1))
    }
    #[test]
    fn doubling_uses_original_baseline_and_marginal_gain_per_worker() {
        let mut c = controller(8);
        assert_eq!(sample(&mut c, 100), 2);
        assert_eq!(sample(&mut c, 160), 4); // equality is accepted
        assert_eq!(sample(&mut c, 280), 8); // (280 - 160) / 2 == 60
        assert_eq!(sample(&mut c, 500), 4); // (500 - 280) / 4 == 55: rollback
    }
    #[test]
    fn downshift_and_recovery_probe_both_directions() {
        let mut c = controller(4);
        assert_eq!(sample(&mut c, 100), 2);
        assert_eq!(sample(&mut c, 180), 4);
        assert_eq!(sample(&mut c, 320), 4);
        assert_eq!(sample(&mut c, 190), 2); // hold -> down probe
        assert_eq!(sample(&mut c, 180), 2); // higher workers offered little benefit
        assert_eq!(sample(&mut c, 180), 4); // retry growth
        assert_eq!(sample(&mut c, 330), 4);
        assert_eq!(sample(&mut c, 330), 2);
        assert_eq!(sample(&mut c, 180), 4); // useful higher count restored
    }
    #[test]
    fn clamps_non_power_of_two_caps_and_handles_empty_baseline() {
        let mut c = controller(3);
        assert_eq!(sample(&mut c, 0), 1);
        assert_eq!(sample(&mut c, 100), 2);
        assert_eq!(sample(&mut c, 160), 3);
        assert_eq!(sample(&mut c, 220), 3);
        let mut c = controller(0);
        assert_eq!(sample(&mut c, 100), 1);
        assert_eq!(sample(&mut c, 100), 1);
    }
    #[test]
    fn threshold_and_measurement_duration_are_configurable() {
        let mut c = Controller::new(AdaptiveThreads {
            efficiency_threshold: 0.8,
            max_threads: 8,
            ..AdaptiveThreads::default()
        });
        assert_eq!(c.sample(200, Duration::from_secs(2)), 2);
        assert_eq!(c.sample(350, Duration::from_secs(2)), 1);
        assert_eq!(c.sample(100, Duration::ZERO), 1);
    }
    #[test]
    fn invalid_configuration_is_bounded() {
        let cfg = AdaptiveThreads {
            max_threads: 0,
            baseline_interval: Duration::ZERO,
            adjustment_interval: Duration::ZERO,
            efficiency_threshold: f64::NAN,
        }
        .normalized();
        assert_eq!(cfg.max_threads, 1);
        assert!((cfg.efficiency_threshold - 0.6).abs() < f64::EPSILON);
        assert!(!cfg.baseline_interval.is_zero());
        assert!(!cfg.adjustment_interval.is_zero());
    }
}
