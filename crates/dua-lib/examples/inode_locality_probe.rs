//! Single-directory diagnostic, restricted to canonical paths within ~/github.
//! Usage: `inode_locality_probe PATH THREADS enumeration|inode`
//!
//! Pre-enumerates at most one million immediate children. Inode mode stably sorts the entire
//! vector; workers receive balanced, contiguous, disjoint slices with no work stealing. Each
//! worker visits its slice in order, but concurrency does not imply globally ordered syscalls.
//! `stat_seconds` and stat CPU include worker creation/join and checksumming. Overall measurements
//! include enumeration, sorting, and stat work, excluding argument validation and the parent open.
//! Byte totals count every successful entry (including each hard link), without deduplication.
//! Checksums are order-independent diagnostics, not cryptographic proofs. They include names and
//! stable stat fields, excluding access time, which unrelated reads can change between runs.
//! Compare counts, byte totals, and both checksums across runs before comparing performance;
//! this program cannot decide equivalence from a single run. No production traversal code is used.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("inode_locality_probe is supported only on macOS");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    macos::run()
}

#[cfg(target_os = "macos")]
mod macos {
    use std::{
        error::Error,
        ffi::{CStr, CString},
        fs, io,
        mem::MaybeUninit,
        os::{
            fd::{AsRawFd, OwnedFd},
            unix::{
                ffi::OsStringExt,
                fs::{DirEntryExt, OpenOptionsExt},
            },
        },
        path::PathBuf,
        thread,
        time::{Duration, Instant},
    };

    const MAX_ENTRIES: usize = 1_000_000;

    struct Child {
        inode: u64,
        name: CString,
    }

    #[derive(Default)]
    struct Totals {
        successes: u64,
        errors: u64,
        logical_bytes: u128,
        allocated_bytes: u128,
        checksum_sum: u64,
        checksum_xor: u64,
    }

    impl Totals {
        fn merge(&mut self, other: Self) {
            self.successes += other.successes;
            self.errors += other.errors;
            self.logical_bytes += other.logical_bytes;
            self.allocated_bytes += other.allocated_bytes;
            self.checksum_sum = self.checksum_sum.wrapping_add(other.checksum_sum);
            self.checksum_xor ^= other.checksum_xor;
        }
    }

    /// Independent byte hash accumulators, reduced commutatively across entries and workers.
    struct Fingerprint(u64, u64);

    impl Fingerprint {
        fn new(name: &CStr) -> Self {
            let mut hash = Self(0xcbf2_9ce4_8422_2325, 0x9e37_79b9_7f4a_7c15);
            hash.number(name.to_bytes().len() as u64);
            hash.bytes(name.to_bytes());
            hash
        }

        fn bytes(&mut self, bytes: &[u8]) {
            for &byte in bytes {
                self.0 = (self.0 ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
                self.1 =
                    (self.1.rotate_left(7) ^ u64::from(byte)).wrapping_mul(0x9e37_79b1_85eb_ca87);
            }
        }

        fn number(&mut self, number: u64) {
            self.bytes(&number.to_le_bytes());
        }

        #[allow(clippy::cast_sign_loss)] // Preserve signed stat fields as their exact bit patterns.
        fn metadata(&mut self, stat: &libc::stat) {
            // Fixed-width little-endian serialization avoids struct padding and platform hash seeds.
            for field in [
                1, // Successful metadata, distinct from the error encoding.
                stat.st_dev as u64,
                stat.st_ino,
                u64::from(stat.st_mode),
                u64::from(stat.st_nlink),
                u64::from(stat.st_uid),
                u64::from(stat.st_gid),
                stat.st_rdev as u64,
                stat.st_size as u64,
                stat.st_blocks as u64,
                stat.st_blksize as u64,
                stat.st_mtime as u64,
                stat.st_mtime_nsec as u64,
                stat.st_ctime as u64,
                stat.st_ctime_nsec as u64,
                stat.st_birthtime as u64,
                stat.st_birthtime_nsec as u64,
                u64::from(stat.st_flags),
                u64::from(stat.st_gen),
            ] {
                self.number(field);
            }
        }
    }

    fn stat_at(directory: &OwnedFd, name: &CStr) -> io::Result<libc::stat> {
        let mut stat = MaybeUninit::<libc::stat>::uninit();
        loop {
            // SAFETY: directory is open for the entire scoped worker phase, name is NUL-terminated,
            // and stat is writable for its full size. NOFOLLOW includes dangling links as entries.
            let result = unsafe {
                libc::fstatat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if result == 0 {
                // SAFETY: successful fstatat initializes the stat fields read by this diagnostic.
                return Ok(unsafe { stat.assume_init() });
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    fn stat_slice(directory: &OwnedFd, children: &[Child]) -> Totals {
        let mut totals = Totals::default();
        for child in children {
            let mut fingerprint = Fingerprint::new(&child.name);
            match stat_at(directory, &child.name) {
                Ok(stat) => {
                    totals.successes += 1;
                    // As in std's MetadataExt, sizes and block counts are unsigned metadata.
                    // Use u128 sums to avoid overflowing even across a million very large files.
                    totals.logical_bytes += u128::from(stat.st_size.cast_unsigned());
                    totals.allocated_bytes += u128::from(stat.st_blocks.cast_unsigned()) * 512;
                    fingerprint.metadata(&stat);
                }
                Err(error) => {
                    totals.errors += 1;
                    fingerprint.number(0);
                    fingerprint
                        .number(u64::from(error.raw_os_error().unwrap_or(0).cast_unsigned()));
                }
            }
            totals.checksum_sum = totals.checksum_sum.wrapping_add(fingerprint.0);
            totals.checksum_xor ^= fingerprint.1;
        }
        totals
    }

    fn stat_parallel(
        directory: &OwnedFd,
        children: &[Child],
        threads: usize,
    ) -> io::Result<Totals> {
        thread::scope(|scope| {
            let mut handles = Vec::new();
            let base = children.len() / threads;
            let remainder = children.len() % threads;
            for index in 0..threads {
                // Slice lengths differ by at most one. Empty slices still receive a fixed worker.
                let start = index * base + index.min(remainder);
                let length = base + usize::from(index < remainder);
                let slice = &children[start..start + length];
                handles.push(
                    thread::Builder::new()
                        .spawn_scoped(scope, move || stat_slice(directory, slice))?,
                );
            }
            let mut totals = Totals::default();
            for handle in handles {
                totals.merge(
                    handle
                        .join()
                        .map_err(|_| io::Error::other("stat worker panicked"))?,
                );
            }
            Ok(totals)
        })
    }

    struct Usage {
        user: Duration,
        system: Duration,
        disk_read_bytes: u64,
    }

    fn cpu_time(time: libc::timeval) -> io::Result<Duration> {
        let seconds = u64::try_from(time.tv_sec).map_err(io::Error::other)?;
        let micros = u32::try_from(time.tv_usec).map_err(io::Error::other)?;
        if micros >= 1_000_000 {
            return Err(io::Error::other("invalid getrusage timeval"));
        }
        Ok(Duration::new(seconds, micros * 1000))
    }

    fn usage() -> io::Result<Usage> {
        let mut cpu = MaybeUninit::<libc::rusage>::uninit();
        // SAFETY: RUSAGE_SELF writes a full rusage to this suitably sized, aligned destination.
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, cpu.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: getrusage succeeded, initializing ru_utime and ru_stime.
        let cpu = unsafe { cpu.assume_init() };
        let mut disk = MaybeUninit::<libc::rusage_info_v2>::uninit();
        // SAFETY: the V2 flavor writes rusage_info_v2 into this buffer. Darwin's declaration uses
        // an opaque rusage_info_t pointer; cast the actual output buffer, not a pointer variable.
        if unsafe {
            libc::proc_pid_rusage(
                libc::getpid(),
                libc::RUSAGE_INFO_V2,
                disk.as_mut_ptr().cast(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the successful V2 call initializes ri_diskio_bytesread. No Mach CPU fields are used.
        let disk = unsafe { disk.assume_init() };
        Ok(Usage {
            user: cpu_time(cpu.ru_utime)?,
            system: cpu_time(cpu.ru_stime)?,
            disk_read_bytes: disk.ri_diskio_bytesread,
        })
    }

    fn elapsed_cpu(after: Duration, before: Duration) -> io::Result<f64> {
        after
            .checked_sub(before)
            .map(|d| d.as_secs_f64())
            .ok_or_else(|| io::Error::other("getrusage CPU counter moved backwards"))
    }

    fn elapsed_disk(after: u64, before: u64) -> io::Result<u64> {
        after
            .checked_sub(before)
            .ok_or_else(|| io::Error::other("disk-read counter moved backwards"))
    }

    pub fn run() -> Result<(), Box<dyn Error>> {
        let args: Vec<_> = std::env::args_os().skip(1).collect();
        if args.len() != 3 {
            return Err("usage: inode_locality_probe PATH THREADS enumeration|inode".into());
        }
        let threads: usize = args[1].to_str().ok_or("invalid thread count")?.parse()?;
        if threads == 0 {
            return Err("THREADS must be at least 1".into());
        }
        let inode_order = match args[2].to_str() {
            Some("enumeration") => false,
            Some("inode") => true,
            _ => return Err("ORDER must be enumeration or inode".into()),
        };
        let path = PathBuf::from(&args[0]).canonicalize()?;
        let allowed = PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set")?)
            .join("github")
            .canonicalize()?;
        if !path.starts_with(&allowed) {
            return Err("diagnostic directory must stay within ~/github".into());
        }
        let directory: OwnedFd = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY)
            .open(&path)?
            .into();

        let overall_started = Instant::now();
        let before_enumeration = usage()?;
        let enumeration_started = Instant::now();
        let mut children = Vec::new();
        let mut enumeration_errors = 0_u64;
        for (index, entry) in fs::read_dir(&path)?.enumerate() {
            if index >= MAX_ENTRIES {
                return Err(
                    "enumeration exceeded 1,000,000 records; no stat workers were started".into(),
                );
            }
            match entry {
                Ok(entry) => children.push(Child {
                    inode: entry.ino(),
                    name: CString::new(entry.file_name().into_vec())?,
                }),
                Err(_) => enumeration_errors += 1,
            }
        }
        let enumeration_seconds = enumeration_started.elapsed().as_secs_f64();
        let enumeration_inode_descents = children
            .windows(2)
            .filter(|pair| pair[0].inode > pair[1].inode)
            .count();
        let sort_seconds = if inode_order {
            let started = Instant::now();
            children.sort_by_key(|entry| entry.inode);
            started.elapsed().as_secs_f64()
        } else {
            0.0
        };
        let before_stat = usage()?;
        let stat_started = Instant::now();
        let totals = stat_parallel(&directory, &children, threads)?;
        let stat_seconds = stat_started.elapsed().as_secs_f64();
        let after_stat = usage()?;
        let overall_seconds = overall_started.elapsed().as_secs_f64();
        let cpu_stat_user = elapsed_cpu(after_stat.user, before_stat.user)?;
        let cpu_stat_system = elapsed_cpu(after_stat.system, before_stat.system)?;
        let cpu_total_user = elapsed_cpu(after_stat.user, before_enumeration.user)?;
        let cpu_total_system = elapsed_cpu(after_stat.system, before_enumeration.system)?;
        let pre_stat_read = elapsed_disk(
            before_stat.disk_read_bytes,
            before_enumeration.disk_read_bytes,
        )?;
        let stat_read = elapsed_disk(after_stat.disk_read_bytes, before_stat.disk_read_bytes)?;
        let total_read = elapsed_disk(
            after_stat.disk_read_bytes,
            before_enumeration.disk_read_bytes,
        )?;

        // Numeric JSON only: no filename escaping or extra dependencies are needed.
        println!(
            concat!(
                "{{\"threads\":{},\"inode_order\":{},\"enumerated_entries\":{},\"enumeration_inode_descents\":{},",
                "\"stat_successes\":{},\"enumeration_errors\":{},\"stat_errors\":{},\"entry_errors\":{},",
                "\"logical_bytes\":{},\"allocated_bytes\":{},\"checksum_sum\":{},\"checksum_xor\":{},",
                "\"enumeration_seconds\":{:.9},\"sort_seconds\":{:.9},\"stat_seconds\":{:.9},\"overall_seconds\":{:.9},",
                "\"cpu_stat_user_seconds\":{:.6},\"cpu_stat_system_seconds\":{:.6},\"cpu_stat_seconds\":{:.6},",
                "\"cpu_total_user_seconds\":{:.6},\"cpu_total_system_seconds\":{:.6},\"cpu_total_seconds\":{:.6},",
                "\"disk_read_before_enumeration\":{},\"disk_read_before_stat\":{},\"disk_read_after_stat\":{},",
                "\"pre_stat_disk_read_bytes\":{},\"stat_disk_read_bytes\":{},\"total_disk_read_bytes\":{}}}"
            ),
            threads,
            u8::from(inode_order),
            children.len(),
            enumeration_inode_descents,
            totals.successes,
            enumeration_errors,
            totals.errors,
            enumeration_errors + totals.errors,
            totals.logical_bytes,
            totals.allocated_bytes,
            totals.checksum_sum,
            totals.checksum_xor,
            enumeration_seconds,
            sort_seconds,
            stat_seconds,
            overall_seconds,
            cpu_stat_user,
            cpu_stat_system,
            cpu_stat_user + cpu_stat_system,
            cpu_total_user,
            cpu_total_system,
            cpu_total_user + cpu_total_system,
            before_enumeration.disk_read_bytes,
            before_stat.disk_read_bytes,
            after_stat.disk_read_bytes,
            pre_stat_read,
            stat_read,
            total_read,
        );
        Ok(())
    }
}
