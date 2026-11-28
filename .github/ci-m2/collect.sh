#!/usr/bin/env bash
# CI benchmark (DO NOT MERGE): gather the raw measurement data of a job.
set -uo pipefail
out=$M2_OUT
mkdir -p "$out/timings"
find target -path '*cargo-timings/cargo-timing-*.html' -print0 2> /dev/null |
  while IFS= read -r -d '' f; do
    cp "$f" "$out/timings/$(echo "$f" | tr '/' '_')"
  done
if [[ "${RUSTC_WRAPPER:-}" == sccache ]]; then
  sccache --show-stats --stats-format json > "$out/sccache-final.json" || true
  sccache --show-stats | tee "$out/sccache-final.txt" || true
  du -sb "$SCCACHE_DIR" | tee "$out/sccache-dir-bytes.txt"
  find "$SCCACHE_DIR" -type f | wc -l | tee "$out/sccache-dir-files.txt"
  sccache --stop-server || true
fi
date -u +%FT%TZ > "$out/collected-at.txt"
ls -la "$out" "$out/timings"
