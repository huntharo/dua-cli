//! Bounded directory-local experiments. Inode IDs are keys, never physical addresses.

use super::{Entry, FileType, Metadata, STAT_BLOCK_BYTES, VDIR, VLNK, VNON, VREG};
use crate::{MacosMetadataStrategy, Options};
use std::{
    ffi::{CStr, CString, OsString},
    fs, io,
    mem::MaybeUninit,
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd},
        unix::{ffi::OsStringExt, fs::OpenOptionsExt},
    },
    path::Path,
    ptr::NonNull,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub(crate) const RELATIVE_STAT_CHUNK_SIZE: usize = 64;
const SORT_BUFFER_ENTRIES: usize = 4096;

/// A single-owner libc directory stream; only its owner calls `readdir`.
struct DirectoryStream(NonNull<libc::DIR>);

// SAFETY: ownership of the stream is exclusive, including across a thread transfer. Stat jobs
// access only the shared descriptor with fstatat, which neither reads nor changes stream state.
unsafe impl Send for DirectoryStream {}

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        // SAFETY: fdopendir returned this uniquely owned stream; it is closed exactly once.
        unsafe { libc::closedir(self.0.as_ptr()) };
    }
}

pub(crate) struct RelativeEntry {
    entry: Entry,
    directory: Arc<OwnedFd>,
    inode: u64,
}

impl RelativeEntry {
    pub(crate) fn read_metadata(mut self, options: Options) -> Entry {
        // Preserve the existing identity validation and allocation accounting for APFS clones.
        if options.apfs_clone_metadata {
            return self.entry.read_metadata(options);
        }
        let name = CString::new(self.entry.file_name.clone().into_vec())
            .expect("directory names cannot contain NUL");
        self.entry.metadata = Some(stat_at(&self.directory, &name).inspect(|metadata| {
            self.entry.file_type = metadata.file_type;
        }));
        self.entry
    }
}

fn stat_at(directory: &OwnedFd, name: &CStr) -> io::Result<Metadata> {
    let mut metadata = MaybeUninit::<libc::stat>::uninit();
    loop {
        // SAFETY: the Arc-owned directory remains open, name is NUL-terminated and metadata is
        // writable for a complete stat. NOFOLLOW preserves symlinks as entries, including dangling
        // links. fstatat resolves only this basename and does not affect the enumeration offset.
        let result = unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                name.as_ptr(),
                metadata.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result == 0 {
            // SAFETY: successful fstatat initializes every stat field used by from_stat.
            return Ok(Metadata::from_stat(unsafe { metadata.assume_init_ref() }));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

impl Metadata {
    #[allow(clippy::cast_sign_loss)] // Match std's Unix MetadataExt unsigned field conversions.
    fn from_stat(stat: &libc::stat) -> Self {
        let allocated_size = (stat.st_blocks as u64).saturating_mul(STAT_BLOCK_BYTES);
        Self {
            len: stat.st_size as u64,
            allocated_size,
            data_allocated_size: allocated_size,
            clone_id: None,
            modified: modification_time(stat.st_mtime, stat.st_mtime_nsec),
            dev: stat.st_dev as u64,
            ino: stat.st_ino,
            nlink: u64::from(stat.st_nlink),
            file_type: FileType {
                kind: match stat.st_mode & libc::S_IFMT {
                    libc::S_IFDIR => VDIR,
                    libc::S_IFREG => VREG,
                    libc::S_IFLNK => VLNK,
                    _ => VNON,
                },
            },
        }
    }
}

fn modification_time(seconds: i64, nanos: i64) -> Option<SystemTime> {
    // Darwin can expose negative fractional nanoseconds. Normalize like std's Unix SystemTime,
    // including times between -1 and 0 seconds, before constructing an unsigned Duration.
    let (seconds, nanos) =
        if seconds <= 0 && seconds > i64::MIN && (-999_999_999..0).contains(&nanos) {
            (seconds - 1, nanos + 1_000_000_000)
        } else {
            (seconds, nanos)
        };
    let nanos = u32::try_from(nanos).ok().filter(|n| *n < 1_000_000_000)?;
    if seconds >= 0 {
        UNIX_EPOCH.checked_add(Duration::new(seconds.cast_unsigned(), nanos))
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::new(seconds.unsigned_abs(), 0))?
            .checked_add(Duration::new(0, nanos))
    }
}

pub(crate) struct RelativeReadDir {
    stream: DirectoryStream,
    directory: Arc<OwnedFd>,
    parent_path: Arc<Path>,
    depth: usize,
    strategy: MacosMetadataStrategy,
    buffered: std::vec::IntoIter<RelativeEntry>,
    error: Option<io::Error>,
    exhausted: bool,
}

impl RelativeReadDir {
    pub(crate) fn open(
        path: Arc<Path>,
        depth: usize,
        strategy: MacosMetadataStrategy,
    ) -> io::Result<Self> {
        let directory: OwnedFd = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY)
            .open(&path)?
            .into();
        // SAFETY: fcntl duplicates a live descriptor; CLOEXEC matches std's descriptor behavior.
        // Both descriptors refer to the same open directory, even if its pathname is renamed.
        let duplicate = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if duplicate < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fcntl returned a newly owned descriptor, guarded until fdopendir takes ownership.
        let duplicate = unsafe { OwnedFd::from_raw_fd(duplicate) };
        // SAFETY: this is an open directory descriptor, exclusively offered to fdopendir.
        let stream = NonNull::new(unsafe { libc::fdopendir(duplicate.as_raw_fd()) })
            .ok_or_else(io::Error::last_os_error)?;
        let _ = duplicate.into_raw_fd(); // fdopendir owns it on success, including on Drop.
        Ok(Self {
            stream: DirectoryStream(stream),
            directory: Arc::new(directory),
            parent_path: path,
            depth,
            strategy,
            buffered: Vec::new().into_iter(),
            error: None,
            exhausted: false,
        })
    }

    fn read_entry(&mut self) -> io::Result<Option<RelativeEntry>> {
        loop {
            // SAFETY: __error points to this thread's errno. Clearing it distinguishes EOF from
            // a failed readdir. This stream is exclusively borrowed until the name is copied.
            let record = unsafe {
                *libc::__error() = 0;
                libc::readdir(self.stream.0.as_ptr())
            };
            if record.is_null() {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return if error.raw_os_error() == Some(0) {
                    Ok(None)
                } else {
                    Err(error)
                };
            }
            // SAFETY: successful readdir returns a valid dirent until the next stream call.
            let record = unsafe { &*record };
            // SAFETY: readdir guarantees a NUL-terminated filename within d_name.
            let name = unsafe { CStr::from_ptr(record.d_name.as_ptr()) }.to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            let file_type = FileType {
                kind: match record.d_type {
                    libc::DT_DIR => VDIR,
                    libc::DT_REG => VREG,
                    libc::DT_LNK => VLNK,
                    _ => VNON,
                },
            };
            return Ok(Some(RelativeEntry {
                entry: Entry {
                    depth: self.depth,
                    file_name: OsString::from_vec(name.to_vec()),
                    file_type,
                    metadata: None,
                    parent_path: Arc::clone(&self.parent_path),
                    directory_id: None,
                    parent_directory_id: None,
                },
                directory: Arc::clone(&self.directory),
                inode: record.d_ino,
            }));
        }
    }

    fn refill(&mut self) {
        let mut entries = Vec::with_capacity(SORT_BUFFER_ENTRIES);
        while entries.len() < SORT_BUFFER_ENTRIES {
            match self.read_entry() {
                Ok(Some(entry)) => entries.push(entry),
                result => {
                    self.error = result.err();
                    self.exhausted = true;
                    break;
                }
            }
        }
        order_buffer(&mut entries, self.strategy);
        self.buffered = entries.into_iter();
    }
}

fn order_buffer(entries: &mut [RelativeEntry], strategy: MacosMetadataStrategy) {
    if strategy == MacosMetadataStrategy::InodeOrdered {
        // Stable ties preserve enumeration order for hard links sharing an inode ID.
        entries.sort_by_key(|entry| entry.inode);
    }
}

impl Iterator for RelativeReadDir {
    type Item = io::Result<RelativeEntry>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.buffered.len() == 0 && !self.exhausted {
            self.refill();
        }
        self.buffered
            .next()
            .map(Ok)
            .or_else(|| self.error.take().map(Err))
    }
}

#[cfg(test)]
mod tests;
