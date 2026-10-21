"""Supplementary ownership-only matrix; run from the designated worktree.

Append each completed case durably enough for the driver to resume after an
interrupted harness. Existing cases are validated and never rerun or overwritten.
"""
import json
import os
from pathlib import Path
import random
import subprocess
import time

base = Path.cwd() / "target/owner-checkpoint-bench"
cases = [(version, pause, size, count) for version in ("before", "after")
         for pause in (0, 100) for size in (100000, 304247, 2132213, 5000000)
         for count in (2, 5, 10, 20)]
random.Random(3701).shuffle(cases)

def key(row):
    assert row["n"] == row["m"]
    assert row["quick_check"] == "ok"
    assert not row["writers"]["other_errors"]
    assert not row["readers"]["other_errors"]
    assert row["rep"] == 1 and row["seconds"] == 5.0
    return row["version"], row["pause_ms"], row["requested_bytes"], row["n"]

path = base / "matrix.jsonl"
completed = [key(json.loads(line)) for line in path.read_text().splitlines()] if path.exists() else []
assert len(completed) == len(set(completed)), "duplicate cases"
assert set(completed) <= set(cases), "unexpected cases"
with path.open("a") as output:
    for index, (version, pause, size, count) in enumerate(cases, 1):
        if (version, pause, size, count) in completed:
            continue
        env = os.environ.copy()
        env["WRITER_PAUSE_MS"] = str(pause)
        command = [str(base / f"build/release/owner-checkpoint-bench-{version}"),
                   "permit", str(size), str(count), str(count), "5", "0", "1"]
        start = time.monotonic()
        result = subprocess.run(command, env=env, capture_output=True, text=True,
                                timeout=60, check=True)
        row = json.loads(result.stdout)
        row.update(version=version, pause_ms=pause)
        assert not row["writers"]["other_errors"]
        assert not row["readers"]["other_errors"]
        assert row["quick_check"] == "ok"
        output.write(json.dumps(row) + "\n")
        output.flush()
        print(index, len(cases), version, pause, size, count,
              "writer", row["writers"]["failed"], "/", row["writers"]["ops"],
              "reader", row["readers"]["failed"], "/", row["readers"]["ops"],
              "elapsed", time.monotonic() - start, flush=True)
