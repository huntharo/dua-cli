#!/usr/bin/env python3
"""Serial core-walk comparison; home traversal requires --allow-home explicitly.

Stores numeric telemetry only, never visited paths. Run after builds have finished.
The home directory is live: report count/error differences instead of assuming an
identical snapshot. This measures raw core traversal, not CLI aggregation.
"""
import argparse
import csv
import hashlib
import json
import os
from pathlib import Path
import resource
import subprocess
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--allow-home", action="store_true", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--binary", type=Path, default=Path("target/release/examples/thread_probe"))
    parser.add_argument("--reference-binary", type=Path, help="Compare new/reference/reference/new throughput16 builds instead of fixed/legacy controls")
    parser.add_argument("--timeout", type=int, default=1800)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    home = Path.home().resolve(strict=True)
    args.output.mkdir(parents=True, exist_ok=True)
    result = {
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "root": "~",
        "scope": "raw core, no symlink following, ordinary user permissions",
        "interval_ms": 250,
        "retained_throughput": 0.80,
        "rows": [],
    }
    cases = [(mode, count, "current", binary) for mode, count in
             [("throughput", 16), ("fixed", 8), ("adaptive", 16), ("fixed", 16), ("throughput", 16)]]
    if args.reference_binary:
        reference = args.reference_binary.resolve(strict=True)
        result["reference_binary_sha256"] = hashlib.sha256(reference.read_bytes()).hexdigest()
        cases = [("throughput", 16, variant, executable) for variant, executable in
                 [("new", binary), ("reference", reference), ("reference", reference), ("new", binary)]]
    for index, (mode, count, variant, executable) in enumerate(cases):
        label = f"{index + 1}-{variant}-{mode}-{count}"
        print(f"START {label}", flush=True)
        before = resource.getrusage(resource.RUSAGE_CHILDREN)
        start = time.monotonic()
        with (args.output / f"{label}.csv").open("w") as telemetry:
            child = subprocess.run(
                [str(executable), mode, str(count), str(home), "--allow-home"],
                stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=telemetry,
                text=True, timeout=args.timeout,
                env={k: v for k, v in os.environ.items() if not k.startswith("DUA_")},
            )
        wall = time.monotonic() - start
        after = resource.getrusage(resource.RUSAGE_CHILDREN)
        if child.returncode != 0:
            raise RuntimeError(f"{label} exited {child.returncode}; inspect its telemetry file")
        summary = {}
        for item in child.stdout.split():
            key, value = item.split("=", 1)
            summary[key] = (value == "true") if value in ("true", "false") else float(value) if "." in value else int(value)
        with (args.output / f"{label}.csv").open() as stream:
            observations = [
                {key: value == "true" if value in ("true", "false") else int(value)
                 for key, value in row.items()}
                for row in csv.DictReader(stream)
            ]
        complete = next((row for row in observations if row["tuning_complete"]), None)
        changes = []
        previous = None
        for row in observations:
            if row["admitted"] != previous:
                changes.append(row)
                previous = row["admitted"]
        row = {
            "label": label, "variant": variant, "mode": mode, "initial_threads": count,
            "outer_wall_seconds": wall,
            "user_seconds": after.ru_utime - before.ru_utime,
            "system_seconds": after.ru_stime - before.ru_stime,
            "summary": summary, "observations": observations, "count_changes": changes,
            "first_tuning_complete": complete,
            "changes_after_tuning_complete": [] if complete is None else [
                change for change in changes if change["elapsed_ms"] > complete["elapsed_ms"]
            ],
        }
        result["rows"].append(row)
        (args.output / "results.json").write_text(json.dumps(result, indent=2) + "\n")
        print(json.dumps({key: value for key, value in row.items() if key not in ("observations", "count_changes")}), flush=True)
    (args.output / "comparison.done").write_text("0\n")


if __name__ == "__main__":
    main()
