#!/usr/bin/env bash
# Use the same workspace feature union for gates and the remaining tests.
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/contract-test-filters.sh
suite="${1:?expected cache, io, or remaining}"
shift
case "$suite" in
  cache) filter="$CACHE_CONTRACT_FILTER" ;;
  io) filter="($IO_CONTRACT_FILTER) & not ($CACHE_CONTRACT_FILTER)" ;;
  remaining) filter="not (($CACHE_CONTRACT_FILTER) | ($IO_CONTRACT_FILTER))" ;;
  *) echo "unknown contract suite: $suite" >&2; exit 2 ;;
esac
selection=(--workspace --locked)
if [[ -n "${EVORCH_CI_METADATA_DIR:-}" ]]; then
  # Metadata is generated in this checkout immediately before execution. These
  # flags reuse binaries without invoking Cargo or changing the feature union.
  selection=(--cargo-metadata "$EVORCH_CI_METADATA_DIR/cargo.json"
            --binaries-metadata "$EVORCH_CI_METADATA_DIR/default-binaries.json")
fi
exec cargo nextest run "${selection[@]}" -E "$filter" --no-tests fail "$@"
