use super::*;
use std::collections::{BTreeSet, HashMap, HashSet};

fn scheduler_fixture(threads: usize) -> (PoolShared, Receiver<Event>) {
    let parkers = (0..threads).map(|_| Parker::new()).collect::<Vec<_>>();
    let (events, receiver) = sync_channel(threads * 2);
    (
        PoolShared {
            injector: Injector::new(),
            work: Injector::new(),
            work_batches: AtomicUsize::new(0),
            idle_count: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
            events,
            active_roots: AtomicUsize::new(1),
            order: Order::ParentFirst,
            options: Options::default(),
            unparkers: parkers
                .iter()
                .map(|parker| parker.unparker().clone())
                .collect(),
            idle: (0..threads).map(|_| AtomicBool::new(false)).collect(),
            next_wake: AtomicUsize::new(0),
            next_directory_id: AtomicUsize::new(0),
        },
        receiver,
    )
}

fn queued_jobs(count: usize) -> (LocalQueue, Arc<Root>) {
    let worker = LocalQueue::new();
    let root = Arc::new(Root {
        index: 7,
        pending: AtomicUsize::new(count),
        descend: Arc::new(|_, _| true),
    });
    for directory_id in 0..count {
        worker.push(Job::ReadDir {
            root: Arc::clone(&root),
            path: Arc::from(Path::new("unused")),
            directory_id,
            entry_depth: 1,
        });
    }
    (worker, root)
}

fn directory_job_id(job: Job) -> usize {
    let Job::ReadDir { directory_id, .. } = job else {
        panic!("the fixture only queues directory jobs");
    };
    directory_id
}

fn shared_chunk(shared: &PoolShared) -> Vec<usize> {
    let Steal::Success(chunk) = shared.work.steal() else {
        panic!("expected a shared work chunk");
    };
    shared.work_batches.fetch_sub(1, AtomicOrdering::Relaxed);
    chunk.into_iter().map(directory_job_id).collect()
}

#[test]
fn local_work_is_shared_in_bounded_deterministic_chunks() {
    let (shared, _receiver) = scheduler_fixture(4);
    let count = WORK_CHUNK_SIZE * 3 + 7;
    let (worker, root) = queued_jobs(count);
    worker.share_work(&shared, worker.last_share.get());

    assert_eq!(worker.len(), WORK_CHUNK_SIZE);
    assert_eq!(shared.work_batches.load(AtomicOrdering::Relaxed), 3);
    assert_eq!(shared_chunk(&shared), (0..100).collect::<Vec<_>>());
    assert_eq!(shared_chunk(&shared), (100..200).collect::<Vec<_>>());
    assert_eq!(shared_chunk(&shared), (200..207).collect::<Vec<_>>());
    let retained = std::iter::from_fn(|| worker.pop())
        .map(directory_job_id)
        .collect::<Vec<_>>();
    assert_eq!(retained, (207..307).rev().collect::<Vec<_>>());
    assert_eq!(root.pending.load(AtomicOrdering::Relaxed), count);
}

#[test]
fn unfinished_work_returns_half_only_after_the_share_interval() {
    let (shared, _receiver) = scheduler_fixture(4);
    // Model a standing reserve owned by unrelated work so this isolates timed sharing.
    let reserve = shared.unparkers.len();
    shared.work_batches.store(reserve, AtomicOrdering::Relaxed);
    let (worker, root) = queued_jobs(101);
    let started = worker.last_share.get();
    worker.share_work(
        &shared,
        (started + WORK_SHARE_INTERVAL)
            .checked_sub(Duration::from_nanos(1))
            .unwrap(),
    );
    assert_eq!(worker.len(), 101);
    assert_eq!(shared.work_batches.load(AtomicOrdering::Relaxed), reserve);

    worker.share_work(&shared, started + WORK_SHARE_INTERVAL);
    assert_eq!(worker.len(), 50);
    assert_eq!(shared_chunk(&shared), (0..51).collect::<Vec<_>>());
    worker.share_work(
        &shared,
        (started + WORK_SHARE_INTERVAL * 2)
            .checked_sub(Duration::from_nanos(1))
            .unwrap(),
    );
    assert_eq!(worker.len(), 50);
    worker.share_work(&shared, started + WORK_SHARE_INTERVAL * 2);
    assert_eq!(worker.len(), 25);
    assert_eq!(shared_chunk(&shared), (51..76).collect::<Vec<_>>());
    assert_eq!(root.pending.load(AtomicOrdering::Relaxed), 101);
}

#[test]
fn a_shared_reserve_is_published_before_peers_announce_idleness() {
    let (shared, _receiver) = scheduler_fixture(4);
    let (worker, root) = queued_jobs(7);
    assert_eq!(shared.idle_count.load(AtomicOrdering::Relaxed), 0);

    worker.share_work(&shared, worker.last_share.get());

    assert_eq!(worker.len(), 3);
    assert_eq!(shared.work_batches.load(AtomicOrdering::Relaxed), 1);
    assert_eq!(shared_chunk(&shared), [0, 1, 2, 3]);
    assert_eq!(root.pending.load(AtomicOrdering::Relaxed), 7);
}

#[test]
fn idle_workers_receive_small_initial_chunks_without_waiting() {
    let (shared, _receiver) = scheduler_fixture(4);
    shared.idle[1].store(true, AtomicOrdering::Relaxed);
    shared.idle_count.store(1, AtomicOrdering::Relaxed);
    let (worker, root) = queued_jobs(7);
    worker.share_work(&shared, worker.last_share.get());

    assert_eq!(worker.len(), 3);
    assert_eq!(shared_chunk(&shared), [0, 1, 2, 3]);
    assert!(
        !shared.idle[1].load(AtomicOrdering::Relaxed),
        "publishing the initial chunk must claim and wake an idle worker"
    );
    assert_eq!(root.pending.load(AtomicOrdering::Relaxed), 7);
}

#[test]
fn batches_without_new_children_still_share_existing_private_work() {
    for order in [Order::Completion, Order::ParentFirst] {
        let (mut shared, receiver) = scheduler_fixture(2);
        shared.order = order;
        shared.idle[1].store(true, AtomicOrdering::Relaxed);
        shared.idle_count.store(1, AtomicOrdering::Relaxed);
        let (worker, root) = queued_jobs(7);

        assert!(publish_directory(
            &root,
            Ok(Vec::new()),
            Vec::new(),
            &worker,
            &shared,
        ));

        assert_eq!(worker.len(), 3);
        assert_eq!(shared_chunk(&shared), [0, 1, 2, 3]);
        assert_eq!(root.pending.load(AtomicOrdering::Relaxed), 7);
        let Event::Batch { root_idx, batch } = receiver.try_recv().unwrap() else {
            panic!("sharing existing work must preserve the result batch");
        };
        assert_eq!(root_idx, 7);
        assert!(batch.unwrap().is_empty());
        assert!(receiver.try_recv().is_err());
    }
}

#[test]
fn full_shared_queue_keeps_remaining_work_local_until_capacity_returns() {
    let (shared, _receiver) = scheduler_fixture(2);
    let (worker, root) = queued_jobs(701);
    let started = worker.last_share.get();
    worker.share_work(&shared, started);
    assert_eq!(shared.work_batches.load(AtomicOrdering::Relaxed), 4);
    assert_eq!(worker.len(), 301);
    worker.share_work(&shared, started + WORK_SHARE_INTERVAL);
    assert_eq!(worker.len(), 301);

    let mut ids = shared_chunk(&shared);
    worker.share_work(&shared, started + WORK_SHARE_INTERVAL);
    assert_eq!(shared.work_batches.load(AtomicOrdering::Relaxed), 4);
    assert_eq!(worker.len(), 201);
    for _ in 0..4 {
        ids.extend(shared_chunk(&shared));
    }
    ids.extend(std::iter::from_fn(|| worker.pop()).map(directory_job_id));
    ids.sort_unstable();
    assert_eq!(ids, (0..701).collect::<Vec<_>>());
    assert_eq!(root.pending.load(AtomicOrdering::Relaxed), 701);
}

#[test]
fn suspending_worker_releases_all_jobs_even_when_the_shared_queue_is_full() {
    let (shared, _receiver) = scheduler_fixture(2);
    let (worker, root) = queued_jobs(701);
    worker.share_work(&shared, worker.last_share.get());
    assert_eq!(shared.work_batches.load(AtomicOrdering::Relaxed), 4);
    assert_eq!(worker.len(), 301);

    worker.release_all(&shared);
    assert_eq!(worker.len(), 0);
    assert_eq!(shared.work_batches.load(AtomicOrdering::Relaxed), 8);
    assert_eq!(root.pending.load(AtomicOrdering::Relaxed), 701);
    worker.release_all(&shared);
    assert_eq!(shared.work_batches.load(AtomicOrdering::Relaxed), 8);

    // Exercise the actual claim path used by an active worker after a peer suspends.
    let active = LocalQueue::new();
    let mut ids = Vec::new();
    while let Some(job) = find_job(&active, &shared) {
        assert!(active.len() < WORK_CHUNK_SIZE);
        ids.push(directory_job_id(job));
    }
    ids.sort_unstable();
    assert_eq!(ids, (0..701).collect::<Vec<_>>());
    assert_eq!(shared.work_batches.load(AtomicOrdering::Relaxed), 0);
    assert_eq!(root.pending.load(AtomicOrdering::Relaxed), 701);
}

#[test]
fn chunky_walk_visits_wide_and_deep_trees_exactly_once() {
    let directory = tempfile::tempdir().unwrap();
    let mut expected = BTreeSet::from([PathBuf::new()]);
    // More than two normal claims, with new jobs produced while those claims run.
    for index in 0..217 {
        let parent = PathBuf::from(format!("branch-{index:03}"));
        let child = parent.join("child");
        let file = child.join("file");
        fs::create_dir_all(directory.path().join(&child)).unwrap();
        fs::write(directory.path().join(&file), b"entry").unwrap();
        expected.extend([parent, child, file]);
    }
    let mut deep = PathBuf::new();
    for _ in 0..19 {
        deep.push("deep");
        fs::create_dir(directory.path().join(&deep)).unwrap();
        expected.insert(deep.clone());
    }

    for threads in [1, 4] {
        for order in [Order::Completion, Order::ParentFirst] {
            for options in [Options::default(), Options::default().skip_metadata()] {
                let mut positions = HashMap::new();
                let mut directory_positions = HashMap::new();
                for (position, entry) in
                    walk(directory.path(), threads, order, options, |_| true).enumerate()
                {
                    let entry = entry.unwrap();
                    let relative = entry
                        .path()
                        .strip_prefix(directory.path())
                        .unwrap()
                        .to_owned();
                    assert!(
                        positions.insert(relative.clone(), position).is_none(),
                        "duplicate entry {relative:?} with {threads} threads"
                    );
                    assert_eq!(entry.depth, relative.components().count());
                    if let Some(id) = entry.directory_id {
                        assert!(directory_positions.insert(id, position).is_none());
                    }
                    if matches!(order, Order::ParentFirst)
                        && let Some(parent) = entry.parent_directory_id
                    {
                        assert!(
                            directory_positions[&parent] < position,
                            "parent must precede {relative:?}"
                        );
                    }
                }
                assert_eq!(
                    positions.into_keys().collect::<BTreeSet<_>>(),
                    expected,
                    "every entry must arrive with {threads} threads and {options:?}"
                );
            }
        }
    }
}

#[test]
fn chunky_roots_preserve_predicates_errors_and_completion() {
    let directory = tempfile::tempdir().unwrap();
    for side in ["left", "right"] {
        for index in 0..117 {
            let branch = directory.path().join(side).join(index.to_string());
            fs::create_dir_all(&branch).unwrap();
            fs::write(branch.join("file"), b"entry").unwrap();
        }
    }
    let file = directory.path().join("root-file");
    fs::write(&file, b"root").unwrap();
    let missing = directory.path().join("missing");

    for order in [Order::Completion, Order::ParentFirst] {
        let roots = [
            (11, directory.path().to_owned()),
            (22, directory.path().to_owned()),
            (33, missing.clone()),
            (44, file.clone()),
            (55, directory.path().to_owned()),
        ];
        let mut paths = HashMap::<usize, BTreeSet<PathBuf>>::new();
        let mut finished = HashSet::new();
        let mut errors = Vec::new();
        for (root_index, event) in walk_roots(
            roots,
            4,
            order,
            Options::default(),
            |root_index, entry| match root_index {
                11 => entry.file_name != "right",
                22 => entry.file_name != "left",
                55 => false,
                _ => true,
            },
        ) {
            assert!(
                !finished.contains(&root_index),
                "a root cannot publish entries or finish again after finishing"
            );
            match event {
                RootEvent::Entry(Ok(entry)) => {
                    let path = entry.path();
                    assert!(paths.entry(root_index).or_default().insert(path));
                }
                RootEvent::Entry(Err(error)) => {
                    errors.push((root_index, error.kind()));
                }
                RootEvent::Finished => {
                    assert!(finished.insert(root_index));
                }
            }
        }

        assert_eq!(finished, HashSet::from([11, 22, 33, 44, 55]));
        assert_eq!(errors, [(33, io::ErrorKind::NotFound)]);
        assert_eq!(paths[&44], BTreeSet::from([file.clone()]));
        assert_eq!(paths[&55], BTreeSet::from([directory.path().to_owned()]));
        for (root_index, accepted) in [(11, "left"), (22, "right")] {
            let mut expected = BTreeSet::from([
                directory.path().to_owned(),
                directory.path().join("left"),
                directory.path().join("right"),
                file.clone(),
            ]);
            for index in 0..117 {
                let branch = directory.path().join(accepted).join(index.to_string());
                expected.insert(branch.join("file"));
                expected.insert(branch);
            }
            assert_eq!(paths[&root_index], expected);
        }
    }
}
