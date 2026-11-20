#!/usr/bin/env bash
# Build -> bind -> execute the same nextest metadata. No cargo invocation occurs
# after binding, so a caller hash cannot silently become stale before a test.
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_BUILD_GC_ENABLED=false
die() { echo "guard-cargo: $*" >&2; exit 1; }
prepare() {
  local dir=$1; shift
  mkdir -p "$dir"
  cargo metadata --locked --format-version 1 >"$dir/cargo.json"
  cargo nextest list "$@" --list-type binaries-only --message-format json >"$dir/default-binaries.json"
}
case "${1:-}" in
  prepare) shift; prepare "$@"; exit 0;;
  run) shift;;
  *) die 'prepare DIR [Cargo selection...] | run [nextest arguments...]';;
esac
selection=() execution=() metadata='' binaries=''
while [[ $# -gt 0 ]]; do
  case "$1" in
    --cargo-metadata) metadata=$2;shift 2;;
    --binaries-metadata) binaries=$2;shift 2;;
    --package|-p|--features|-F|--test|--bin|--example|--bench|--exclude|--target|--cargo-profile|--manifest-path)
      selection+=("$1" "$2");shift 2;;
    --workspace|--locked|--offline|--frozen|--release|--all-features|--no-default-features|--lib|--bins|--tests|--all-targets)
      selection+=("$1");shift;;
    --) execution+=("$@");break;;
    --partition|--no-tests|--run-ignored|--success-output|--failure-output|--status-level|--final-status-level|--message-format|--test-threads|-j|--retries|--profile|-E|--filterset)
      execution+=("$1" "$2");shift 2;;
    *) execution+=("$1");shift;;
  esac
done
if [[ "$(uname -s)" == Linux ]]; then
  bundle=${EVORCH_PROCESS_GUARD_BUNDLE:?prepare with scripts/guard-artifact.sh prepare --profile release --out DIR}
  scripts/guard-artifact.sh verify "$bundle"
fi
if [[ -z "$metadata" && -z "$binaries" ]]; then
  work=$(mktemp -d); trap 'rm -rf "$work"' EXIT
  prepare "$work" "${selection[@]}"
  metadata="$work/cargo.json"; binaries="$work/default-binaries.json"
elif [[ -z "$metadata" || -z "$binaries" ]]; then
  die 'both cargo and binaries metadata are required'
fi
if [[ "$(uname -s)" == Linux ]]; then
  scripts/guard-artifact.sh bind-callers --bundle "$bundle" --build-metadata "$binaries"
  scripts/guard-artifact.sh smoke "$bundle"
fi
# Do not exec: the EXIT trap owns the temporary metadata, not the test runner.
cargo nextest run --cargo-metadata "$metadata" --binaries-metadata "$binaries" "${execution[@]}"
