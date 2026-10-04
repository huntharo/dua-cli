//! Read-only whole-machine CPU samples. Usage: `system_cpu_probe [sample_count]`.
use dua_core::SystemCpuSampler;
use std::time::{Duration, Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let count: usize = std::env::args().nth(1).map_or(Ok(6), |n| n.parse())?;
    let mut sampler = SystemCpuSampler::default();
    sampler.sample();
    let start = Instant::now();
    println!("elapsed_ms,system_cpu_percent");
    for _ in 0..count {
        std::thread::sleep(Duration::from_millis(500));
        let percent = sampler
            .sample()
            .map_or_else(String::new, |v| format!("{:.4}", v * 100.0));
        println!("{},{}", start.elapsed().as_millis(), percent);
    }
    Ok(())
}
