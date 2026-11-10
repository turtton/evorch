#!/usr/bin/env bash
# Run (or list) one of the CI test suites.
#
# Usage: scripts/run-contract-tests.sh [--group NAME] [--shard K/N] [--list] <suite> [nextest args...]
#        scripts/run-contract-tests.sh --print-filter <suite>
#
# Suites partition every default (non-ignored) test:
#   cache     = C
#   io        = I - C
#   remaining = all - (C | I)
#   default   = every non-ignored test (list / print-filter only)
#   all       = every test including ignored ones (--list only)
#
# Selection:
#   no --group   `--workspace --locked` (all packages, the workspace feature union).
#   --group G    the `-p` packages and `-F` features of group G in
#                scripts/ci-test-groups.json. Every shard of a group builds the
#                same packages, features and profile; --shard K/N only adds
#                `--partition hash:K/N` to the run, so the shards of a group
#                partition the group's tests.
#   --list       `cargo nextest list --message-format json` with exactly the
#                arguments the run would use (the inventory source for CI).
# EVORCH_CI_METADATA_DIR (workspace mode only): reuse binaries listed in
# $EVORCH_CI_METADATA_DIR/{cargo,default-binaries}.json without invoking Cargo.
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/contract-test-filters.sh
groups_file=scripts/ci-test-groups.json

group=""
shard=""
mode=run
while [[ $# -gt 0 ]]; do
  case "$1" in
    --group) group="${2:?--group needs a group name}"; shift 2 ;;
    --shard) shard="${2:?--shard needs K/N}"; shift 2 ;;
    --list) mode=list; shift ;;
    --print-filter) mode=print-filter; shift ;;
    *) break ;;
  esac
done
suite="${1:?expected cache, io, or remaining}"
shift

extra=()
case "$suite" in
  cache) filter="$CACHE_CONTRACT_FILTER" ;;
  io) filter="($IO_CONTRACT_FILTER) & not ($CACHE_CONTRACT_FILTER)" ;;
  remaining) filter="not (($CACHE_CONTRACT_FILTER) | ($IO_CONTRACT_FILTER))" ;;
  default) filter="" ;;
  all) filter=""; extra=(--run-ignored all) ;;
  *) echo "unknown contract suite: $suite" >&2; exit 2 ;;
esac
if [[ "$mode" == print-filter ]]; then
  printf '%s\n' "$filter"
  exit 0
fi
if [[ "$mode" == run && ( "$suite" == default || "$suite" == all ) ]]; then
  echo "suite $suite can only be listed (--list)" >&2
  exit 2
fi

if [[ -n "$group" ]]; then
  jq -e --arg g "$group" '.groups[$g]' "$groups_file" > /dev/null \
    || { echo "unknown test group: $group (see $groups_file)" >&2; exit 2; }
  selection=(--locked)
  while read -r p; do selection+=(-p "$p"); done \
    < <(jq -r --arg g "$group" '.groups[$g].packages[]' "$groups_file")
  while read -r f; do selection+=(-F "$f"); done \
    < <(jq -r --arg g "$group" '.groups[$g].features[]?' "$groups_file")
elif [[ -n "${EVORCH_CI_METADATA_DIR:-}" ]]; then
  # Metadata is generated in this checkout immediately before execution. These
  # flags reuse binaries without invoking Cargo or changing the feature union.
  selection=(--cargo-metadata "$EVORCH_CI_METADATA_DIR/cargo.json"
            --binaries-metadata "$EVORCH_CI_METADATA_DIR/default-binaries.json")
else
  selection=(--workspace --locked)
fi
if [[ -n "$shard" ]]; then
  [[ "$shard" =~ ^[1-9][0-9]*/[1-9][0-9]*$ ]] || { echo "--shard must look like K/N: $shard" >&2; exit 2; }
  extra+=(--partition "hash:$shard")
fi
if [[ -n "$filter" ]]; then
  extra+=(-E "$filter")
fi

if [[ "$mode" == list ]]; then
  exec cargo nextest list "${selection[@]}" "${extra[@]}" --message-format json "$@"
fi
exec cargo nextest run "${selection[@]}" "${extra[@]}" --no-tests fail "$@"
