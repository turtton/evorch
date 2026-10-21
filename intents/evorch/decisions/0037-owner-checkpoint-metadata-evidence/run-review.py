"""Frozen Q9 review remeasurement; run only after builds and validation finish."""
import datetime
import hashlib
import json
import math
import os
from pathlib import Path
import random
import subprocess
import time

EVIDENCE = Path(__file__).resolve().parent
ROOT = EVIDENCE.parents[3]
BINARY = ROOT / "target/owner-checkpoint-bench/build/release"
PS = "/nix/store/0m9zlli0rz8vz4rbnlfdkaa963mx2nga-ps-procps-4.0.7/bin/ps"


def host_state():
    listing = subprocess.check_output([PS, "-eo", "pid,comm,pcpu"], text=True)
    conflicts = []
    for line in listing.splitlines()[1:]:
        fields = line.split()
        if len(fields) != 3:
            continue
        command = fields[1]
        if (command in ("cargo", "rustc", "cargo-nextest", "cc1", "cc1plus", "clang", "rust-lld", "ld")
                or command.startswith(("owner-checkpoin", "owner_checkpoin", "shell_jobs_inte"))):
            conflicts.append(line.strip())
    return {"utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
            "loadavg": Path("/proc/loadavg").read_text().strip(),
            "processes": listing, "conflicts": conflicts}


def upper95_zero(n):
    return -math.expm1(math.log(0.05) / n) if n else None


def summarize(rows):
    lines = ["# Q9 レビュー再測定結果", "", "各行15秒、N=M=20、5,000,000 B。throughput は成功数 / elapsed_seconds。",
             "p50/p99 は成功・失敗を含む reader 操作 latency（ms）。", "",
             "| 休止 ms | rep | 実装 | writer 失敗/総数 (%) | writer 成功/s | reader 失敗/総数 (%) | reader 成功/s | reader p50/p99 ms |",
             "|---:|---:|---|---:|---:|---:|---:|---:|"]
    for row in sorted(rows, key=lambda r: (r["pause_ms"], r["rep"], r["version"] == "after")):
        w, r, elapsed = row["writers"], row["readers"], row["elapsed_seconds"]
        lines.append(f'| {row["pause_ms"]} | {row["rep"]} | {row["version"]} | {w["failed"]}/{w["ops"]} ({100*w["failed"]/w["ops"]:.4f}) | {w["ok"]/elapsed:.2f} | {r["failed"]}/{r["ops"]} ({100*r["failed"]/r["ops"]:.4f}) | {r["ok"]/elapsed:.2f} | {r["p50_ms"]:.3f}/{r["p99_ms"]:.3f} |')
    lines += ["", "## 5反復の合算", "", "| 休止 ms | 実装 | writer 失敗/総数 (%) | reader 失敗/総数 (%) | writer/reader 成功/s |", "|---:|---|---:|---:|---:|"]
    for pause in (0, 100):
        for version in ("before", "after"):
            selected = [r for r in rows if r["pause_ms"] == pause and r["version"] == version]
            if not selected:
                continue
            sums = {who: {key: sum(row[who][key] for row in selected) for key in ("failed", "ops", "ok")} for who in ("writers", "readers")}
            elapsed = sum(row["elapsed_seconds"] for row in selected)
            w, r = sums["writers"], sums["readers"]
            lines.append(f'| {pause} | {version} | {w["failed"]}/{w["ops"]} ({100*w["failed"]/w["ops"]:.4f}) | {r["failed"]}/{r["ops"]} ({100*r["failed"]/r["ops"]:.4f}) | {w["ok"]/elapsed:.2f}/{r["ok"]/elapsed:.2f} |')
    lines += ["", "## 0件失敗の片側95%上限（独立二項仮定、%）", "", "競合失敗には相関があり、将来率の保証ではない。nは総操作数。非0件も上表で全件公開する。", ""]
    for pause in (0, 100):
        for version in ("before", "after"):
            selected = [r for r in rows if r["pause_ms"] == pause and r["version"] == version]
            if not selected:
                continue
            for who in ("writers", "readers"):
                for rep, group in [(str(row["rep"]), [row]) for row in selected] + [("all", selected)]:
                    n = sum(row[who]["ops"] for row in group)
                    failed = sum(row[who]["failed"] for row in group)
                    if failed == 0:
                        lines.append(f'- pause={pause}, {version}, {who}, rep={rep}: 0/{n}, 95% upper={100*upper95_zero(n):.5f}%.')
    return "\n".join(lines) + "\n"


def main():
    cases = [(version, pause, rep) for rep in range(1, 6) for pause in (0, 100) for version in ("before", "after")]
    random.Random(3703).shuffle(cases)
    source_hash = hashlib.sha256()
    for path in sorted((ROOT / "crates/runtime/src").rglob("*.rs")) + sorted((ROOT / "crates/storage/src").rglob("*.rs")):
        source_hash.update(str(path.relative_to(ROOT)).encode() + b"\0" + path.read_bytes() + b"\0")
    manifest = {"before_revision": "1d2cc55c", "after_runtime_storage_source_sha256": source_hash.hexdigest(), "seed": 3703, "cases": cases, "plan_sha256": hashlib.sha256((EVIDENCE / "review-remeasurement-plan.md").read_bytes()).hexdigest(),
                "binaries": {v: hashlib.sha256((BINARY / f"owner-checkpoint-bench-{v}").read_bytes()).hexdigest() for v in ("before", "after")}}
    paths = [EVIDENCE / name for name in ("review-manifest.json", "review-host.jsonl", "review-acceptance.jsonl")]
    if any(path.exists() for path in paths):
        assert all(path.exists() for path in paths), "incomplete evidence file set"
        assert json.loads(paths[0].read_text()) == json.loads(json.dumps(manifest)), "inputs changed on resume"
        rows = [json.loads(line) for line in paths[2].read_text().splitlines()]
        assert [(r["version"], r["pause_ms"], r["rep"]) for r in rows] == cases[:len(rows)], "resume order differs"
    else:
        paths[0].write_text(json.dumps(manifest, indent=2) + "\n")
        rows = []
    with paths[1].open("a") as host, paths[2].open("a") as output:
        for version, pause, rep in cases[len(rows):]:
            state = host_state()
            state.update(version=version, pause_ms=pause, rep=rep)
            host.write(json.dumps(state) + "\n"); host.flush()
            if state["conflicts"]:
                raise RuntimeError(f"concurrent build/test: {state['conflicts']}")
            env = os.environ.copy(); env["WRITER_PAUSE_MS"] = str(pause)
            start = time.monotonic()
            result = subprocess.run([str(BINARY / f"owner-checkpoint-bench-{version}"), "permit", "5000000", "20", "20", "15", "0", str(rep)], env=env, capture_output=True, text=True, timeout=90, check=True)
            row = json.loads(result.stdout)
            row.update(version=version, pause_ms=pause, wall_seconds=time.monotonic()-start)
            output.write(json.dumps(row) + "\n"); output.flush()
            rows.append(row)
            assert row["quick_check"] == "ok"
            assert not row["writers"]["other_errors"] and not row["readers"]["other_errors"]
            (EVIDENCE / "review-summary.md").write_text(summarize(rows))
            print(version, pause, rep, "writer", row["writers"]["failed"], "/", row["writers"]["ops"], "reader", row["readers"]["failed"], "/", row["readers"]["ops"], flush=True)


if __name__ == "__main__":
    main()
