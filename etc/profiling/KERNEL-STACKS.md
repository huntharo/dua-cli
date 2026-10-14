# Kernel-stack capture after KDK installation

2026-09-26, macOS 26.6.1 (25G76), KDK_26.6.1_25G76.kdk. The running
ARM64_T6050 kernel and installed release-kernel dSYM both have UUID
`1C459E7C-531A-3F4F-B8E2-DE7E34E21F35`.

In Instruments, a fresh Time Profiler document was configured with **Record
Kernel Callstacks** enabled and saved as the user template `Dua Kernel Stacks`.
Record Waiting Threads and Context Switch Sampling remained disabled. Recording
and export succeeded as the existing user, with SIP enabled and no elevation,
boot changes, or security changes.

The existing original fixed-16 profiling harness scanned only `/Users/huntharo/github`.
Five passes completed with 3,384,035 entries, 190,630 directories, zero errors,
15.551 seconds wall time, 3.864 user CPU seconds and 104.996 system CPU seconds.
The tree has changed since earlier measurements, so counts and timings should
not be treated as a controlled before/after comparison.

Commands (paths refer to this machine):

```sh
xcrun xctrace record \
  --template "$HOME/Library/Application Support/Instruments/Templates/Dua Kernel Stacks.tracetemplate" \
  --output /tmp/dua-profile-20260926/kernel16.trace --time-limit 25s \
  --target-stdout /tmp/dua-profile-20260926/kernel16-run.json \
  --launch -- /tmp/dua-profile-20260926/harness/target/release/dua-profile-walk \
  /tmp/dua-profile-20260926/profile-16.json
xcrun xctrace symbolicate --input /tmp/dua-profile-20260926/kernel16.trace \
  --dsym /Library/Developer/KDKs/KDK_26.6.1_25G76.kdk
xcrun xctrace export --input /tmp/dua-profile-20260926/kernel16.trace \
  --xpath '/trace-toc/run[@number="1"]/data/table[@schema="time-profile"]' \
  --output /tmp/dua-profile-20260926/kernel16-samples.xml
```

XML global id/ref links were resolved and rows filtered to `dua-profile-walk`
(PID 95261). All 106,726 samples were Running-state rows, with total sample
weight 106.726 seconds. Stacks containing the kernel binary accounted for
102.993 seconds. Sample weight is summed across threads, not elapsed time.

Selected exclusive leaf weights:

| Function | Sample weight (seconds) |
| --- | ---: |
| atomic_exchange_complete32 | 24.063 |
| ml_get_timebase | 6.216 |
| thread_block_reason | 4.727 |
| load_exclusive32 | 4.218 |
| IORWLockUnlock | 3.427 |
| apfs_key_compare | 3.094 |
| obj_get | 2.979 |
| lck_rw_lock_shared_internal_inline | 2.971 |

Named internal stacks now include `apfs_vnop_lookup -> apfs_load_inode_internal
-> fs_get_inode_with_hint -> ... -> btree_node_get -> obj_get -> obj_get_finish
-> lck_rw_unlock_shared -> atomic_exchange_complete32`. Other samples reach
`lck_rw_lock_shared_gen -> ml_get_timebase` beneath APFS object lookup. Bulk
directory enumeration also reaches the same APFS B-tree/object machinery.

This establishes substantial sampled CPU cost in kernel synchronization and
APFS metadata traversal. The atomic leaf alone accounts for 22.5% of total
sample weight, but it occurs in several callers (including sandbox code).
It is not valid to label all of that time lock waiting or retry spinning.
The capture does not quantify blocked time, identify a specific contended lock,
or establish how these costs scale without a matched lower-concurrency capture.

The symbolicator reports no APFS dSYM, but many APFS function names are available
in the trace. Kernel source paths and line numbers resolve using the matching
kernel dSYM. Full private APFS debug information is not established.

Raw trace, workload JSON, TOC, exported samples, summary and lock-caller groups:
`/tmp/dua-profile-20260926/kernel16*`. No production source or defaults changed.

## Lower-concurrency comparison

A later fixed-eight capture used the same harness/template, with all samples filtered
to the target process. Summaries and workload counters are preserved in
[artifacts/kernel-comparison.json](artifacts/kernel-comparison.json).
Total CPU per entry was 22.08 microseconds at eight workers and 32.17 at sixteen;
exclusive `atomic_exchange_complete32` sample weight per entry was 2.81 and 7.11
microseconds respectively. The live tree changed slightly between these captures.
These instrumented comparisons establish a concurrency-sensitive synchronization
cost; uninstrumented equivalent-work benchmarks remain the acceptance criterion
for production changes.

Apple's [atomic exchange implementation](https://github.com/apple-oss-distributions/xnu/blob/main/osfmk/arm/locks_arm.c)
uses compare/exchange or exclusive load/store operations. A sample in that primitive
does not by itself count retries, identify a lock address, or measure time blocked.
