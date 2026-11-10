#!/usr/bin/env bash
# Verify the test inventory behind the CI suites (cache, io, remaining and the
# remaining shards) with `cargo nextest list --message-format json`.
#
# Invariants checked on every run:
#   * cache, io and remaining are non-empty, pairwise disjoint, and their union
#     is exactly the default run target (every non-ignored test);
#   * the remaining shards (--partition hash:k/N) are non-empty, pairwise
#     disjoint, and their union is exactly remaining;
#   * every default test is also in the full listing (--run-ignored all).
#
# The suite filters come from scripts/run-contract-tests.sh --print-filter, the
# same expressions the suites execute, so this cannot drift from them.
#
# Usage: scripts/verify-test-inventory.sh [--output-dir DIR]
#   EVORCH_NEXTEST_ARCHIVE=<file.tar.zst>  list the archive (extracted once)
#                                          instead of building the workspace.
#   EVORCH_REMAINING_SHARDS=N              number of remaining shards (default 4).
#   DIR receives sorted `package<TAB>binary-id<TAB>test<TAB>ignored` files
#   (all, default, cache, io, remaining, remaining-shard-K) plus summary.md, so
#   two commits can be compared with `diff -r`.
set -euo pipefail
cd "$(dirname "$0")/.."

shards="${EVORCH_REMAINING_SHARDS:-4}"
out=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --output-dir) out="${2:?--output-dir needs a directory}"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
command -v jq >/dev/null || { echo "verify-test-inventory: jq is required" >&2; exit 2; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
out="${out:-$work/out}"
mkdir -p "$out"

extract_dir="$work/extract"
mkdir -p "$extract_dir"
extracted=0
# nextest_list <args...>: JSON listing on stdout. An archive is extracted by the
# first call and its metadata reused afterwards, so the archive is unpacked once.
nextest_list() {
  if [[ -z "${EVORCH_NEXTEST_ARCHIVE:-}" ]]; then
    cargo nextest list --workspace --locked --message-format json "$@"
  elif [[ "$extracted" == 0 ]]; then
    [[ -f "$EVORCH_NEXTEST_ARCHIVE" ]] || {
      echo "EVORCH_NEXTEST_ARCHIVE does not name a file: $EVORCH_NEXTEST_ARCHIVE" >&2
      return 2
    }
    cargo nextest list --archive-file "$EVORCH_NEXTEST_ARCHIVE" --workspace-remap "$PWD" \
      --extract-to "$extract_dir" --message-format json "$@"
  else
    cargo nextest list --cargo-metadata "$extract_dir/target/nextest/cargo-metadata.json" \
      --binaries-metadata "$extract_dir/target/nextest/binaries-metadata.json" \
      --target-dir-remap "$extract_dir/target" --workspace-remap "$PWD" \
      --message-format json "$@"
  fi
}

# Sorted identity lines of the tests nextest would run (filter-match status
# "matches"), read from a listing on stdin.
select_tests() {
  jq -r '
    .["rust-suites"][] | . as $suite
    | .testcases | to_entries[]
    | select(.value["filter-match"].status == "matches")
    | [$suite["package-name"], $suite["binary-id"], .key, (.value.ignored | tostring)]
    | @tsv' | LC_ALL=C sort
}

list_to() { # list_to <file> <nextest list args...>
  local file=$1
  shift
  nextest_list "$@" > "$work/list.json"
  extracted=1
  select_tests < "$work/list.json" > "$file"
}

list_to "$out/all.tsv" --run-ignored all
list_to "$out/default.tsv"
for suite in cache io remaining; do
  filter=$(scripts/run-contract-tests.sh --print-filter "$suite")
  list_to "$out/$suite.tsv" -E "$filter"
done
remaining_filter=$(scripts/run-contract-tests.sh --print-filter remaining)
for ((k = 1; k <= shards; k++)); do
  list_to "$out/remaining-shard-$k.tsv" -E "$remaining_filter" --partition "hash:$k/$shards"
done

failures=0
fail() {
  echo "FAIL: $*" >&2
  failures=$((failures + 1))
}
count() { wc -l < "$1" | tr -d ' '; }
common() { LC_ALL=C comm -12 "$1" "$2" | wc -l | tr -d ' '; }
sorted_cat() { cat "$@" | LC_ALL=C sort; }

for f in all default cache io remaining; do
  [[ "$(count "$out/$f.tsv")" -gt 0 ]] || fail "$f has no tests"
done
for pair in "cache io" "cache remaining" "io remaining"; do
  set -- $pair
  n=$(common "$out/$1.tsv" "$out/$2.tsv")
  [[ "$n" == 0 ]] || fail "$1 and $2 share $n tests"
done
sorted_cat "$out/cache.tsv" "$out/io.tsv" "$out/remaining.tsv" > "$work/suites-union.tsv"
if ! cmp -s "$work/suites-union.tsv" "$out/default.tsv"; then
  fail "cache + io + remaining differs from the default run target ($(count "$work/suites-union.tsv") vs $(count "$out/default.tsv"))"
  LC_ALL=C comm -3 "$work/suites-union.tsv" "$out/default.tsv" | head -20 >&2
fi
n=$(LC_ALL=C comm -23 "$out/default.tsv" "$out/all.tsv" | wc -l | tr -d ' ')
[[ "$n" == 0 ]] || fail "$n default tests are missing from the full listing"
n=$(grep -c $'\ttrue$' "$out/default.tsv" || true)
[[ "$n" == 0 ]] || fail "$n ignored tests are in the default run target"

shard_files=()
for ((k = 1; k <= shards; k++)); do
  shard_files+=("$out/remaining-shard-$k.tsv")
  [[ "$(count "$out/remaining-shard-$k.tsv")" -gt 0 ]] || fail "remaining shard $k/$shards has no tests"
done
for ((a = 1; a <= shards; a++)); do
  for ((b = a + 1; b <= shards; b++)); do
    n=$(common "$out/remaining-shard-$a.tsv" "$out/remaining-shard-$b.tsv")
    [[ "$n" == 0 ]] || fail "remaining shards $a and $b share $n tests"
  done
done
sorted_cat "${shard_files[@]}" > "$work/shards-union.tsv"
cmp -s "$work/shards-union.tsv" "$out/remaining.tsv" \
  || fail "remaining shards ($(count "$work/shards-union.tsv")) differ from remaining ($(count "$out/remaining.tsv"))"

{
  echo "### Test inventory"
  echo
  echo "| set | tests | sha256 (first 12) |"
  echo "| --- | ---: | --- |"
  for f in all default cache io remaining; do
    echo "| $f | $(count "$out/$f.tsv") | $(sha256sum "$out/$f.tsv" | cut -c1-12) |"
  done
  for ((k = 1; k <= shards; k++)); do
    echo "| remaining shard $k/$shards | $(count "$out/remaining-shard-$k.tsv") | $(sha256sum "$out/remaining-shard-$k.tsv" | cut -c1-12) |"
  done
  echo
  if [[ "$failures" == 0 ]]; then
    echo "cache + io + remaining = default run target; remaining shards partition remaining."
  else
    echo "**$failures invariant(s) failed.**"
  fi
} > "$out/summary.md"
cat "$out/summary.md"
if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
  cat "$out/summary.md" >> "$GITHUB_STEP_SUMMARY"
fi
[[ "$failures" == 0 ]]
