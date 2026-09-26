# Throughput-retention worker tuning

The previous CLI controller tested 16 -> 15 and permanently held 16 after its first
rejected sample. It could not reach the much cheaper eight-worker configuration on
the reference tree. The CLI now defaults to a coarse throughput-retention search:
measure the initial count once, halve, then refine the first failing bracket.
Two passing windows out of at most three are required per count; repeated votes
stay at that count. The same initial reference is retained, so the 80% allowance
does not compound. No additional high-count reference measurements are performed.

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
restart. A failure advances directly to the midpoint between rejected and accepted
counts. Starting at 16, a return to 16 requires rejecting 8, 12, 14, then 15;
there is no direct reset from a lower candidate. Three inconclusive windows reject
a candidate and refine the bracket in the same way. Empty initial windows wait
for work; a repeatedly backpressured initial reference retains the initial count
without starting a search. Synthetic tests cover both valid and inconclusive
samples and verify that arbitrary rate changes cannot restart a completed search.
The one initial reference can become stale as directory composition changes.

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

## Latest fixed-16 comparison: performance failure

After the operator reported removing approximately 650 GB and millions of files,
the same frozen `c1e072a` executable ran **fixed 16 / tuner / tuner / fixed 16**
over the home directory. No builds overlapped these scans; no cache purge,
privilege escalation or production changes occurred. These are raw core timings,
not CLI aggregation. All four cases completed.

| Run | Policy | Elapsed seconds | CPU seconds | Final workers | Search complete after |
| --- | --- | ---: | ---: | ---: | ---: |
| 1 | Fixed 16 | 73.891 | 763.895 | 16 | Fixed |
| 2 | Revised tuner | 106.871 | 262.209 | 4 | 2.551 s |
| 3 | Revised tuner | 398.597 | 255.872 | 1 | 6.795 s |
| 4 | Fixed 16 | 96.688 | 792.343 | 16 | Fixed |

Two-run means were **85.289 s elapsed / 778.119 CPU s** for fixed 16, versus
**252.734 s / 259.040 CPU s** for the tuner. The 106.9–398.6 s tuner range is
material; the mean must not hide the one-worker failure. Stable admission does
not establish a useful choice: there were no changes after search completion,
but the one-worker run took approximately 4.1–5.4 times the fixed-16 durations.
This result invalidates treating the current tuner as a reliably measured
performance improvement. PR #5 is draft pending correction and validation.

The sequences were `16 → 8 → 4 → 2 → 3 → 4` and `16 → 8 → 4 → 2 → 1`.
The first completed-search observation at four workers was unsettled; both final
summaries were settled. No repeated initial-count reference resets occurred.

### Failure investigation

At the first observed reduction in the slow run, only 9,489 entries had arrived
in 504 ms; in the other tuner run, 66,026 entries had arrived in 253 ms. These are
consumer observations, **not the controller's exact measurement windows**. The
existing telemetry does not record the controller's private rate or vote decisions.

Code inspection confirms that the controller retains its first valid reference
rate unchanged, accepts subsequent candidates against that reference, then holds
its choice without ongoing performance validation. An unusually low startup rate
can therefore make much lower counts pass, even if later work could benefit from
more workers. The trace is consistent with this failure mode; it does not isolate
whether startup I/O, metadata-path selection, scheduling or workload shape produced
the initial low rate. A representative reference and detection of a bad low-count
choice remain unresolved. Any correction must preserve the requested midpoint-only
upward search and must not restore repeated resets to 16.

### Observed tree and errors

| Run | Entries | Directories | Logical bytes | Errors |
| --- | ---: | ---: | ---: | ---: |
| 1 | 22,478,838 | 3,898,420 | 2,066,382,212,822 | 158 |
| 2 | 22,478,838 | 3,898,420 | 2,066,382,930,696 | 158 |
| 3 | 22,479,603 | 3,898,544 | 2,066,526,183,549 | 158 |
| 4 | 22,498,831 | 3,900,703 | 2,067,747,530,741 | 158 |

These totals remain around 22.5 million entries and 2.067 trillion logical bytes;
they do **not** show the expected large decrease from the reported deletion. The
reason has not been established. Do not infer that the deletion did not occur,
or that logical bytes are physical disk use. This harness visits accessible paths
under the home directory, does not follow symlinks, and counts individual entry
metadata without CLI hard-link deduplication.

All cases reported 158 unclassified traversal/metadata errors. First-to-last totals
changed by 19,993 entries, 2,283 directories and 1,365,317,919 logical bytes. Equal
error totals and the first pair's matching entry counts do not prove identical work.
Only the fixed and tuned results in this batch are used for the comparison above.

[Full numeric observations and executable hash](artifacts/current-tree-fixed16-comparison.json)
are retained. The Python harness wrote all four rows and its success marker.
The monitor's shell wrapper subsequently failed by assigning a reserved read-only
variable; its exit file was reconstructed from the harness success marker, not
captured directly from the wrapper. That bookkeeping error did not interrupt the
already-completed scans.

## Earlier single-reference comparison (2026-09-26)

The revised controller (`c1e072a`) and previous repeated-reference controller
(`5c36949`) were compared using frozen release binaries in **new/old/old/new**
order. Both used initial 16, 250 ms windows and an 80% target. The full home
directory was traversed serially with no overlapping local builds, no cache purge
and ordinary permissions. These are raw core measurements, not CLI aggregation.

| Run | Controller | Elapsed seconds | CPU seconds | Final workers | Search complete after |
| --- | --- | ---: | ---: | ---: | ---: |
| 1 | New, single reference | 102.320 | 308.005 | 5 | 2.360 s |
| 2 | Old, repeated reference | 97.285 | 457.825 | 7 | 5.171 s |
| 3 | Old, repeated reference | 86.393 | 449.129 | 8 | 5.091 s |
| 4 | New, single reference | 110.212 | 272.733 | 4 | 2.555 s |

The new count-change sequences were exactly:

- Run 1: **16 → 8 → 4 → 6 → 5**, then hold.
- Run 4: **16 → 8 → 4 → 2 → 3 → 4**, then hold.

Neither returned to 16 after its first reduction. Both showed **zero observed
count changes after tuning completion**, holding their final count through the
remaining traversal. Run 1 was settled when completion was observed. In run 4,
completion first appeared with `retirement_settled=false`; the next entry in the
same 2,555 ms timestamp showed retirement settled. Thus the first completed-search
observation alone is not evidence that all old workers have parked.

Across the **two observations per controller**, arithmetic means were:

| Controller | Elapsed seconds | CPU seconds | Search completion seconds |
| --- | ---: | ---: | ---: |
| Old | 91.839 | 453.477 | 5.131 |
| New | 106.266 | 290.369 | 2.458 |

The new search completed **52.1% sooner** and used **36.0% less total CPU**, but
complete traversal took **15.7% longer**. This is a CPU-efficiency tradeoff, not an
end-to-end speedup. The new controller chose four/five workers instead of seven/eight;
therefore the CPU difference cannot be attributed solely to removing a few seconds
of reference probes. Holding lower concurrency for the rest of the walk contributes
to both lower CPU consumption and longer elapsed time. Different chosen counts
also show sensitivity to the sampled workload. No optimal-count claim is made.

| Run | Entries | Directories | Logical bytes | Errors |
| --- | ---: | ---: | ---: | ---: |
| 1 | 22,478,128 | 3,898,332 | 2,066,306,479,732 | 158 |
| 2 | 22,478,130 | 3,898,332 | 2,066,307,765,637 | 158 |
| 3 | 22,478,143 | 3,898,333 | 2,066,308,605,367 | 158 |
| 4 | 22,478,146 | 3,898,333 | 2,066,309,509,700 | 158 |

All four processes exited successfully. All reported 158 traversal/metadata errors,
which this numeric harness does not classify. Equal error counts do not prove
identical failures. First-to-last totals differ by 18 entries, one directory and
3,029,968 logical bytes; the home directory remained live. This small sample and
run ordering do not isolate cache/background activity or establish identical work.
Logical bytes are metadata totals, not physical I/O volume.

[Complete numeric telemetry and both binary hashes](artifacts/single-reference-home-tuning.json)
are retained. Reproduce this ABBA comparison with separately built/frozen executables:

```sh
python3 etc/profiling/compare_home_tuning.py --allow-home \
  --binary /tmp/new-thread-probe --reference-binary /tmp/old-thread-probe \
  --output /tmp/dua-home-abba
```

The historical fixed-count table below was measured in a different batch and must
not be substituted for contemporaneous fixed-count controls in this comparison.

## Historical repeated-reference results (2026-09-26)

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

The revised standalone build passed 337 workspace tests with all features. Strict
workspace/all-target/all-feature Clippy, Linux musl and Windows MSVC core all-target
checks, formatting and diff checks passed. The telemetry example built in release
mode. Tests cover in-place votes, the exact midpoint-only return path for both
failed and inconclusive samples, non-compounding thresholds, bounded refinement,
permanent hold, retirement, restart, cancellation and output backpressure.
The stacked APFS branch also passed workspace tests and strict Clippy after integration.
