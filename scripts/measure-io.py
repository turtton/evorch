#!/usr/bin/env python3
"""Read-only Linux process I/O sampling. JSON goes to stdout, never a log file."""

import argparse
import json
import math
import os
from pathlib import Path
import time


IO_FIELDS = (
    "rchar", "wchar", "syscr", "syscw", "read_bytes", "write_bytes",
    "cancelled_write_bytes",
)


def positive_float(value):
    result = float(value)
    if not math.isfinite(result) or result <= 0:
        raise argparse.ArgumentTypeError("must be a finite positive number")
    return result


def snapshot(pid):
    root = Path("/proc") / str(pid)
    # comm may contain spaces and parentheses. The last ')' terminates it.
    stat = (root / "stat").read_text().rsplit(")", 1)[1].split()
    result = {
        "time": time.monotonic(),
        "start_ticks": int(stat[19]),  # stat field 22, detects PID reuse
        "cpu_ticks": int(stat[11]) + int(stat[12]),
        "io": {},
        "io_error": None,
    }
    try:
        for line in (root / "io").read_text().splitlines():
            name, value = line.split(":", 1)
            if name in IO_FIELDS:
                result["io"][name] = int(value.strip())
    except (OSError, ValueError) as error:
        # Missing permissions are unavailable, not a zero-byte measurement.
        result["io_error"] = str(error)
    return result


def report(pid, first, last, tick_rate, kind, stop_reason=None):
    elapsed = last["time"] - first["time"]
    delta = {}
    for field in IO_FIELDS:
        before = first["io"].get(field)
        after = last["io"].get(field)
        delta[field] = (
            after - before
            if before is not None and after is not None and after >= before
            else None
        )
    cpu_seconds = (last["cpu_ticks"] - first["cpu_ticks"]) / tick_rate
    return {
        "kind": kind,
        "pid": pid,
        "scope": "process (all threads; excludes child processes)",
        "elapsed_seconds": elapsed,
        "cpu_seconds": cpu_seconds,
        "cpu_percent_one_core": cpu_seconds / elapsed * 100 if elapsed > 0 else None,
        "io_delta": delta,
        "io_per_second": {
            field: value / elapsed if value is not None and elapsed > 0 else None
            for field, value in delta.items()
        },
        "unavailable_fields": [field for field, value in delta.items() if value is None],
        "io_error": last["io_error"] or first["io_error"],
        "stop_reason": stop_reason,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pid", type=int, required=True)
    parser.add_argument("--duration", type=positive_float, default=60.0)
    parser.add_argument("--interval", type=positive_float, default=1.0)
    parser.add_argument("--samples", action="store_true", help="also print per-interval JSON lines")
    args = parser.parse_args()
    if args.pid <= 0 or not Path("/proc/self/stat").exists():
        parser.error("requires Linux /proc and a positive PID")
    tick_rate = os.sysconf("SC_CLK_TCK")
    try:
        first = snapshot(args.pid)
    except (OSError, ValueError, IndexError) as error:
        parser.exit(1, f"cannot inspect PID {args.pid}: {error}\n")
    previous = first
    deadline = first["time"] + args.duration
    stop_reason = "duration_completed"
    try:
        while previous["time"] < deadline:
            time.sleep(min(args.interval, max(0.0, deadline - time.monotonic())))
            try:
                current = snapshot(args.pid)
            except (OSError, ValueError, IndexError) as error:
                stop_reason = f"process_unavailable: {error}"
                break
            if current["start_ticks"] != first["start_ticks"]:
                stop_reason = "pid_reused"
                break
            if args.samples:
                print(json.dumps(report(args.pid, previous, current, tick_rate, "sample")), flush=True)
            previous = current
    except KeyboardInterrupt:
        stop_reason = "interrupted"
    print(json.dumps(report(args.pid, first, previous, tick_rate, "summary", stop_reason)))


if __name__ == "__main__":
    main()
