#!/usr/bin/env python3
"""Compare complete walks on one filesystem; never claim to measure disk capacity.

Run serially after building harness/Cargo.toml. All roots must be inside ~/github.
This deliberately calibrates before choosing a strategy: it does not add duplicate
metadata probes or change strategies during an ordinary production traversal.
"""
import argparse
import hashlib
import json
import plistlib
import statistics
import subprocess
import tempfile
import time
from pathlib import Path


def device_snapshot(device):
    if not device:
        return {}
    data = plistlib.loads(subprocess.check_output([
        "/usr/sbin/ioreg", "-a", "-l", "-r", "-c", "IOBlockStorageDriver", "-d", "2"
    ]))
    for driver in data:
        if any(c.get("BSD Name") == device and c.get("Whole")
               for c in driver.get("IORegistryEntryChildren", [])):
            return driver["Statistics"]
    raise RuntimeError(f"physical device {device!r} not found")


def summarize(rows, fraction):
    """Reject incomplete/non-equivalent walks before selecting by measured cost."""
    if not rows or not 0 < fraction <= 1:
        raise ValueError("nonempty measurements and fraction in (0, 1] required")
    reference = rows[0]["result"]["count"]
    if reference["errors"] or reference["entries"] == 0:
        raise ValueError("reference has errors or no entries; selection refused")
    if len(reference["devices"]) != 1:
        raise ValueError("calibration crosses filesystems; select a subtree on one device")
    if any(row["result"]["count"] != reference for row in rows):
        raise ValueError("entry, directory, size, allocation or error totals differ; selection refused")
    by_strategy = {}
    for row in rows:
        by_strategy.setdefault(row["strategy"], []).append(row["result"])
    summary = []
    for name, runs in by_strategy.items():
        wall = statistics.median(r["wall_seconds"] for r in runs)
        cpu = statistics.median(r["user_seconds"] + r["system_seconds"] for r in runs)
        reads = statistics.median(r["proc_after"]["disk_read_bytes"] -
                                  r["proc_before"]["disk_read_bytes"] for r in runs)
        summary.append(dict(strategy=name, runs=len(runs), median_seconds=wall,
                            median_cpu_seconds=cpu, entries_per_second=reference["entries"] / wall,
                            cpu_seconds_per_entry=cpu / reference["entries"],
                            median_process_read_bytes=reads,
                            min_seconds=min(r["wall_seconds"] for r in runs),
                            max_seconds=max(r["wall_seconds"] for r in runs)))
    best_rate = max(s["entries_per_second"] for s in summary)
    eligible = [s for s in summary if s["entries_per_second"] >= fraction * best_rate]
    choice = min(eligible, key=lambda s: (s["cpu_seconds_per_entry"], s["median_seconds"]))
    return dict(throughput_fraction=fraction, selected=choice["strategy"], strategies=summary,
                count=reference, rule="lowest median CPU per entry within fraction of best observed throughput")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("--harness", type=Path,
                        default=Path(__file__).parent / "harness/target/release/dua-profile-walk")
    parser.add_argument("--threads", type=int, default=8)
    parser.add_argument("--repetitions", type=int, default=3)
    parser.add_argument("--throughput-fraction", type=float, default=1.0,
                        help="1 selects fastest median; e.g. .8 permits 20%% throughput loss for lower CPU")
    parser.add_argument("--device", help="optional physical-device counters, e.g. disk0; includes other processes")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    root = args.root.resolve(strict=True)
    if not root.is_relative_to((Path.home() / "github").resolve(strict=True)):
        parser.error("calibration must stay within ~/github")
    if args.threads < 1 or args.repetitions < 2 or not 0 < args.throughput_fraction <= 1:
        parser.error("positive threads, at least two repetitions, and fraction in (0, 1] required")
    exe = str(args.harness.resolve(strict=True))
    fs = json.loads(subprocess.check_output([exe, "--filesystem", str(root)], text=True))
    # Inode ordering follows APFS logical B-tree keys; other filesystems have no such prior here.
    strategies = ["adaptive", "bulk"]
    if fs["type"] == "apfs":
        strategies += ["directory-local", "inode-ordered"]
    report = dict(filesystem=fs, root=str(root), threads=args.threads,
                  harness_sha256=hashlib.sha256(Path(exe).read_bytes()).hexdigest(),
                  repetitions=args.repetitions, rows=[], selection=None)
    args.output.parent.mkdir(parents=True, exist_ok=True)

    def save():
        args.output.write_text(json.dumps(report, indent=2) + "\n")

    with tempfile.TemporaryDirectory(prefix="dua-metadata-calibration-") as temporary:
        config = Path(temporary) / "config.json"
        for repeat in range(args.repetitions):
            # Rotate and reverse order to reduce systematic first/last and cache advantages.
            order = strategies[repeat % len(strategies):] + strategies[:repeat % len(strategies)]
            if repeat % 2:
                order.reverse()
            for strategy in order:
                config.write_text(json.dumps(dict(threads=args.threads, roots=[str(root)], strategy=strategy)))
                before = device_snapshot(args.device)
                start = time.monotonic()
                process = subprocess.run([exe, str(config)], capture_output=True, text=True, timeout=120)
                wall = time.monotonic() - start
                after = device_snapshot(args.device)
                if process.returncode:
                    report["failure"] = dict(strategy=strategy, returncode=process.returncode, stderr=process.stderr)
                    save()
                    raise RuntimeError(process.stderr)
                result = json.loads(process.stdout)
                row = dict(strategy=strategy, repeat=repeat, outer_seconds=wall, result=result,
                           device_delta={k: after[k] - v for k, v in before.items()})
                report["rows"].append(row)
                save()
                print(json.dumps(dict(strategy=strategy, repeat=repeat,
                                      wall=result["wall_seconds"],
                                      cpu=result["user_seconds"] + result["system_seconds"],
                                      count=result["count"])), flush=True)
    try:
        report["selection"] = summarize(report["rows"], args.throughput_fraction)
    except ValueError as error:
        report["selection_error"] = str(error)
        save()
        raise
    save()
    print(json.dumps(report["selection"], indent=2))


if __name__ == "__main__":
    main()
