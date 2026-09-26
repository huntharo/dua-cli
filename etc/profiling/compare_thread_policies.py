#!/usr/bin/env python3
"""Serial CLI policy comparison on ~/github; build the executable first."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import resource
import subprocess
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("--executable", type=Path,
                        default=Path(__file__).resolve().parents[2] / "target/release/dua")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    root = args.root.resolve(strict=True)
    if not root.is_relative_to((Path.home() / "github").resolve(strict=True)):
        parser.error("comparison must remain within ~/github")
    executable = args.executable.resolve(strict=True)
    report = dict(root=str(root), sha256=hashlib.sha256(executable.read_bytes()).hexdigest(),
                  rows=[], identical_output=False)
    cases = [("legacy16", ["--threads", "16", "--thread-loss-percent", "20"]),
             ("default16", ["--threads", "16"]),
             ("fixed8", ["--threads", "8", "--fixed-threads"])]
    args.output.parent.mkdir(parents=True, exist_ok=True)
    tuning_environment = {"DUA_THREADS", "DUA_FIXED_THREADS", "DUA_MAX_THREADS",
                          "DUA_THREAD_BASELINE_MS", "DUA_THREAD_ADJUSTMENT_MS",
                          "DUA_THREAD_LOSS_PERCENT", "DUA_THREAD_THROUGHPUT_PERCENT"}
    environment = {key: value for key, value in os.environ.items() if key not in tuning_environment}
    for label, flags in cases + list(reversed(cases)) + cases:
        before = resource.getrusage(resource.RUSAGE_CHILDREN)
        start = time.monotonic()
        result = subprocess.run([str(executable), "--format", "bytes", *flags, str(root)],
                                capture_output=True, text=True, timeout=120, env=environment)
        elapsed = time.monotonic() - start
        after = resource.getrusage(resource.RUSAGE_CHILDREN)
        row = dict(label=label, args=flags, wall_seconds=elapsed,
                   cpu_seconds=after.ru_utime + after.ru_stime - before.ru_utime - before.ru_stime,
                   returncode=result.returncode, stderr=result.stderr,
                   stdout_sha256=hashlib.sha256(result.stdout.encode()).hexdigest(),
                   stdout_line_count=len(result.stdout.splitlines()))
        report["rows"].append(row)
        args.output.write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(row), flush=True)
        if result.returncode:
            raise RuntimeError("CLI failed; see recorded stderr")
    report["identical_output"] = len({r["stdout_sha256"] for r in report["rows"]}) == 1
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    if not report["identical_output"]:
        raise RuntimeError("aggregate outputs differ; do not treat these as equivalent-work timings")


if __name__ == "__main__":
    main()
