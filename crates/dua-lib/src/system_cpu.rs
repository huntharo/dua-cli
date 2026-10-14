//! Rootless system-wide CPU accounting and worker-pressure hysteresis.

#[derive(Clone, Copy)]
struct Ticks {
    busy: u64,
    idle: u64,
}

/// Samples system-wide CPU utilization across logical processors, including other processes.
///
/// [`Self::sample`] returns a fraction in `0.0..=1.0` from changes in cumulative OS
/// counters, not process CPU percentages or load averages. Samples are supported on macOS,
/// Linux and single-processor-group Windows. Unsupported systems and unavailable counters
/// return `None`. This does not measure disk utilization or container CPU quotas.
#[derive(Default)]
pub struct SystemCpuSampler {
    previous: Option<Ticks>,
}

impl SystemCpuSampler {
    /// Read a new sample. The first successful read primes the counters and returns `None`.
    /// Zero-duration counter intervals, counter resets and read failures also return `None`.
    /// Sampling around twice a second avoids interpreting timer-resolution noise as load.
    pub fn sample(&mut self) -> Option<f64> {
        let current = read_ticks();
        let previous = std::mem::replace(&mut self.previous, current);
        utilization(previous?, current?)
    }
}

fn utilization(previous: Ticks, current: Ticks) -> Option<f64> {
    let busy = current.busy.checked_sub(previous.busy)?;
    let idle = current.idle.checked_sub(previous.idle)?;
    let total = busy.checked_add(idle)?;
    (total != 0).then(|| busy as f64 / total as f64)
}

#[cfg(target_os = "macos")]
#[allow(deprecated)] // Use existing libc Mach bindings without an additional dependency.
fn read_ticks() -> Option<Ticks> {
    // One process-lifetime host send right, rather than acquiring/leaking one per sample.
    static HOST: std::sync::OnceLock<libc::mach_port_t> = std::sync::OnceLock::new();
    let host = *HOST.get_or_init(|| unsafe { libc::mach_host_self() });
    let mut info = libc::host_cpu_load_info { cpu_ticks: [0; 4] };
    let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
    // SAFETY: info is the four-natural_t output buffer required by HOST_CPU_LOAD_INFO.
    let result = unsafe {
        libc::host_statistics(
            host,
            libc::HOST_CPU_LOAD_INFO,
            std::ptr::from_mut(&mut info).cast(),
            &raw mut count,
        )
    };
    if result != libc::KERN_SUCCESS || count != libc::HOST_CPU_LOAD_INFO_COUNT {
        return None;
    }
    Some(Ticks {
        busy: u64::from(info.cpu_ticks[0])
            + u64::from(info.cpu_ticks[1])
            + u64::from(info.cpu_ticks[3]),
        idle: u64::from(info.cpu_ticks[2]),
    })
}

#[cfg(target_os = "linux")]
fn read_ticks() -> Option<Ticks> {
    parse_proc_stat(&std::fs::read_to_string("/proc/stat").ok()?)
}

#[cfg(any(target_os = "linux", test))]
fn parse_proc_stat(text: &str) -> Option<Ticks> {
    let mut fields = text.lines().next()?.split_whitespace();
    if fields.next()? != "cpu" {
        return None;
    }
    let values: Vec<u64> = fields
        .take(8)
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    if values.len() < 4 {
        return None;
    }
    // guest/guest_nice are already included in user/nice. iowait is not executing CPU work.
    let idle = values[3].checked_add(*values.get(4).unwrap_or(&0))?;
    let busy = values
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != 3 && *i != 4)
        .try_fold(0_u64, |sum, (_, v)| sum.checked_add(*v))?;
    Some(Ticks { busy, idle })
}

#[cfg(windows)]
fn read_ticks() -> Option<Ticks> {
    use windows_sys::Win32::{
        Foundation::FILETIME,
        System::Threading::{GetActiveProcessorGroupCount, GetSystemTimes},
    };
    // GetSystemTimes covers only the calling processor group on >64-CPU systems.
    // Do not mislabel one group's usage as whole-machine CPU.
    if unsafe { GetActiveProcessorGroupCount() } != 1 {
        return None;
    }
    let mut idle = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut kernel = idle;
    let mut user = idle;
    if unsafe { GetSystemTimes(&raw mut idle, &raw mut kernel, &raw mut user) } == 0 {
        return None;
    }
    let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    let idle = ticks(idle);
    // Windows kernel time includes idle time.
    let busy = ticks(kernel).checked_sub(idle)?.checked_add(ticks(user))?;
    Some(Ticks { busy, idle })
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn read_ticks() -> Option<Ticks> {
    None
}

pub(crate) const SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

pub(crate) struct Governor {
    limit: f64,
    pub(crate) cap: usize,
    pub(crate) overloaded: bool,
    high: u8,
    low: u8,
}

impl Governor {
    pub(crate) fn new(limit: f64, initial: usize) -> Self {
        Self {
            limit,
            cap: initial,
            overloaded: false,
            high: 0,
            low: 0,
        }
    }

    pub(crate) fn limited(&self, desired: usize) -> bool {
        self.overloaded || self.cap < desired
    }

    pub(crate) fn sample(
        &mut self,
        usage: Option<f64>,
        active: usize,
        desired: usize,
        settled: bool,
    ) {
        let Some(usage) = usage.filter(|v| v.is_finite() && (0.0..=1.0).contains(v)) else {
            self.high = 0;
            self.low = 0;
            return; // Missing data is not evidence of spare capacity; keep any existing cap.
        };
        self.overloaded = usage > self.limit;
        if self.overloaded {
            self.low = 0;
            self.high = (self.high + 1).min(2);
            if self.high == 2 && settled {
                self.cap = (active / 2).max(1);
                self.high = 0;
            }
        } else if usage < (self.limit - 0.05).max(0.0) {
            self.high = 0;
            self.low = (self.low + 1).min(4);
            if self.low == 4 && settled {
                self.cap = self.cap.saturating_add(1).min(desired.max(self.cap));
                self.low = 0;
            }
        } else {
            self.high = 0;
            self.low = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utilization_uses_deltas_and_rejects_missing_or_reset_intervals() {
        let first = Ticks {
            busy: 100,
            idle: 100,
        };
        assert!(
            (utilization(
                first,
                Ticks {
                    busy: 180,
                    idle: 120
                }
            )
            .unwrap()
                - 0.8)
                .abs()
                < f64::EPSILON
        );
        assert!(utilization(first, first).is_none());
        assert!(utilization(first, Ticks { busy: 0, idle: 0 }).is_none());
    }

    #[test]
    fn linux_excludes_guest_double_counting_and_iowait_from_busy() {
        let ticks = parse_proc_stat("cpu 10 20 30 40 50 60 70 80 1000 2000\ncpu0 0").unwrap();
        assert_eq!(ticks.busy, 270);
        assert_eq!(ticks.idle, 90);
        assert!(parse_proc_stat("cpu0 1 2 3 4").is_none());
        assert!(parse_proc_stat("cpu 1 invalid 2 3").is_none());
    }

    #[test]
    fn pressure_halves_to_one_and_recovers_one_at_a_time_with_hysteresis() {
        let mut g = Governor::new(0.8, 16);
        let mut active = 16;
        for expected in [8, 4, 2, 1, 1] {
            g.sample(Some(0.95), active, 16, true);
            assert_eq!(g.cap, active);
            g.sample(Some(0.95), active, 16, true);
            assert_eq!(g.cap, expected);
            active = expected;
        }
        for expected in 2..=16 {
            for _ in 0..3 {
                g.sample(Some(0.70), active, 16, true);
                assert_eq!(g.cap, active);
            }
            g.sample(Some(0.70), active, 16, true);
            assert_eq!(g.cap, expected);
            active = expected;
        }
        assert!(!g.limited(16));
    }

    #[test]
    fn deadband_missing_data_and_unsettled_workers_do_not_cause_recovery() {
        let mut g = Governor::new(0.8, 8);
        for _ in 0..2 {
            g.sample(Some(0.9), 8, 8, true);
        }
        assert_eq!(g.cap, 4);
        for usage in [Some(0.78), None, Some(f64::NAN)] {
            for _ in 0..20 {
                g.sample(usage, 4, 8, true);
            }
            assert_eq!(g.cap, 4);
        }
        for _ in 0..20 {
            g.sample(Some(0.5), 4, 8, false);
        }
        assert_eq!(g.cap, 4);
        g.sample(Some(0.5), 4, 8, true);
        assert_eq!(g.cap, 5);
    }
}
