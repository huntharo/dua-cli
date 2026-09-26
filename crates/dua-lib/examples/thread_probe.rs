//! A core traversal comparison harness restricted to ~/github unless --allow-home is explicitly supplied.
//! Usage: `thread_probe adaptive|throughput|fixed THREADS PATH [BASELINE_MS PROBE_MS PERCENT] [--allow-home] [--cpu-log PATH]`
use dua_core::{AdaptiveThreads, Options, Order, SystemCpuSampler, ThroughputThreads};
use std::{
    error::Error,
    io::Write,
    path::PathBuf,
    time::{Duration, Instant},
};

fn main() -> Result<(), Box<dyn Error>> {
    let mut args: Vec<_> = std::env::args_os().skip(1).collect();
    let cpu_log_path = if let Some(index) = args.iter().position(|arg| arg == "--cpu-log") {
        if index + 1 >= args.len() {
            return Err("--cpu-log requires a path".into());
        }
        let path = PathBuf::from(args.remove(index + 1));
        args.remove(index);
        Some(path)
    } else {
        None
    };
    let allow_home = args.last().is_some_and(|arg| arg == "--allow-home");
    if allow_home {
        args.pop();
    }
    if args.len() != 3 && args.len() != 6 {
        return Err(
            "usage: thread_probe adaptive|throughput|fixed THREADS PATH [BASELINE_MS PROBE_MS PERCENT] [--allow-home] [--cpu-log PATH]"
                .into(),
        );
    }
    let mode = args[0].to_str().ok_or("invalid mode")?;
    if !["adaptive", "throughput", "fixed"].contains(&mode) {
        return Err("mode must be adaptive, throughput, or fixed".into());
    }
    let threads: usize = args[1].to_str().ok_or("invalid thread count")?.parse()?;
    if threads == 0 {
        return Err("THREADS must be at least 1".into());
    }
    let path = PathBuf::from(&args[2]).canonicalize()?;
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set")?);
    let allowed = if allow_home {
        home
    } else {
        home.join("github")
    }
    .canonicalize()?;
    if !path.starts_with(&allowed) {
        return Err(
            "comparison scan is outside the allowed root (~/github, or ~ with --allow-home)".into(),
        );
    }
    let mut config = AdaptiveThreads::default();
    let mut retained_throughput = ThroughputThreads::default().retained_throughput;
    if args.len() == 6 {
        config.baseline_interval =
            Duration::from_millis(args[3].to_str().ok_or("invalid baseline")?.parse()?);
        config.adjustment_interval =
            Duration::from_millis(args[4].to_str().ok_or("invalid probe")?.parse()?);
        let percent: f64 = args[5].to_str().ok_or("invalid percentage")?.parse()?;
        if config.baseline_interval.is_zero()
            || config.adjustment_interval.is_zero()
            || !(0.0..=100.0).contains(&percent)
        {
            return Err(
                "intervals must be positive milliseconds; percentage must be in 0..=100".into(),
            );
        }
        config.loss_threshold = percent / 100.0;
        retained_throughput = percent / 100.0;
    }
    let start = Instant::now();
    let cpu_log = cpu_log_path
        .map(|path| CpuLog::start(path, start))
        .transpose()?;
    let mut walk = dua_core::walk(
        &path,
        threads,
        Order::Completion,
        Options {
            adaptive_threads: (mode == "adaptive").then_some(config),
            throughput_threads: (mode == "throughput").then_some(ThroughputThreads {
                max_threads: config.max_threads,
                baseline_interval: config.baseline_interval,
                adjustment_interval: config.adjustment_interval,
                retained_throughput,
                ..ThroughputThreads::default()
            }),
            ..Options::default()
        },
        |_| true,
    );
    let mut entries = 0_u64;
    let mut errors = 0_u64;
    let mut state = (
        walk.active_threads(),
        walk.threads_settled(),
        walk.thread_tuning_complete(),
        walk.system_cpu_limited(),
    );
    let mut heartbeat = Instant::now();
    let mut directories = 0_u64;
    let mut logical_bytes = 0_u128;
    eprintln!("elapsed_ms,admitted,retirement_settled,tuning_complete,cpu_limited,entries,errors");
    eprintln!("0,{},{},{},{},0,0", state.0, state.1, state.2, state.3);
    while let Some(entry) = walk.next() {
        match entry {
            Ok(entry) => {
                entries += 1;
                directories += u64::from(entry.file_type.is_dir());
                if let Some(Ok(metadata)) = &entry.metadata {
                    logical_bytes += u128::from(metadata.len());
                }
                if entry.metadata.as_ref().is_some_and(Result::is_err) {
                    errors += 1;
                }
            }
            Err(_) => errors += 1,
        }
        let current = (
            walk.active_threads(),
            walk.threads_settled(),
            walk.thread_tuning_complete(),
            walk.system_cpu_limited(),
        );
        if current != state || heartbeat.elapsed() >= Duration::from_secs(1) {
            heartbeat = Instant::now();
            state = current;
            eprintln!(
                "{},{},{},{},{},{entries},{errors}",
                start.elapsed().as_millis(),
                state.0,
                state.1,
                state.2,
                state.3
            );
        }
    }
    if let Some(log) = cpu_log {
        log.finish()?;
    }
    println!(
        "seconds={:.6} entries={entries} directories={directories} logical_bytes={logical_bytes} errors={errors} admitted={} retirement_settled={} tuning_complete={} cpu_limited={}",
        start.elapsed().as_secs_f64(),
        walk.active_threads(),
        walk.threads_settled(),
        walk.thread_tuning_complete(),
        walk.system_cpu_limited()
    );
    Ok(())
}

// Independent sampling continues even while Walk::next is blocked in filesystem work.
struct CpuLog {
    stop: std::sync::mpsc::Sender<()>,
    handle: std::thread::JoinHandle<std::io::Result<()>>,
}

impl CpuLog {
    fn start(path: PathBuf, start: Instant) -> std::io::Result<Self> {
        let mut file = std::fs::File::create(path)?;
        let (stop, receiver) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let mut sampler = SystemCpuSampler::default();
            writeln!(file, "elapsed_ms,system_cpu_percent")?;
            loop {
                let value = sampler
                    .sample()
                    .map_or_else(String::new, |v| format!("{:.4}", v * 100.0));
                writeln!(file, "{},{}", start.elapsed().as_millis(), value)?;
                file.flush()?;
                if receiver.recv_timeout(Duration::from_millis(500))
                    != Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                {
                    return Ok(());
                }
            }
        });
        Ok(Self { stop, handle })
    }

    fn finish(self) -> std::io::Result<()> {
        let _ = self.stop.send(());
        self.handle
            .join()
            .map_err(|_| std::io::Error::other("CPU sampler panicked"))?
    }
}
