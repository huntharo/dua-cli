use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{Duration, Instant},
};
#[derive(Deserialize)]
struct Config {
    threads: usize,
    roots: Vec<PathBuf>,
    #[serde(default)]
    partitions: Vec<Vec<PathBuf>>,
    #[serde(default = "one")]
    repeats: usize,
    #[serde(default)]
    min_seconds: f64,
    #[serde(default)]
    skip_metadata: bool,
    #[serde(default)]
    calibrate: bool,
    #[serde(default = "default_strategy")]
    strategy: String,
}
fn default_strategy() -> String {
    "adaptive".into()
}
fn one() -> usize {
    1
}
#[derive(Default, Serialize)]
struct Count {
    entries: u64,
    errors: u64,
    bytes: u64,
    dirs: u64,
    allocated_bytes: u64,
    devices: std::collections::BTreeSet<u64>,
}
impl Count {
    fn add(&mut self, e: std::io::Result<dua_core::Entry>) {
        match e {
            Ok(e) => {
                self.entries += 1;
                self.dirs += u64::from(e.file_type.is_dir());
                if let Some(m) = e.metadata {
                    match m {
                        Ok(m) => {
                            self.bytes = self.bytes.wrapping_add(m.len());
                            self.allocated_bytes =
                                self.allocated_bytes.wrapping_add(m.allocated_size());
                            self.devices.insert(m.dev());
                        }
                        Err(_) => self.errors += 1,
                    }
                }
            }
            Err(_) => self.errors += 1,
        }
    }
}
fn usage() -> libc::rusage {
    let mut r = std::mem::MaybeUninit::uninit();
    // SAFETY: getrusage writes the entire correctly sized structure on success.
    unsafe {
        assert_eq!(libc::getrusage(libc::RUSAGE_SELF, r.as_mut_ptr()), 0);
        r.assume_init()
    }
}
fn seconds(t: libc::timeval) -> f64 {
    t.tv_sec as f64 + t.tv_usec as f64 / 1e6
}
fn filesystem(path: &std::path::Path) -> serde_json::Value {
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    let mut fs = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: name is NUL-terminated; statfs initializes the correctly sized output on success.
    let fs = unsafe {
        assert_eq!(libc::statfs(name.as_ptr(), fs.as_mut_ptr()), 0);
        fs.assume_init()
    };
    // SAFETY: successful statfs returns NUL-terminated filesystem and mount names.
    let (kind, mount, source) = unsafe {
        (
            std::ffi::CStr::from_ptr(fs.f_fstypename.as_ptr()).to_string_lossy(),
            std::ffi::CStr::from_ptr(fs.f_mntonname.as_ptr()).to_string_lossy(),
            std::ffi::CStr::from_ptr(fs.f_mntfromname.as_ptr()).to_string_lossy(),
        )
    };
    serde_json::json!({"type":kind,"mount":mount,"source":source,"block_size":fs.f_bsize})
}
// libc retains the native Mach timebase ABI; this standalone macOS probe uses it directly.
#[allow(deprecated)]
fn proc_stats() -> serde_json::Value {
    // These C data structures consist only of integer fields and fixed byte arrays.
    let mut ri: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    let mut ti: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let mut tb: libc::mach_timebase_info = unsafe { std::mem::zeroed() };
    // SAFETY: all output pointers have the matching ABI and live for each call.
    unsafe {
        assert_eq!(
            libc::proc_pid_rusage(libc::getpid(), libc::RUSAGE_INFO_V4, (&raw mut ri).cast()),
            0
        );
        assert_eq!(
            libc::proc_pidinfo(
                libc::getpid(),
                libc::PROC_PIDTASKINFO,
                0,
                (&raw mut ti).cast(),
                std::mem::size_of_val(&ti) as i32
            ),
            std::mem::size_of_val(&ti) as i32
        );
        libc::mach_timebase_info(&raw mut tb);
    }
    serde_json::json!({"disk_read_bytes":ri.ri_diskio_bytesread,"disk_write_bytes":ri.ri_diskio_byteswritten,"unix_syscalls":ti.pti_syscalls_unix,"mach_syscalls":ti.pti_syscalls_mach,"context_switches":ti.pti_csw,"runnable_seconds":ri.ri_runnable_time as f64*tb.numer as f64/tb.denom as f64/1e9})
}
fn main() {
    let path = std::env::args().nth(1).expect("JSON config path");
    if path == "--filesystem" {
        println!(
            "{}",
            filesystem(std::path::Path::new(
                &std::env::args().nth(2).expect("path")
            ))
        );
        return;
    }
    let c: Config = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert!(c.threads > 0 && (!c.roots.is_empty() || !c.partitions.is_empty()));
    let allowed = PathBuf::from(std::env::var_os("HOME").expect("HOME"))
        .join("github")
        .canonicalize()
        .unwrap();
    for root in c.roots.iter().chain(c.partitions.iter().flatten()) {
        assert!(
            root.canonicalize().unwrap().starts_with(&allowed),
            "profiling roots must stay within ~/github"
        );
    }
    let mut opt = dua_core::Options::default();
    opt.skip_metadata = c.skip_metadata;
    opt.macos_metadata_strategy = match c.strategy.as_str() {
        "adaptive" => dua_core::MacosMetadataStrategy::Adaptive,
        "bulk" => dua_core::MacosMetadataStrategy::Bulk,
        "directory-local" => dua_core::MacosMetadataStrategy::DirectoryLocal,
        "inode-ordered" => dua_core::MacosMetadataStrategy::InodeOrdered,
        other => panic!("unknown strategy {other}"),
    };
    let proc_before = proc_stats();
    let before = usage();
    let start = Instant::now();
    let mut total = Count::default();
    let mut passes = 0;
    let mut roots = BTreeMap::new();
    loop {
        if !c.partitions.is_empty() {
            let results = std::thread::scope(|scope| {
                let handles: Vec<_> = c
                    .partitions
                    .iter()
                    .map(|roots| {
                        scope.spawn(move || {
                            let mut count = Count::default();
                            for (_, e) in dua_core::walk_roots(
                                roots.iter().cloned().enumerate(),
                                1,
                                dua_core::Order::Completion,
                                opt,
                                |_, _| true,
                            ) {
                                if let dua_core::RootEvent::Entry(e) = e {
                                    count.add(e)
                                }
                            }
                            count
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().unwrap())
                    .collect::<Vec<_>>()
            });
            for count in results {
                total.entries += count.entries;
                total.errors += count.errors;
                total.bytes = total.bytes.wrapping_add(count.bytes);
                total.dirs += count.dirs;
                total.allocated_bytes = total.allocated_bytes.wrapping_add(count.allocated_bytes);
                total.devices.extend(count.devices.iter().copied());
            }
        } else if c.calibrate {
            for p in &c.roots {
                let s = Instant::now();
                let mut count = Count::default();
                for e in dua_core::walk(p, c.threads, dua_core::Order::Completion, opt, |_| true) {
                    count.add(e)
                }
                roots.insert(
                    p.to_string_lossy().into_owned(),
                    serde_json::json!({"seconds":s.elapsed().as_secs_f64(),"count":count}),
                );
                total.entries += count.entries;
                total.errors += count.errors;
                total.bytes = total.bytes.wrapping_add(count.bytes);
                total.dirs += count.dirs;
                total.allocated_bytes = total.allocated_bytes.wrapping_add(count.allocated_bytes);
                total.devices.extend(count.devices.iter().copied());
            }
        } else {
            for (_, e) in dua_core::walk_roots(
                c.roots.iter().cloned().enumerate(),
                c.threads,
                dua_core::Order::Completion,
                opt,
                |_, _| true,
            ) {
                if let dua_core::RootEvent::Entry(e) = e {
                    total.add(e)
                }
            }
        }
        passes += 1;
        if passes >= c.repeats && start.elapsed() >= Duration::from_secs_f64(c.min_seconds) {
            break;
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    let after = usage();
    let proc_after = proc_stats();
    println!(
        "{}",
        serde_json::json!({"strategy":c.strategy,"proc_before":proc_before,"proc_after":proc_after,"threads":c.threads,"passes":passes,"wall_seconds":elapsed,"user_seconds":seconds(after.ru_utime)-seconds(before.ru_utime),"system_seconds":seconds(after.ru_stime)-seconds(before.ru_stime),"voluntary_switches":after.ru_nvcsw-before.ru_nvcsw,"involuntary_switches":after.ru_nivcsw-before.ru_nivcsw,"count":total,"roots":roots})
    );
}
