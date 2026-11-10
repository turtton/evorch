#!/usr/bin/env bash
# Fixture tests for scripts/verify-test-inventory.sh and the archive mode of
# scripts/run-contract-tests.sh. A fake `cargo` serves `nextest list` JSON
# fixtures, so no workspace build is needed.
set -euo pipefail
cd "$(dirname "$0")/../.."
repo=$PWD

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
bin="$work/bin"
mkdir -p "$bin"

cache_filter=$(scripts/run-contract-tests.sh --print-filter cache)
io_filter=$(scripts/run-contract-tests.sh --print-filter io)
remaining_filter=$(scripts/run-contract-tests.sh --print-filter remaining)

# The fake records every invocation in $FAKE_LOG and answers `nextest list`
# from $FIXTURES: all.json, default.json, cache.json, io.json, remaining.json and
# remaining-K.json, chosen from the arguments the script passes.
cat > "$bin/cargo" <<'FAKE'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$FAKE_LOG"
[[ "$1 $2" == "nextest list" ]] || exit 0
filter=""
partition=""
all=0
args=("$@")
for ((i = 0; i < ${#args[@]}; i++)); do
  case "${args[i]}" in
    -E) filter="${args[i + 1]}" ;;
    --partition) partition="${args[i + 1]}" ;;
    --run-ignored) [[ "${args[i + 1]}" == all ]] && all=1 ;;
  esac
done
if [[ -n "$partition" ]]; then
  k="${partition#hash:}"
  cat "$FIXTURES/remaining-${k%%/*}.json"
elif [[ "$filter" == "$CACHE_FILTER" ]]; then
  cat "$FIXTURES/cache.json"
elif [[ "$filter" == "$IO_FILTER" ]]; then
  cat "$FIXTURES/io.json"
elif [[ "$filter" == "$REMAINING_FILTER" ]]; then
  cat "$FIXTURES/remaining.json"
elif [[ "$all" == 1 ]]; then
  cat "$FIXTURES/all.json"
else
  cat "$FIXTURES/default.json"
fi
FAKE
chmod +x "$bin/cargo"

# listing <test>... : nextest-shaped JSON in which each "binary::name" or
# "binary::name!ignored" argument is a test that matches.
listing() {
  printf '%s\n' "$@" | jq -Rn '
    [inputs | select(length > 0)]
    | map({binary: (split("::")[0]), rest: (split("::")[1:] | join("::"))}
          | .ignored = (.rest | endswith("!ignored"))
          | .name = (.rest | sub("!ignored$"; "")))
    | group_by(.binary)
    | map({key: .[0].binary, value: {
        "package-name": "pkg", "binary-id": .[0].binary, "binary-name": .[0].binary,
        kind: "test", status: "listed",
        testcases: (map({key: .name, value: {kind: "test", ignored: .ignored,
          "filter-match": {status: "matches"}}}) | from_entries)}})
    | {"test-count": (map(.value.testcases | length) | add // 0),
       "rust-suites": from_entries}'
}

pass=0
fail=0
expect() { # expect <name> <ok|fail> <needle or ""> ; reads $status and $output
  local name=$1 want=$2 needle=$3 got=ok
  [[ "$status" == 0 ]] || got=fail
  if [[ "$got" != "$want" ]]; then
    echo "FAIL $name: expected $want, got $got"
    echo "$output" | sed 's/^/    /'
    fail=$((fail + 1))
  elif [[ -n "$needle" && "$output" != *"$needle"* ]]; then
    echo "FAIL $name: output lacks '$needle'"
    echo "$output" | sed 's/^/    /'
    fail=$((fail + 1))
  else
    echo "ok   $name"
    pass=$((pass + 1))
  fi
}

# Fixture sets: cache {c1,c2}, io {i1}, remaining {r1..r4}, one ignored test.
good_fixtures() {
  local dir=$1
  mkdir -p "$dir"
  listing 'a::c1' 'a::c2' 'a::i1' 'b::r1' 'b::r2' 'b::r3' 'b::r4' 'b::ign!ignored' > "$dir/all.json"
  listing 'a::c1' 'a::c2' 'a::i1' 'b::r1' 'b::r2' 'b::r3' 'b::r4' > "$dir/default.json"
  listing 'a::c1' 'a::c2' > "$dir/cache.json"
  listing 'a::i1' > "$dir/io.json"
  listing 'b::r1' 'b::r2' 'b::r3' 'b::r4' > "$dir/remaining.json"
  listing 'b::r1' > "$dir/remaining-1.json"
  listing 'b::r2' > "$dir/remaining-2.json"
  listing 'b::r3' > "$dir/remaining-3.json"
  listing 'b::r4' > "$dir/remaining-4.json"
}

# run_verify <fixture dir> [env assignments...]
run_verify() {
  local dir=$1
  shift
  : > "$work/fake.log"
  status=0
  output=$(env PATH="$bin:$PATH" FIXTURES="$dir" FAKE_LOG="$work/fake.log" \
    CACHE_FILTER="$cache_filter" IO_FILTER="$io_filter" REMAINING_FILTER="$remaining_filter" \
    "$@" scripts/verify-test-inventory.sh --output-dir "$work/out" 2>&1) || status=$?
}

good="$work/good"
good_fixtures "$good"
run_verify "$good"
expect "consistent inventory passes" ok "cache + io + remaining = default run target"
[[ "$(wc -l < "$work/out/cache.tsv")" == 2 && "$(wc -l < "$work/out/remaining-shard-4.tsv")" == 1 ]] \
  || { echo "FAIL tsv outputs have unexpected counts"; fail=$((fail + 1)); }
grep -q $'^pkg\ta\tc1\tfalse$' "$work/out/cache.tsv" \
  || { echo "FAIL cache.tsv lacks the identity line"; fail=$((fail + 1)); }
grep -q $'^pkg\tb\tign\ttrue$' "$work/out/all.tsv" \
  || { echo "FAIL all.tsv lacks the ignored test"; fail=$((fail + 1)); }
if grep -q ign "$work/out/default.tsv"; then
  echo "FAIL default.tsv contains the ignored test"
  fail=$((fail + 1))
fi

# Aggregated test binaries name their tests `<former binary>::<test>`, so the
# test names carry `::` and the same former-binary prefix appears in two suites.
grouped="$work/grouped"
mkdir -p "$grouped"
listing 'a::old_cache::c1' 'a::old_io::i1' 'b::old_one::r1' 'b::old_one::r2' 'b::old_two::r1' 'b::ign!ignored' > "$grouped/all.json"
listing 'a::old_cache::c1' 'a::old_io::i1' 'b::old_one::r1' 'b::old_one::r2' 'b::old_two::r1' > "$grouped/default.json"
listing 'a::old_cache::c1' > "$grouped/cache.json"
listing 'a::old_io::i1' > "$grouped/io.json"
listing 'b::old_one::r1' 'b::old_one::r2' 'b::old_two::r1' > "$grouped/remaining.json"
listing 'b::old_one::r1' > "$grouped/remaining-1.json"
listing 'b::old_one::r2' > "$grouped/remaining-2.json"
listing 'b::old_two::r1' > "$grouped/remaining-3.json"
: > "$grouped/remaining-4.json"
run_verify "$grouped" EVORCH_REMAINING_SHARDS=3
expect "aggregated test names pass" ok "remaining shards partition remaining"
grep -q $'^pkg\tb\told_two::r1\tfalse$' "$work/out/remaining-shard-3.tsv" \
  || { echo "FAIL shard tsv lacks the module-qualified identity"; fail=$((fail + 1)); }

bad="$work/bad"
good_fixtures "$bad"
listing 'a::c1' 'a::i1' > "$bad/io.json"
run_verify "$bad"
expect "a test in both cache and io fails" fail "cache and io share 1 tests"

good_fixtures "$bad"
listing 'a::c1' 'a::c2' 'a::i1' 'b::r1' 'b::r2' 'b::r3' 'b::r4' 'b::lost' > "$bad/default.json"
run_verify "$bad"
expect "a default test outside every suite fails" fail "differs from the default run target"

good_fixtures "$bad"
listing 'b::r1' 'b::r2' 'b::r3' 'b::r4' 'b::extra' > "$bad/remaining.json"
run_verify "$bad"
expect "a remaining test outside the default target fails" fail "differs from the default run target"

good_fixtures "$bad"
listing 'b::r4' > "$bad/remaining-3.json"
run_verify "$bad"
expect "overlapping shards fail" fail "remaining shards 3 and 4 share 1 tests"

good_fixtures "$bad"
listing > "$bad/remaining-2.json"
run_verify "$bad"
expect "an empty shard fails" fail "remaining shard 2/4 has no tests"

good_fixtures "$bad"
listing 'a::c1' 'a::c2' 'a::i1' 'b::r1' 'b::r2' 'b::r3' > "$bad/default.json"
listing 'a::c1' 'a::c2' > "$bad/cache.json"
listing 'a::i1' > "$bad/io.json"
listing 'b::r1' 'b::r2' 'b::r3' > "$bad/remaining.json"
run_verify "$bad"
expect "shards missing a remaining test fail" fail "differ from remaining"

good_fixtures "$bad"
listing 'a::c1' 'a::c2' 'a::i1' 'b::r1' 'b::r2' 'b::r3' 'b::r4' 'b::ign' > "$bad/default.json"
listing 'a::c1' 'a::c2' 'a::i1' 'b::r1' 'b::r2' 'b::r3' 'b::r4' > "$bad/all.json"
run_verify "$bad"
expect "a default test missing from the full listing fails" fail "missing from the full listing"

good_fixtures "$bad"
listing 'a::c1' 'a::c2' 'a::i1' 'b::r1' 'b::r2' 'b::r3' 'b::r4' 'b::ign!ignored' > "$bad/all.json"
listing 'a::c1' 'a::c2' 'a::i1' 'b::r1' 'b::r2' 'b::r3' 'b::r4' 'b::ign!ignored' > "$bad/default.json"
run_verify "$bad"
expect "an ignored test in the default target fails" fail "ignored tests are in the default run target"

# Archive mode: extract once with --archive-file, then reuse the metadata.
printf 'archive' > "$work/ws.tar.zst"
run_verify "$good" EVORCH_NEXTEST_ARCHIVE="$work/ws.tar.zst"
expect "archive mode passes" ok "cache + io + remaining = default run target"
archive_calls=$(grep -c -- '--archive-file' "$work/fake.log" || true)
metadata_calls=$(grep -c -- '--binaries-metadata' "$work/fake.log" || true)
if [[ "$archive_calls" != 1 || "$metadata_calls" != 8 ]]; then
  echo "FAIL archive mode should extract once (got $archive_calls) and reuse metadata 8 times (got $metadata_calls)"
  fail=$((fail + 1))
fi
if grep -q -- '--workspace --locked' "$work/fake.log"; then
  echo "FAIL archive mode invoked a workspace build selection"
  fail=$((fail + 1))
fi

run_verify "$good" EVORCH_NEXTEST_ARCHIVE="$work/missing.tar.zst"
expect "a missing archive fails without rebuilding" fail "does not name a file"
if grep -q -- '--workspace' "$work/fake.log"; then
  echo "FAIL a missing archive fell back to a workspace build"
  fail=$((fail + 1))
fi

# run-contract-tests.sh: archive mode runs the archive, never rebuilds.
run_suite() { # run_suite <suite> [env assignments...]
  local suite=$1
  shift
  : > "$work/fake.log"
  status=0
  output=$(env PATH="$bin:$PATH" FAKE_LOG="$work/fake.log" "$@" scripts/run-contract-tests.sh "$suite" 2>&1) || status=$?
}
run_suite cache EVORCH_NEXTEST_ARCHIVE="$work/ws.tar.zst"
expect "archive mode runs the archive" ok ""
line=$(cat "$work/fake.log")
for want in "nextest run" "--archive-file $work/ws.tar.zst" "--workspace-remap $repo" "--no-tests fail" "-E"; do
  [[ "$line" == *"$want"* ]] || { echo "FAIL archive mode command lacks '$want': $line"; fail=$((fail + 1)); }
done
for unwanted in "--workspace --locked" "--cargo-metadata"; do
  [[ "$line" != *"$unwanted"* ]] || { echo "FAIL archive mode command has '$unwanted': $line"; fail=$((fail + 1)); }
done

run_suite io EVORCH_NEXTEST_ARCHIVE="$work/missing.tar.zst"
expect "run-contract-tests fails on a missing archive" fail "does not name a file"
[[ ! -s "$work/fake.log" ]] || { echo "FAIL a missing archive still invoked cargo"; fail=$((fail + 1)); }

run_suite remaining
expect "local mode builds the workspace" ok ""
line=$(cat "$work/fake.log")
[[ "$line" == *"nextest run --workspace --locked"* && "$line" != *"--archive-file"* ]] \
  || { echo "FAIL local mode command unexpected: $line"; fail=$((fail + 1)); }

run_suite bogus
expect "an unknown suite fails" fail "unknown contract suite"

for suite in cache io remaining; do
  printed=$(scripts/run-contract-tests.sh --print-filter "$suite")
  [[ -n "$printed" ]] || { echo "FAIL --print-filter $suite is empty"; fail=$((fail + 1)); }
done

echo "$pass passed, $fail failed"
[[ "$fail" == 0 ]]
