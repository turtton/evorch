#!/usr/bin/env python3
"""Read-only Linux process/thread I/O sampling; JSON goes to stdout, never a log file."""

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


class IdentityChangedError(RuntimeError):
    """A PID or TID changed identity while reading one sample."""


def positive_int(value):
    result = int(value)
    if result <= 0:
        raise argparse.ArgumentTypeError("must be a positive integer")
    return result


def positive_float(value):
    result = float(value)
    if not math.isfinite(result) or result <= 0:
        raise argparse.ArgumentTypeError("must be a finite positive number")
    return result


def parse_stat(text):
    # comm may contain spaces and parentheses. The last ')' terminates it.
    identifier, separator, _ = text.partition(" (")
    if not separator:
        raise ValueError("invalid /proc stat header")
    stat = text.rsplit(")", 1)[1].split()
    return {
        "id": int(identifier),
        "start_ticks": int(stat[19]),  # field 22
        # Fields 14/15, deliberately excluding cutime/cstime (fields 16/17).
        "cpu_ticks": int(stat[11]) + int(stat[12]),
    }


def parse_io(text):
    result = {}
    for line in text.splitlines():
        name, value = line.split(":", 1)
        if name in IO_FIELDS:
            counter = int(value.strip())
            if counter < 0:
                raise ValueError(f"negative I/O counter: {name}")
            result[name] = counter
    return result


def identity_change(first, last):
    if last["process_start_ticks"] != first["process_start_ticks"]:
        return "pid_reused"
    if last["start_ticks"] != first["start_ticks"]:
        return "tid_reused"
    return None


def snapshot(pid, tid=None, proc_root=Path("/proc")):
    process_root = proc_root / str(pid)
    process = parse_stat((process_root / "stat").read_text())
    root = process_root if tid is None else process_root / "task" / str(tid)
    target = process if tid is None else parse_stat((root / "stat").read_text())
    if process["id"] != pid or target["id"] != (pid if tid is None else tid):
        raise ValueError("/proc stat identifier does not match the selected PID/TID")
    result = {
        "time": time.monotonic(),
        "process_start_ticks": process["start_ticks"],
        "start_ticks": target["start_ticks"],
        "cpu_ticks": target["cpu_ticks"],
        "io": {},
        "io_error": None,
    }
    try:
        result["io"] = parse_io((root / "io").read_text())
    except (OSError, ValueError) as error:
        # Missing permissions are unavailable, not a zero-byte measurement.
        result["io_error"] = str(error)
    # Reject a sample that could mix the old task's stat with a reused task's
    # I/O. Check the process too: a TID alone does not identify a thread group.
    target_after = parse_stat((root / "stat").read_text())
    process_after = target_after if tid is None else parse_stat((process_root / "stat").read_text())
    change = identity_change(result, {
        "process_start_ticks": process_after["start_ticks"],
        "start_ticks": target_after["start_ticks"],
    })
    if change:
        raise IdentityChangedError(f"{change}_during_sample")
    return result


def report(pid, first, last, tick_rate, kind, stop_reason=None, tid=None):
    # /proc/PID/io includes signal->ioac, to which wait_task_zombie adds reaped
    # child I/O. /proc/PID/task/TID/io reads only that task's own accounting.
    # CPU utime/stime deliberately do not include child cutime/cstime.
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
    cpu_delta = last["cpu_ticks"] - first["cpu_ticks"]
    cpu_seconds = cpu_delta / tick_rate if cpu_delta >= 0 else None
    return {
        "kind": kind,
        "pid": pid,
        "tid": tid,
        "scope": "process" if tid is None else "thread",
        "cpu_scope": (
            "process threads; excludes child CPU" if tid is None
            else "selected thread only; excludes other threads and child CPU"
        ),
        "io_scope": (
            "process threads and reaped children; excludes still-running children" if tid is None
            else "selected thread only; excludes other threads and child processes"
        ),
        "elapsed_seconds": elapsed,
        "cpu_seconds": cpu_seconds,
        "cpu_percent_one_core": cpu_seconds / elapsed * 100
        if cpu_seconds is not None and elapsed > 0 else None,
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
    parser.add_argument("--pid", type=positive_int, required=True)
    parser.add_argument("--tid", type=positive_int,
                        help="sample only this thread via /proc/PID/task/TID (e.g. storage-writer)")
    parser.add_argument("--duration", type=positive_float, default=60.0)
    parser.add_argument("--interval", type=positive_float, default=1.0)
    parser.add_argument("--samples", action="store_true", help="also print per-interval JSON lines")
    args = parser.parse_args()
    if not Path("/proc/self/stat").exists():
        parser.error("requires Linux /proc")
    tick_rate = os.sysconf("SC_CLK_TCK")
    target_name = f"PID {args.pid}" + (f" TID {args.tid}" if args.tid is not None else "")
    try:
        first = snapshot(args.pid, args.tid)
    except (OSError, ValueError, IndexError, IdentityChangedError) as error:
        parser.exit(1, f"cannot inspect {target_name}: {error}\n")
    previous = first
    deadline = first["time"] + args.duration
    stop_reason = "duration_completed"
    try:
        while previous["time"] < deadline:
            time.sleep(min(args.interval, max(0.0, deadline - time.monotonic())))
            try:
                current = snapshot(args.pid, args.tid)
            except IdentityChangedError as error:
                stop_reason = str(error)
                break
            except (OSError, ValueError, IndexError) as error:
                stop_reason = f"{'process' if args.tid is None else 'thread'}_unavailable: {error}"
                break
            change = identity_change(first, current)
            if change:
                stop_reason = change
                break
            if args.samples:
                print(json.dumps(report(args.pid, previous, current, tick_rate, "sample", tid=args.tid)), flush=True)
            previous = current
    except KeyboardInterrupt:
        stop_reason = "interrupted"
    print(json.dumps(report(args.pid, first, previous, tick_rate, "summary", stop_reason, tid=args.tid)))


if __name__ == "__main__":
    main()
