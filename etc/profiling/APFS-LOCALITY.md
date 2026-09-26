# APFS metadata locality experiment

## Hypothesis and scope

APFS stores filesystem records in a B-tree ordered first by object identifier, then record type, with additional key fields for some record types. An inode record's object identifier is its inode number. Directory records are keyed under their parent directory. Consequently, a directory enumeration order need not resemble the order of the target files' inode records. Sorting metadata requests by inode number is a plausible way to improve reuse of B-tree nodes; it is not a physical-block-address sort. APFS copy-on-write and object maps further prevent treating a virtual object identifier as a disk address. See Apple's [APFS format reference](https://developer.apple.com/support/apple-file-system/Apple-File-System-Reference.pdf), File-System Objects and Objects.

The existing measurements demonstrate substantial physical metadata reads and kernel CPU, but do not identify unique block addresses. They cannot establish that a page was reread 10–50 times. Reduced process-attributed read bytes in an ordering experiment would support the locality hypothesis without proving that exact mechanism. Cache residency, file creation history, directory shape, other processes, and APFS implementation details remain relevant.

The first implementation uses supported directory APIs, not a raw APFS parser. Raw traversal would need a consistent filesystem checkpoint, object maps, encryption handling, snapshots, links, and correct visible-volume semantics. In contrast, the kernel already resolves these for directory-relative metadata calls.

The DiskHound audit found a materially different NTFS path: `native/diskhound-native-scanner/src/mft.rs` opens the volume, reads MFT record zero and its nonresident data runlist, then reads metadata extents in 4 MiB chunks. Physical cluster numbers locate extents; record numbers identify positions in the logical MFT stream. Even a requested subtree uses a volume-wide MFT read followed by reconstructed-path filtering. Selection there is capability precedence with ordinary-walker fallback, not a measured algorithm race. Its checked-out macOS path uses jwalk; a separate existing branch switches to dua-core. The useful transferable idea is bulk access to the filesystem's metadata index, not assuming every filesystem has an MFT equivalent exposed through the same interface.

`fs_capabilities.c` queries this machine without enumerating a volume. `~/github` resides on APFS `/dev/disk3s5`, with 4,096-byte filesystem blocks, in container `disk3` backed by `disk0s2`. The volume reports `VOL_CAP_INT_SEARCHFS` supported. However, Apple's [searchfs documentation](https://raw.githubusercontent.com/apple-oss-distributions/xnu/main/bsd/man/man2/searchfs.2) states that it searches the entire volume even when passed a subtree path. No `searchfs` search or raw-device read was run: all traversal experiments remain confined to `~/github`. The capability bit for `readdirattr` does not describe `getattrlistbulk` support.

## Controlled candidates

The macOS-only core option `Options.macos_metadata_strategy` and CLI `--metadata-strategy` expose four strategies:

| Strategy | Metadata access | Purpose |
| --- | --- | --- |
| `adaptive` | Existing bulk probe, potentially reopen and distribute full-path stats | Preserve the existing default as the control |
| `bulk` | Native bulk metadata, without the timing-based reopen | Isolate bulk access; retain unsupported-filesystem fallback |
| `directory-local` | Parallel metadata batches relative to an open directory descriptor | Avoid resolving the full parent pathname for each stat |
| `inode-ordered` | Same descriptor-relative batches, with bounded inode ordering | Isolate the extra benefit or cost of logical metadata ordering |

The two new candidates use the same 4,096-entry buffer and 64-entry metadata jobs. Sorting is bounded and does not globally order the entire filesystem. Concurrent batches can execute in a different order. These are metadata-locality candidates, not a guarantee that all physical reads are sequential. Symbolic links remain unfollowed; metadata errors, traversal predicates, directory identity and parent-before-child output retain their existing meaning. Clone-metadata requests retain the existing path-based metadata implementation where needed for accounting compatibility.

Keep worker count fixed while comparing metadata strategies. Otherwise a change in thread admission changes two variables at once. All strategies use the same consumer and accounting logic in the comparison harness.

The walk signatures remain unchanged. macOS API callers constructing exhaustive `Options` literals must add `macos_metadata_strategy` or use `..Options::default()`; Linux and Windows do not gain this field. The default variant is `MacosMetadataStrategy::Adaptive`.

## Measurement-based selection

`calibrate_metadata.py` detects the root filesystem. On APFS it measures all four candidates; on other macOS filesystems it currently compares only the existing path and native bulk. It rotates/reverses candidate order, runs the complete same subtree repeatedly, records CPU and process-attributed reads, and optionally records device-wide I/O counters. Selection is refused on traversal errors, mismatched entry/directory/size/allocation totals, or multiple observed devices.

The default selects the fastest median traversal. `--throughput-fraction .8` instead chooses the lowest median CPU per entry among candidates achieving at least 80% of the best observed throughput. This is an explicit performance/resource tradeoff, not a disk-utilization target. A generated recommendation applies to the sampled workload, filesystem, options, worker count and current cache conditions. It is not silently installed as a machine-wide default or an ongoing capacity estimate.

```sh
cargo build --release --locked --manifest-path etc/profiling/harness/Cargo.toml
python3 etc/profiling/calibrate_metadata.py ~/github \
  --threads 8 --repetitions 3 --device disk0 \
  --output /tmp/dua-apfs-locality.json

# After inspecting the recommendation, use a candidate in an ordinary CLI scan:
dua --fixed-threads --threads 8 --metadata-strategy inode-ordered ~/github

# Read-only capability query, no traversal:
cc -Wall -Wextra -Werror etc/profiling/fs_capabilities.c -o /tmp/dua-fs-capabilities
/tmp/dua-fs-capabilities ~/github
```

Builds and other benchmarks must finish before measuring. Do not purge caches or expand the scan to the whole disk to make results look cleaner. Device counters include unrelated processes, and their accumulated request durations are not utilization percentages.

## Initial APFS results

Three runs per strategy at fixed 8 workers on this machine's `~/github` tree, after all builds finished, returned matching counts: 660,862 entries, 36,176 directories, zero errors, 234,038,233,698 logical bytes and 235,406,057,472 allocated bytes on one device. All candidates used the same optimized harness and dependency versions pinned from the repository lockfile. This is core traversal, not CLI aggregation. The tree differs slightly from the earlier profiling experiment and its timings are not a direct before/after comparison to that older harness.

| Strategy | Median scan seconds (range) | Median CPU seconds | Median process physical reads, MiB |
| --- | ---: | ---: | ---: |
| Existing adaptive | 2.926 (2.86–6.59) | 12.800 | 492.1 |
| Bulk | 9.800 (7.38–10.77) | 11.025 | 521.2 |
| Directory-local | 4.054 (3.94–7.99) | 16.858 | 632.0 |
| Inode-ordered, 4,096-entry windows | 3.705 (3.64–4.41) | 17.217 | 519.9 |

The selector retained **adaptive**. Relative to directory-local, inode ordering reduced median process-attributed reads by about 18% and median wall time by about 9%, with slightly higher CPU. This supports further examination of locality, but the new candidates did not beat the existing implementation overall. The ranges show substantial temporal variability; three runs are evidence for this reference workload, not a filesystem-wide performance guarantee. Full observations, counter windows and executable SHA-256 are in [artifacts/apfs-locality-8.json](artifacts/apfs-locality-8.json).

Validation after integration: 334 workspace tests passed with all features, strict workspace/all-target/all-feature Clippy passed, the five `make check` configurations and journey tests passed, and the selector's three tests passed. Core candidate checks also compiled for Linux musl and Windows MSVC in the implementation worktree. Tests cover metadata parity, sparse files, hard links, symlinks, non-ASCII names, permissions/errors, pre-epoch timestamps, directory-descriptor lifetime after rename, pruning and parent IDs, bounded buffering/jobs, cancellation, and clone/resource-fork parity. Production defaults remain unchanged.
