#!/usr/bin/env bash
# CI benchmark (DO NOT MERGE): dump and reset sccache stats between build steps.
# usage: snap.sh <name>   (no-op when sccache is off)
set -euo pipefail
[[ "${RUSTC_WRAPPER:-}" == sccache ]] || exit 0
sccache --show-stats --stats-format json > "$M2_OUT/sccache-$1.json"
sccache --show-stats | tee "$M2_OUT/sccache-$1.txt"
sccache --zero-stats > /dev/null
