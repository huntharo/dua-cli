use super::*;
use crate::{Order, walk};
use std::{
    collections::BTreeMap,
    os::unix::fs::{MetadataExt, symlink},
    time::Duration,
};

const STRATEGIES: [MacosMetadataStrategy; 4] = [
    MacosMetadataStrategy::Adaptive,
    MacosMetadataStrategy::Bulk,
    MacosMetadataStrategy::DirectoryLocal,
    MacosMetadataStrategy::InodeOrdered,
];

fn options(strategy: MacosMetadataStrategy) -> Options {
    Options {
        macos_metadata_strategy: strategy,
        ..Options::default()
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Observed {
    len: u64,
    allocated: u64,
    data_allocated: u64,
    dev: u64,
    ino: u64,
    links: u64,
    modified: SystemTime,
    file: bool,
    dir: bool,
    symlink: bool,
    depth: usize,
    clone_id: Option<std::num::NonZeroU64>,
}

fn observed(entry: &Entry) -> Observed {
    let metadata = entry.metadata.as_ref().unwrap().as_ref().unwrap();
    Observed {
        len: metadata.len(),
        allocated: metadata.allocated_size(),
        data_allocated: metadata.data_allocated_size(),
        dev: metadata.dev(),
        ino: metadata.ino(),
        links: metadata.nlink(),
        modified: metadata.modified().unwrap(),
        file: metadata.is_file(),
        dir: entry.file_type.is_dir(),
        symlink: entry.file_type.is_symlink(),
        depth: entry.depth,
        clone_id: metadata.clone_id(),
    }
}

#[test]
fn forced_strategies_match_legacy_and_stat_with_parent_order_and_pruning() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    fs::create_dir_all(path.join("parent/child")).unwrap();
    fs::create_dir(path.join("pruned")).unwrap();
    fs::write(path.join("pruned/hidden"), b"hidden").unwrap();
    fs::write(path.join("parent/child/unicode-λ-日本語"), vec![7; 9000]).unwrap();
    fs::hard_link(
        path.join("parent/child/unicode-λ-日本語"),
        path.join("hardlink"),
    )
    .unwrap();
    symlink("parent", path.join("dir-link")).unwrap();
    symlink("missing", path.join("dangling")).unwrap();
    let sparse = fs::File::create(path.join("sparse")).unwrap();
    sparse.set_len(1024 * 1024).unwrap();
    // Stat must correctly preserve a fractional pre-epoch modification timestamp.
    sparse
        .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH - Duration::from_millis(250)))
        .unwrap();
    for index in 0..130 {
        fs::write(path.join(format!("file-{index}")), b"small").unwrap();
    }
    let mut expected = None;
    for strategy in STRATEGIES {
        for order in [Order::ParentFirst, Order::Completion] {
            for threads in [1, 4] {
                let mut entries = BTreeMap::new();
                let mut ids = BTreeMap::new();
                for entry in walk(path, threads, order, options(strategy), |e| {
                    e.file_name != "pruned"
                }) {
                    let entry = entry.unwrap();
                    let name = entry.path();
                    if entry.depth > 0 && matches!(order, Order::ParentFirst) {
                        assert_eq!(
                            ids.get(name.parent().unwrap()),
                            Some(&entry.parent_directory_id.unwrap())
                        );
                    }
                    if let Some(id) = entry.directory_id {
                        assert!(ids.insert(name.clone(), id).is_none());
                    }
                    let data = observed(&entry);
                    let stat = fs::symlink_metadata(&name).unwrap();
                    assert_eq!(
                        (data.len, data.allocated, data.dev, data.ino, data.links),
                        (
                            stat.len(),
                            stat.blocks() * 512,
                            stat.dev(),
                            stat.ino(),
                            stat.nlink()
                        )
                    );
                    assert_eq!(data.modified, stat.modified().unwrap());
                    assert!(entries.insert(name, data).is_none(), "exactly once");
                }
                assert!(!entries.contains_key(&path.join("pruned/hidden")));
                assert!(!entries.contains_key(&path.join("dir-link/child")));
                if let Some(expected) = &expected {
                    assert_eq!(&entries, expected);
                } else {
                    expected = Some(entries);
                }
            }
        }
    }
    assert_eq!(
        Options::default().macos_metadata_strategy,
        MacosMetadataStrategy::Adaptive
    );
}

#[test]
fn direct_read_dir_and_clone_fallback_match_path_metadata() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("data"), vec![42; 8192]).unwrap();
    fs::copy(
        directory.path().join("data"),
        directory.path().join("clone"),
    )
    .unwrap();
    fs::hard_link(
        directory.path().join("data"),
        directory.path().join("hard-link"),
    )
    .unwrap();
    fs::write(directory.path().join("clone/..namedfork/rsrc"), [7; 8192]).unwrap();
    fs::create_dir(directory.path().join("child")).unwrap();
    symlink("missing", directory.path().join("link")).unwrap();
    for strategy in STRATEGIES {
        let options = Options {
            apfs_clone_metadata: true,
            ..options(strategy)
        };
        for entry in crate::read_dir(directory.path(), options).unwrap() {
            let entry = entry.unwrap();
            let expected = Entry::from_path(&entry.path(), options).unwrap();
            assert_eq!(observed(&entry), observed(&expected));
        }
        for entry in crate::read_dir(directory.path(), options.skip_metadata()).unwrap() {
            assert!(entry.unwrap().metadata.is_none());
        }
    }
}

#[test]
fn relative_jobs_retain_directory_after_rename_and_reader_drop() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("original");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("file"), b"original").unwrap();
    for strategy in [
        MacosMetadataStrategy::DirectoryLocal,
        MacosMetadataStrategy::InodeOrdered,
    ] {
        let mut reader = RelativeReadDir::open(Arc::from(source.as_path()), 1, strategy).unwrap();
        let descriptor = Arc::downgrade(&reader.directory);
        let entry = reader.next().unwrap().unwrap();
        fs::rename(&source, directory.path().join("moved")).unwrap();
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file"), b"replacement with different length").unwrap();
        drop(reader);
        assert!(descriptor.upgrade().is_some());
        let entry = entry.read_metadata(options(strategy));
        assert_eq!(entry.metadata.unwrap().unwrap().len(), 8);
        assert!(
            descriptor.upgrade().is_none(),
            "last stat releases directory"
        );
        fs::remove_dir_all(&source).unwrap();
        fs::rename(directory.path().join("moved"), &source).unwrap();
    }
}

#[test]
fn metadata_errors_remain_attached_to_entries_and_listing_errors_are_outer() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("vanishes");
    fs::write(&file, b"gone").unwrap();
    let mut reader = RelativeReadDir::open(
        Arc::from(directory.path()),
        1,
        MacosMetadataStrategy::DirectoryLocal,
    )
    .unwrap();
    let entry = reader.next().unwrap().unwrap();
    fs::remove_file(&file).unwrap();
    let entry = entry.read_metadata(Options::default());
    assert!(entry.file_type.is_file());
    assert_eq!(
        entry.metadata.unwrap().err().unwrap().kind(),
        io::ErrorKind::NotFound
    );
    // Injectable terminal listing error proves the iterator yields it once after buffered entries.
    reader.error = Some(io::Error::from_raw_os_error(libc::EIO));
    reader.exhausted = true;
    assert_eq!(
        reader.next().unwrap().err().unwrap().raw_os_error(),
        Some(libc::EIO)
    );
    assert!(reader.next().is_none());
    fs::write(&file, b"file").unwrap();
    assert_eq!(
        RelativeReadDir::open(Arc::from(file), 1, MacosMetadataStrategy::DirectoryLocal)
            .err()
            .unwrap()
            .raw_os_error(),
        Some(libc::ENOTDIR)
    );
}

#[test]
fn buffers_are_bounded_stably_ordered_and_exactly_once() {
    let directory = tempfile::tempdir().unwrap();
    for index in 0..SORT_BUFFER_ENTRIES + 17 {
        fs::write(directory.path().join(format!("entry-{index}")), b"").unwrap();
    }
    let local: Vec<_> = RelativeReadDir::open(
        Arc::from(directory.path()),
        1,
        MacosMetadataStrategy::DirectoryLocal,
    )
    .unwrap()
    .map(|e| e.unwrap())
    .collect();
    let mut expected = Vec::new();
    for chunk in local.chunks(SORT_BUFFER_ENTRIES) {
        let mut keys: Vec<_> = chunk
            .iter()
            .map(|e| (e.inode, e.entry.file_name.clone()))
            .collect();
        keys.sort_by_key(|e| e.0);
        expected.extend(keys);
    }
    let mut reader = RelativeReadDir::open(
        Arc::from(directory.path()),
        1,
        MacosMetadataStrategy::InodeOrdered,
    )
    .unwrap();
    let mut actual = Vec::new();
    while let Some(entry) = reader.next() {
        assert!(reader.buffered.len() < SORT_BUFFER_ENTRIES);
        let entry = entry.unwrap();
        actual.push((entry.inode, entry.entry.file_name));
    }
    assert_eq!(actual, expected);
    assert_eq!(actual.len(), SORT_BUFFER_ENTRIES + 17);
    let mut names: Vec<_> = actual.iter().map(|e| &e.1).collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), actual.len());
    let mut tied: Vec<_> = local.into_iter().take(4).collect();
    for (entry, inode) in tied.iter_mut().zip([2, 1, 2, 1]) {
        entry.inode = inode;
    }
    let names: Vec<_> = tied.iter().map(|e| e.entry.file_name.clone()).collect();
    order_buffer(&mut tied, MacosMetadataStrategy::DirectoryLocal);
    assert_eq!(
        tied.iter().map(|e| &e.entry.file_name).collect::<Vec<_>>(),
        names.iter().collect::<Vec<_>>()
    );
    order_buffer(&mut tied, MacosMetadataStrategy::InodeOrdered);
    assert_eq!(
        tied.iter().map(|e| &e.entry.file_name).collect::<Vec<_>>(),
        vec![&names[1], &names[3], &names[0], &names[2]]
    );

    for strategy in [
        MacosMetadataStrategy::DirectoryLocal,
        MacosMetadataStrategy::InodeOrdered,
    ] {
        // Exercise the real scheduling guard with no consumers on the local metadata queue.
        let pool = crate::start_pool(1, Order::ParentFirst, options(strategy), 1);
        let root = Arc::new(crate::Root {
            index: 7,
            pending: crate::AtomicUsize::new(1),
            descend: Arc::new(|_, _| true),
        });
        let worker = crate::Worker::new_lifo();
        crate::read_dir_relative(
            &root,
            Arc::from(directory.path()),
            0,
            1,
            &worker,
            &pool.shared,
        );
        assert_eq!(worker.len(), crate::MAX_QUEUED_STAT_JOBS);
        let crate::Event::Batch { batch, .. } = pool.events.try_recv().unwrap() else {
            panic!("inline batch");
        };
        assert_eq!(batch.unwrap().len(), 17);
        while let Some(job) = worker.pop() {
            let crate::Job::StatRelative { entries, .. } = job else {
                panic!("relative metadata job");
            };
            assert_eq!(entries.len(), RELATIVE_STAT_CHUNK_SIZE);
        }
        // The full worker pipeline must retain every entry across the buffer boundary.
        let names = walk(
            directory.path(),
            4,
            Order::ParentFirst,
            options(strategy),
            |_| true,
        )
        .map(|e| e.unwrap().path())
        .collect::<Vec<_>>();
        let unique = names.iter().collect::<std::collections::HashSet<_>>();
        assert_eq!(names.len(), SORT_BUFFER_ENTRIES + 18); // Includes the root.
        assert_eq!(unique.len(), names.len());
    }

    // Dropping an iterator with queued fd-owning jobs and a full output channel must join workers.
    for strategy in STRATEGIES {
        let path = directory.path().to_owned();
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let mut walker = walk(&path, 4, Order::ParentFirst, options(strategy), |_| true);
            for _ in 0..3 {
                walker.next().unwrap().unwrap();
            }
            drop(walker);
            let mut walker = walk(&path, 4, Order::ParentFirst, options(strategy), |_| true);
            for _ in 0..3 {
                walker.next().unwrap().unwrap();
            }
            assert!(
                walker
                    .next_cancellable(&crate::AtomicBool::new(true))
                    .is_none()
            );
            assert!(walker.next().is_none());
            drop(walker);
            tx.send(()).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(10))
            .expect("cancellation must finish");
        handle.join().unwrap();
    }
}

#[test]
fn negative_fractional_timestamps_are_normalized_without_unsigned_wrap() {
    for (seconds, nanos, expected) in [
        (-1, 750_000_000, UNIX_EPOCH - Duration::from_millis(250)),
        (0, -250_000_000, UNIX_EPOCH - Duration::from_millis(250)),
        (-1, -250_000_000, UNIX_EPOCH - Duration::from_millis(1250)),
        (1, 250_000_000, UNIX_EPOCH + Duration::from_millis(1250)),
    ] {
        assert_eq!(modification_time(seconds, nanos), Some(expected));
    }
    assert!(modification_time(1, -1).is_none());
    assert!(modification_time(0, 1_000_000_000).is_none());
}

#[test]
fn readable_directory_preserves_search_permission_errors_for_each_strategy() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let restricted = directory.path().join("restricted");
    fs::create_dir(&restricted).unwrap();
    fs::write(restricted.join("file"), b"content").unwrap();
    fs::set_permissions(&restricted, fs::Permissions::from_mode(0o400)).unwrap();
    let results: Vec<_> = STRATEGIES
        .into_iter()
        .map(|strategy| {
            crate::read_dir(&restricted, options(strategy))
                .unwrap()
                .collect::<Vec<_>>()
        })
        .collect();
    fs::set_permissions(&restricted, fs::Permissions::from_mode(0o700)).unwrap();
    for entries in results {
        assert_eq!(entries.len(), 1);
        let entry = entries.into_iter().next().unwrap().unwrap();
        assert_eq!(entry.file_name, "file");
        assert!(entry.file_type.is_file());
        assert_eq!(
            entry.metadata.unwrap().err().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
    }
}
