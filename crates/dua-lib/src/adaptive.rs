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

/// Opt-in coarse search for fewer workers retaining recent useful throughput.
///
/// Probe half the accepted count, then bisect the first failing bracket. Each candidate
/// needs two passing comparisons out of at most three. Every comparison measures the
/// initial count again immediately before the candidate, so losses do not compound as
/// counts fall and one noisy rejection does not end the search. At most logarithmically
/// many candidate counts are tested; the final count is held until the walk restarts.
///
/// Measurements begin after retirement acknowledgements. Empty, completed, or substantially
/// consumer-backpressured windows cannot accept a reduction. Three inconclusive windows
/// abandon the search at the last accepted count. Initial empty windows wait for work.
/// The target describes observed entries/second, not disk utilization. Changing workloads
/// can still bias comparisons; this bounded search does not guarantee a global optimum.
#[derive(Clone, Copy, Debug)]
pub struct ThroughputThreads {
    /// Cap on initial workers. Zero becomes one; workers never exceed the initial count.
    pub max_threads: usize,
    /// Duration of each fresh initial-count reference measurement. Defaults to 250 ms.
    pub baseline_interval: Duration,
    /// Duration of each settled candidate measurement. Defaults to 250 ms.
    pub adjustment_interval: Duration,
    /// Minimum candidate/reference throughput ratio, inclusive. Defaults to `0.80`.
    /// Invalid values outside `0.0..=1.0` use the default. Zero still requires useful work.
    pub retained_throughput: f64,
}

impl Default for ThroughputThreads {
    fn default() -> Self {
        Self {
            max_threads: usize::MAX,
            baseline_interval: Duration::from_millis(250),
            adjustment_interval: Duration::from_millis(250),
            retained_throughput: 0.80,
        }
    }
}

impl ThroughputThreads {
    fn normalized(mut self) -> Self {
        self.max_threads = self.max_threads.max(1);
        self.baseline_interval = self.baseline_interval.max(Duration::from_millis(1));
        self.adjustment_interval = self.adjustment_interval.max(Duration::from_millis(1));
        if !(0.0..=1.0).contains(&self.retained_throughput) {
            self.retained_throughput = Self::default().retained_throughput;
        }
        self
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Policy {
    Marginal(AdaptiveThreads),
    Throughput(ThroughputThreads),
}

impl From<AdaptiveThreads> for Policy {
    fn from(config: AdaptiveThreads) -> Self {
        Self::Marginal(config)
    }
}

impl Policy {
    pub(crate) fn for_options(options: crate::Options) -> Option<Self> {
        options
            .throughput_threads
            .map(Self::Throughput)
            .or_else(|| options.adaptive_threads.map(Self::Marginal))
    }

    pub(crate) fn max_threads(self) -> usize {
        match self {
            Self::Marginal(config) => config.normalized().max_threads,
            Self::Throughput(config) => config.normalized().max_threads,
        }
    }

    pub(crate) fn controller(self, initial: usize) -> Tuner {
        match self {
            Self::Marginal(config) => Tuner::Marginal(Controller::new(config, initial)),
            Self::Throughput(config) => {
                Tuner::Throughput(ThroughputController::new(config, initial))
            }
        }
    }
}

pub(crate) enum Tuner {
    Marginal(Controller),
    Throughput(ThroughputController),
}

impl Tuner {
    pub(crate) fn holding(&self) -> bool {
        match self {
            Self::Marginal(c) => c.holding(),
            Self::Throughput(c) => c.holding(),
        }
    }

    pub(crate) fn interval(&self) -> Duration {
        match self {
            Self::Marginal(c) => c.interval(),
            Self::Throughput(c) => c.interval(),
        }
    }

    pub(crate) fn sample(
        &mut self,
        entries: usize,
        elapsed: Duration,
        backpressured: bool,
    ) -> usize {
        match self {
            Self::Marginal(c) => c.sample(entries, elapsed),
            Self::Throughput(c) => c.sample(entries, elapsed, backpressured),
        }
    }
}

#[derive(Clone, Copy)]
enum ThroughputPhase {
    Reference,
    Candidate { reference_rate: f64 },
    Hold,
}

pub(crate) struct ThroughputController {
    config: ThroughputThreads,
    phase: ThroughputPhase,
    initial: usize,
    active: usize,
    accepted: usize,
    rejected: usize,
    candidate: usize,
    passes: u8,
    failures: u8,
    inconclusive: u8,
}

impl ThroughputController {
    fn new(config: ThroughputThreads, initial: usize) -> Self {
        let config = config.normalized();
        let initial = initial.max(1).min(config.max_threads);
        Self {
            config,
            phase: if initial == 1 {
                ThroughputPhase::Hold
            } else {
                ThroughputPhase::Reference
            },
            initial,
            active: initial,
            accepted: initial,
            rejected: 0,
            candidate: (initial / 2).max(1),
            passes: 0,
            failures: 0,
            inconclusive: 0,
        }
    }

    fn holding(&self) -> bool {
        matches!(self.phase, ThroughputPhase::Hold)
    }

    fn interval(&self) -> Duration {
        if matches!(self.phase, ThroughputPhase::Reference) {
            self.config.baseline_interval
        } else {
            self.config.adjustment_interval
        }
    }

    fn hold(&mut self) {
        self.active = self.accepted;
        self.phase = ThroughputPhase::Hold;
    }

    fn sample(&mut self, entries: usize, elapsed: Duration, backpressured: bool) -> usize {
        if elapsed.is_zero() || self.holding() {
            return self.active;
        }
        if entries == 0 || backpressured {
            // A streaming pool may be created long before the first root arrives.
            if entries == 0
                && self.accepted == self.initial
                && self.passes == 0
                && self.failures == 0
                && self.inconclusive == 0
                && matches!(self.phase, ThroughputPhase::Reference)
                && !backpressured
            {
                return self.active;
            }
            self.inconclusive += 1;
            if self.inconclusive == 3 {
                self.hold();
            } else {
                self.active = self.initial;
                self.phase = ThroughputPhase::Reference;
            }
            return self.active;
        }
        let rate = entries as f64 / elapsed.as_secs_f64();
        match self.phase {
            ThroughputPhase::Reference => {
                self.phase = ThroughputPhase::Candidate {
                    reference_rate: rate,
                };
                self.active = self.candidate;
            }
            ThroughputPhase::Candidate { reference_rate } => {
                if rate >= reference_rate * self.config.retained_throughput {
                    self.passes += 1;
                } else {
                    self.failures += 1;
                }
                if self.passes == 2 || self.failures == 2 {
                    if self.passes == 2 {
                        self.accepted = self.candidate;
                    } else {
                        self.rejected = self.candidate;
                    }
                    if self.accepted - self.rejected <= 1 {
                        self.hold();
                        return self.active;
                    }
                    self.candidate = self.rejected + (self.accepted - self.rejected) / 2;
                    self.passes = 0;
                    self.failures = 0;
                    self.inconclusive = 0;
                }
                self.active = self.initial;
                self.phase = ThroughputPhase::Reference;
            }
            ThroughputPhase::Hold => {}
        }
        self.active
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

#[cfg(test)]
mod throughput_tests {
    use super::*;

    fn sample(c: &mut ThroughputController, rate: usize) -> usize {
        c.sample(rate, Duration::from_secs(1), false)
    }

    #[test]
    fn coarse_probe_retries_noise_and_requires_two_votes() {
        let mut c = ThroughputController::new(ThroughputThreads::default(), 16);
        assert_eq!(sample(&mut c, 1000), 8);
        assert_eq!(sample(&mut c, 790), 16); // No permanent first-failure hold.
        assert!(!c.holding());
        assert_eq!(sample(&mut c, 1000), 8);
        assert_eq!(sample(&mut c, 800), 16); // Inclusive 80% boundary.
        assert_eq!(c.accepted, 16); // One pass cannot accept.
        assert_eq!(sample(&mut c, 1000), 8);
        assert_eq!(sample(&mut c, 810), 16);
        assert_eq!(c.accepted, 8);
        assert_eq!(sample(&mut c, 1000), 4);
    }

    #[test]
    fn every_candidate_uses_fresh_initial_reference_without_compounding_loss() {
        let mut c = ThroughputController::new(ThroughputThreads::default(), 16);
        for _ in 0..2 {
            assert_eq!(sample(&mut c, 1000), 8);
            assert_eq!(sample(&mut c, 800), 16);
        }
        for _ in 0..2 {
            assert_eq!(sample(&mut c, 1000), 4);
            assert_eq!(sample(&mut c, 640), 16); // 80% of 8's rate is insufficient.
        }
        assert_eq!(sample(&mut c, 1000), 6); // Refine the 4..8 bracket.
        assert_eq!(c.accepted, 8);
        assert_eq!(c.rejected, 4);
    }

    #[test]
    fn bounded_refinement_finds_smallest_sufficient_count() {
        for initial in [1_usize, 2, 3, 8, 16, 31, 127] {
            for target in 1..=initial {
                let mut c = ThroughputController::new(ThroughputThreads::default(), initial);
                let mut windows = 0;
                while !c.holding() {
                    let rate = if c.active >= target { 1000 } else { 700 };
                    let active = sample(&mut c, rate);
                    assert!((1..=initial).contains(&active));
                    windows += 1;
                    assert!(windows <= 8 * initial.ilog2() + 8);
                }
                assert_eq!(c.active, target);
            }
        }
    }

    #[test]
    fn empty_and_backpressured_windows_cannot_accept_even_zero_target() {
        for blocked in [false, true] {
            let mut c = ThroughputController::new(
                ThroughputThreads {
                    retained_throughput: 0.0,
                    ..ThroughputThreads::default()
                },
                16,
            );
            for _ in 0..10 {
                assert_eq!(sample(&mut c, 0), 16); // Initial streaming gap waits.
            }
            for _ in 0..3 {
                assert_eq!(sample(&mut c, 1000), 8);
                assert_eq!(
                    c.sample(
                        if blocked { 1000 } else { 0 },
                        Duration::from_secs(1),
                        blocked
                    ),
                    16
                );
            }
            assert!(c.holding());
        }
    }

    #[test]
    fn invalid_later_probe_restores_last_accepted_count() {
        let mut c = ThroughputController::new(ThroughputThreads::default(), 16);
        for _ in 0..2 {
            sample(&mut c, 1000);
            sample(&mut c, 1000);
        }
        for _ in 0..3 {
            sample(&mut c, 1000);
            sample(&mut c, 0);
        }
        assert!(c.holding());
        assert_eq!(c.active, 8);
    }

    #[test]
    fn config_normalization_intervals_and_policy_precedence() {
        let cfg = ThroughputThreads {
            max_threads: 0,
            baseline_interval: Duration::ZERO,
            adjustment_interval: Duration::ZERO,
            retained_throughput: f64::NAN,
        }
        .normalized();
        assert_eq!(cfg.max_threads, 1);
        assert_eq!(cfg.baseline_interval, Duration::from_millis(1));
        assert_eq!(cfg.adjustment_interval, Duration::from_millis(1));
        assert!((cfg.retained_throughput - 0.80).abs() < f64::EPSILON);
        let policy = Policy::for_options(crate::Options {
            adaptive_threads: Some(AdaptiveThreads {
                max_threads: 2,
                ..AdaptiveThreads::default()
            }),
            throughput_threads: Some(ThroughputThreads {
                max_threads: 8,
                ..ThroughputThreads::default()
            }),
            ..crate::Options::default()
        })
        .unwrap();
        assert_eq!(policy.max_threads(), 8);
        let mut c = policy.controller(16);
        assert_eq!(c.sample(1000, Duration::ZERO, false), 8);
        assert_eq!(c.sample(1000, Duration::from_secs(1), false), 4);
    }
}
