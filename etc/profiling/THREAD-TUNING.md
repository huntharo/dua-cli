# Throughput-retention worker tuning

The previous CLI controller tested 16 -> 15 and permanently held 16 after its first
rejected sample. It could not reach the much cheaper eight-worker configuration on
the reference tree. The CLI now defaults to a coarse throughput-retention search:
halve, repeat adjacent reference/candidate comparisons, then refine the first
failing bracket. Two passing votes out of at most three are required per count.
Every pair refreshes the initial-count reference; the 80% allowance does not compound.

`--thread-throughput-percent 80` sets the new target. Explicit
`--thread-loss-percent 20` selects the unchanged legacy formula; `--fixed-threads`
disables both. Core `Options` retains fixed concurrency by default. Core consumers
can opt in with `throughput_threads: Some(dua_core::ThroughputThreads::default())`.
This new field takes precedence over `adaptive_threads`; existing walk signatures
and the legacy `AdaptiveThreads` configuration remain unchanged. Exhaustive
`Options` literals need the new field or `..Default::default()` on every platform.

## Long-walk validation

The standalone branch retains the existing filesystem enumeration implementation.
It contains no APFS metadata-strategy or inode-ordering experiment. The earlier
short `~/github` measurements were collected before separating the branches and
are not a substitute for measuring this standalone build on a longer traversal.

`Walk::thread_tuning_complete()` and `RootWalk::thread_tuning_complete()` distinguish
finished search from `threads_settled()` (acknowledged worker retirement). A final
count can be chosen before all retiring jobs finish. These getters are separate
concurrent observations, not an atomic snapshot.

The controller performs a bounded search and then holds its selection until a
restart. It deliberately returns to the initial count between probe windows.
Those temporary reference comparisons are expected; continuing changes after
search completion would be a defect. Synthetic tests verify that arbitrary rate
changes after completion cannot restart the search.

The telemetry example remains restricted to `~/github` unless `--allow-home` is
explicitly supplied. It prints numeric counts and thread states, without filenames.
A heartbeat is emitted once per second as entries arrive; blocked iterator calls
can delay observation. The final summary records entry/directory/error totals and
logical bytes. This measures raw core traversal, not CLI aggregation.

To run the authorized home-directory comparison after all builds finish:

```sh
cargo build --release -p dua-core --example thread_probe
python3 etc/profiling/compare_home_tuning.py --allow-home --output /tmp/dua-home-tuning
```

The script serially runs throughput 16, fixed 8, legacy 16, fixed 16, throughput 16.
It records cumulative child CPU time, elapsed time, all observations, admission
changes, and the first search-complete observation. Results are saved after each
case. A live home directory can change during the comparison; inaccessible paths
are counted without elevated privileges. Count/error differences must be reported
before interpreting performance differences. No caches are purged.

Home-directory measurements are pending.
