#!/usr/bin/env bash
# Use the same workspace feature union for gates and the remaining tests.
#
# Usage: scripts/run-contract-tests.sh <cache|io|remaining> [nextest run args...]
#        scripts/run-contract-tests.sh --print-filter <cache|io|remaining>
#
# The suites partition every default (non-ignored) test:
#   cache     = C
#   io        = I - C
#   remaining = all - (C | I)
#
# Build selection:
#   default       cargo builds the workspace with `--workspace --locked`.
#   archive mode  EVORCH_NEXTEST_ARCHIVE=<file.tar.zst> runs the tests of a
#                 `cargo nextest archive --workspace --locked` archive. Cargo
#                 and the feature union are not involved, and a missing archive
#                 is an error: it never falls back to rebuilding.
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/contract-test-filters.sh

print_filter=0
if [[ "${1:-}" == "--print-filter" ]]; then
  print_filter=1
  shift
fi
suite="${1:?expected cache, io, or remaining}"
shift
case "$suite" in
  cache) filter="$CACHE_CONTRACT_FILTER" ;;
  io) filter="($IO_CONTRACT_FILTER) & not ($CACHE_CONTRACT_FILTER)" ;;
  remaining) filter="not (($CACHE_CONTRACT_FILTER) | ($IO_CONTRACT_FILTER))" ;;
  *) echo "unknown contract suite: $suite" >&2; exit 2 ;;
esac
if [[ "$print_filter" == 1 ]]; then
  printf '%s\n' "$filter"
  exit 0
fi

if [[ -n "${EVORCH_NEXTEST_ARCHIVE:-}" ]]; then
  if [[ ! -f "$EVORCH_NEXTEST_ARCHIVE" ]]; then
    echo "EVORCH_NEXTEST_ARCHIVE does not name a file: $EVORCH_NEXTEST_ARCHIVE" >&2
    exit 2
  fi
  # The archive fixes the package selection, features and profile; the
  # workspace is remapped to this checkout so CARGO_MANIFEST_DIR-relative test
  # fixtures resolve here.
  selection=(--archive-file "$EVORCH_NEXTEST_ARCHIVE" --workspace-remap "$PWD")
else
  selection=(--workspace --locked)
fi
exec cargo nextest run "${selection[@]}" -E "$filter" --no-tests fail "$@"
