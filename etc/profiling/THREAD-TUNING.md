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

## End-to-end CLI measurements

macOS 26.6.1 (25G76), APFS, release binary, serial execution, three observations per
configuration in forward/reverse/forward order. No concurrent builds were scheduled.
CPU is summed child-process user + system time, including startup and shutdown;
wall time includes process launch. The existing Adaptive metadata strategy was used
throughout. No whole-disk traversal, cache purge, raw-volume access or privilege
escalation was used.

All nine `~/github` runs exited successfully, emitted no stderr, and produced
**byte-identical aggregate output**. Timings below are medians:

| Configuration | Wall seconds | CPU seconds |
| --- | ---: | ---: |
| Legacy, initial 16 | 2.740 | 21.229 |
| New default, initial 16 | 2.763 | 16.986 |
| Fixed 8 control | 2.617 | 10.993 |

The new default reduced median CPU by **20.0%**, with **0.8%** greater wall time.
The fixed-eight control remains cheaper because it pays no online search cost.
These short walks do not establish the eventual chosen count or a full-disk speedup.
Raw timings, executable SHA-256 and matching output hashes are in
[throughput-cli-github.json](artifacts/throughput-cli-github.json).

A separate nine-run comparison on the stable `~/github/codex/codex-rs` subtree also
produced byte-identical output. Legacy initial-16 medians were 4.398 seconds wall /
7.476 CPU; the new default measured 4.518 / 6.478 (**13.4% less CPU**, **2.7% more wall**).
Fixed eight measured 1.826 / 8.399. These differences show that the existing per-directory
bulk-vs-parallel metadata probe and directory shape affect latency substantially;
worker count alone does not determine the result. See
[throughput-cli-codex.json](artifacts/throughput-cli-codex.json).

The separate raw-core comparison visited 446,039 entries / 13,837 directories,
with identical logical and allocated byte totals and zero errors in all 12 runs.
Its telemetry is in [throughput-core-codex.json](artifacts/throughput-core-codex.json).
Transitions are observed every 1,000 entries. A retired worker can still own a long
directory job, so provisional admission may outlast a short scan. Measurement windows
begin only after retirement acknowledgement; transition entries are discarded.

Reproduce after building, while other builds/profilers are idle:

```sh
cargo build --release
python3 etc/profiling/compare_thread_policies.py ~/github --output /tmp/dua-thread-comparison.json
```

## Rejected metadata changes

Descriptor-relative substitutions did not establish a benefit and were removed.
A selective full-directory inode-ordering candidate also failed the end-to-end
comparison, even after reusing existing directory-size hints to avoid probing small
directories. On the stable subtree, fixed-eight medians were 2.190 seconds / 7.027 CPU
for Adaptive versus 2.591 / 8.357 for the selective candidate. All 12 runs in that
comparison had identical counts and byte totals, with zero errors.
[Raw candidate results](artifacts/wide-hint-rejected.json) are retained, but the
candidate implementation and CLI flag are not in the final change. The previously
documented opt-in 4,096-entry experiments remain unchanged.

## Validation

343 workspace tests with all features passed, including 61 core and 159 CLI unit tests.
Controller tests cover threshold boundaries, noisy votes, bounded retries, normalization,
bracket search, and preservation of the legacy formula. Actual-pool tests cover retirement
acknowledgements, discarded transition entries, restart, one-hour timer cancellation,
and shutdown with blocked output. Strict workspace/all-target/all-feature Clippy passed.
Core all-target checks also passed for Linux musl and Windows MSVC.
Kernel-stack capture and interpretation are documented in [KERNEL-STACKS.md](KERNEL-STACKS.md).
