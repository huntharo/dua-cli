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

## Home-directory results (2026-09-26)

macOS 26.6.1 / APFS, release core telemetry example from standalone commit
`5c36949`, initial count 16, 250 ms reference/candidate windows, 80% target.
All five cases ran serially after local builds finished, with ordinary user
permissions and no cache purge. The table lists **individual observations in run
order**, not medians. Elapsed time includes process launch/shutdown; CPU is child
user + system time. These are raw core walks, not CLI aggregation timings.

| Run | Policy | Elapsed seconds | CPU seconds | Final workers | Search complete after |
| --- | --- | ---: | ---: | ---: | ---: |
| 1 | Throughput, initial 16 | 98.346 | 419.885 | 7 | 6.190 s |
| 2 | Fixed 8 | 83.442 | 398.876 | 8 | Fixed |
| 3 | Legacy, initial 16 | 78.839 | 700.417 | 14 | 1.017 s |
| 4 | Fixed 16 | 78.164 | 767.140 | 16 | Fixed |
| 5 | Throughput, initial 16 | 83.465 | 378.168 | 7 | 4.665 s |

Both throughput runs tested candidate counts **8, 4, 6, 7**, returning to 16 for
fresh reference windows between votes. Both finished at **7 workers**, with
retirement already acknowledged at the first search-complete observation. There
were **zero observed count changes after completion**: the chosen count held for
approximately 91.95 seconds and 78.80 seconds respectively, until traversal ended.
The admitted count was seven for 94.53% and 95.35% of the observed walk duration;
the time-weighted admitted means were 7.236 and 7.200. These means describe
admission targets, not instantaneous running threads: old jobs can overlap while
`retirement_settled=false`. Observations are made as entries arrive and can miss
short transitions; the controller's hold behavior is also covered by unit tests.

Relative to the one fixed-16 observation, the two throughput observations used
**45.3% and 50.7% less CPU**, with **25.8% and 6.8% more elapsed time**. The second
run's elapsed time was essentially the same as fixed eight. These measurements
support a CPU-efficiency tradeoff and stable completion of the search, not a
scan-latency improvement. The first throughput run retained approximately 79.4%
of fixed-16's whole-run entry rate, the second 93.6%; an 80% sampled target is not
an end-to-end throughput guarantee. Run order, caches, directory shape and live
changes are confounded here. The selected count is held for the remainder of a
walk; this intentionally prevents oscillation but does not respond to later
changes in the workload.

### Traversal totals and errors

| Run | Entries | Directories | Logical bytes | Errors |
| --- | ---: | ---: | ---: | ---: |
| 1 | 22,439,472 | 3,893,159 | 2,064,702,067,666 | 158 |
| 2 | 22,439,505 | 3,893,159 | 2,064,705,391,235 | 158 |
| 3 | 22,439,528 | 3,893,165 | 2,064,705,955,632 | 158 |
| 4 | 22,470,044 | 3,898,222 | 2,064,925,623,797 | 158 |
| 5 | 22,470,044 | 3,898,222 | 2,064,924,313,340 | 158 |

All processes exited successfully, but each walk reported 158 traversal/metadata
errors. The numeric harness does not identify error paths or categories, so equal
error counts do not establish identical failures or complete coverage. No
permissions were elevated. Logical byte totals describe metadata, not bytes read
from the device, and include individual entries without CLI hard-link deduplication.

The home directory was live: the first and last runs differ by 30,572 entries,
5,063 directories and 222,245,674 logical bytes. Runs four and five have matching
entry/directory/error counts, but differ by 1,310,457 logical bytes. This is not an
identical-snapshot benchmark, and it does not establish a universal optimal count.

[Raw numeric observations and executable SHA-256](artifacts/home-thread-tuning.json)
include every observed state, completion marker and CPU counter. The experiment
finished with exit status zero; no more scans were needed to assess oscillation.

## Validation

335 workspace tests with all features passed. Strict workspace/all-target/all-feature
Clippy, Linux musl and Windows MSVC core all-target checks, formatting and diff checks
passed. The CLI and telemetry example built in release mode. Tests cover threshold
boundaries, repeated votes, bounded refinement and permanent hold under subsequent
rate changes, plus retirement, restart, cancellation and output backpressure.
