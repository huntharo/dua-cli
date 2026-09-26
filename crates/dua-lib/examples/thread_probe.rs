//! A core traversal comparison harness restricted to ~/github.
//! Usage: `thread_probe adaptive|fixed THREADS PATH [BASELINE_MS PROBE_MS LOSS_PERCENT]`
use dua_core::{AdaptiveThreads, Options, Order};
use std::{
    error::Error,
    path::PathBuf,
    time::{Duration, Instant},
};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 3 && args.len() != 6 {
        return Err(
            "usage: thread_probe adaptive|fixed THREADS PATH [BASELINE_MS PROBE_MS LOSS_PERCENT]"
                .into(),
        );
    }
    let adaptive = match args[0].to_str() {
        Some("adaptive") => true,
        Some("fixed") => false,
        _ => return Err("mode must be adaptive or fixed".into()),
    };
    let threads: usize = args[1].to_str().ok_or("invalid thread count")?.parse()?;
    if threads == 0 {
        return Err("THREADS must be at least 1".into());
    }
    let path = PathBuf::from(&args[2]).canonicalize()?;
    let allowed = PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set")?)
        .join("github")
        .canonicalize()?;
    if !path.starts_with(&allowed) {
        return Err("comparison scans must stay within ~/github".into());
    }
    let mut config = AdaptiveThreads::default();
    if args.len() == 6 {
        config.baseline_interval =
            Duration::from_millis(args[3].to_str().ok_or("invalid baseline")?.parse()?);
        config.adjustment_interval =
            Duration::from_millis(args[4].to_str().ok_or("invalid probe")?.parse()?);
        let percent: f64 = args[5].to_str().ok_or("invalid loss threshold")?.parse()?;
        if config.baseline_interval.is_zero()
            || config.adjustment_interval.is_zero()
            || !(0.0..=100.0).contains(&percent)
        {
            return Err(
                "intervals must be positive milliseconds; loss percentage must be in 0..=100"
                    .into(),
            );
        }
        config.loss_threshold = percent / 100.0;
    }
    let start = Instant::now();
    let mut walk = dua_core::walk(
        &path,
        threads,
        Order::Completion,
        Options {
            adaptive_threads: adaptive.then_some(config),
            ..Options::default()
        },
        |_| true,
    );
    let mut entries = 0_u64;
    let mut errors = 0_u64;
    let mut state = (walk.active_threads(), walk.threads_settled());
    eprintln!("elapsed_ms,admitted,retirement_settled,entries,errors");
    eprintln!("0,{},{},0,0", state.0, state.1);
    while let Some(entry) = walk.next() {
        match entry {
            Ok(entry) => {
                entries += 1;
                if entry.metadata.as_ref().is_some_and(Result::is_err) {
                    errors += 1;
                }
            }
            Err(_) => errors += 1,
        }
        let current = (walk.active_threads(), walk.threads_settled());
        if current != state {
            state = current;
            eprintln!(
                "{},{},{},{entries},{errors}",
                start.elapsed().as_millis(),
                state.0,
                state.1
            );
        }
    }
    println!(
        "seconds={:.6} entries={entries} errors={errors} admitted={} retirement_settled={}",
        start.elapsed().as_secs_f64(),
        walk.active_threads(),
        walk.threads_settled()
    );
    Ok(())
}
