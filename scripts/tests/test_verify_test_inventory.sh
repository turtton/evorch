#!/usr/bin/env bash
# Fixture tests for scripts/verify-test-inventory.sh (the merge gate of the
# per-group CI test inventories). No cargo or build is involved: the fixtures
# are hand-written inventory directories.
set -euo pipefail
cd "$(dirname "$0")/../.."
verify="$PWD/scripts/verify-test-inventory.sh"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

cat > "$work/config.json" <<'JSON'
{"groups": {
  "a": {"shards": 2, "packages": ["pa1", "pa2"], "features": [], "allow_empty_suites": []},
  "b": {"shards": 1, "packages": ["pb"], "features": [], "allow_empty_suites": ["io"]}
}}
JSON
printf '%s\n' pa1 pa2 pb > "$work/members.txt"

tab() { printf '%s\t%s\t%s\t%s\n' "$@"; }

# write_meta <dir> <group> <shard> <shards> <sha> <packages-json>
write_meta() {
  jq -n --arg g "$2" --argjson k "$3" --argjson n "$4" --arg sha "$5" --argjson p "$6" \
    '{status: "ok", group: $g, shard: $k, shards: $n, sha: $sha, config_hash: "h1", packages: $p, features: []}' > "$1/meta.json"
}

# split_parts <dir> <shards>: shard k gets rows k, k+N, ... of each suite.
split_parts() {
  local dir=$1 n=$2 k s
  for s in cache io remaining; do
    for ((k = 1; k <= n; k++)); do
      awk -v k="$k" -v n="$n" '(NR - 1) % n == k - 1' "$dir/$s.tsv" > "$work/part-$k-$s"
    done
  done
}

# A valid inventory tree in $1.
make_valid() {
  local root=$1 d k
  rm -rf "$root"
  mkdir -p "$root"
  # group a: two shards
  d="$root/inv-a-1"
  mkdir -p "$d"
  {
    tab pa1 pa1 t1 false
    tab pa1 pa1 t2 false
    tab pa2 pa2 t3 false
    tab pa2 pa2 t4 false
  } | LC_ALL=C sort > "$d/default.tsv"
  { cat "$d/default.tsv"; tab pa1 pa1 ign true; } | LC_ALL=C sort > "$d/all.tsv"
  tab pa1 pa1 t1 false > "$d/cache.tsv"
  tab pa2 pa2 t3 false > "$d/io.tsv"
  { tab pa1 pa1 t2 false; tab pa2 pa2 t4 false; } | LC_ALL=C sort > "$d/remaining.tsv"
  split_parts "$d" 2
  mkdir -p "$root/inv-a-2"
  for f in all default cache io remaining; do cp "$d/$f.tsv" "$root/inv-a-2/$f.tsv"; done
  for k in 1 2; do
    for s in cache io remaining; do cp "$work/part-$k-$s" "$root/inv-a-$k/$s.part.tsv"; done
    write_meta "$root/inv-a-$k" a "$k" 2 deadbeef '["pa1","pa2"]'
  done
  # group b: one shard, io is empty and allowed
  d="$root/inv-b-1"
  mkdir -p "$d"
  { tab pb pb b1 false; tab pb pb b2 false; } > "$d/default.tsv"
  cp "$d/default.tsv" "$d/all.tsv"
  tab pb pb b1 false > "$d/cache.tsv"
  : > "$d/io.tsv"
  tab pb pb b2 false > "$d/remaining.tsv"
  for s in cache io remaining; do cp "$d/$s.tsv" "$d/$s.part.tsv"; done
  write_meta "$d" b 1 1 deadbeef '["pb"]'
}

run_verify() { # run_verify <root> [extra args...] -> status; output in $work/out
  local root=$1
  shift
  "$verify" --config "$work/config.json" --members-file "$work/members.txt" "$@" "$root" > "$work/out" 2>&1
}

expect_pass() { # expect_pass <name> <root> [args...]
  local name=$1
  shift
  if ! run_verify "$@"; then
    echo "FAIL ($name): expected success" >&2
    cat "$work/out" >&2
    exit 1
  fi
  echo "ok: $name"
}

expect_fail() { # expect_fail <name> <message regexp> <root> [args...]
  local name=$1 pattern=$2
  shift 2
  if run_verify "$@"; then
    echo "FAIL ($name): expected the merge gate to reject this" >&2
    cat "$work/out" >&2
    exit 1
  fi
  if ! grep -Eq "$pattern" "$work/out"; then
    echo "FAIL ($name): rejected, but not with /$pattern/:" >&2
    cat "$work/out" >&2
    exit 1
  fi
  echo "ok: $name"
}

t="$work/t"

make_valid "$t"
expect_pass "valid inventories merge" "$t"
cat "$t/inv-a-1/default.tsv" "$t/inv-b-1/default.tsv" > "$work/ref.tsv"
expect_pass "reference default equals the union" "$t" --reference-default "$work/ref.tsv"
echo "$(tab pa1 pa1 extra false)" >> "$work/ref.tsv"
expect_fail "reference default with an extra test" "differs from the reference" "$t" --reference-default "$work/ref.tsv"
expect_pass "expected sha matches" "$t" --expected-sha deadbeef
expect_fail "expected sha differs" "expected cafe" "$t" --expected-sha cafe

# --- config vs workspace members -------------------------------------------
make_valid "$t"
jq '.groups.b.packages += ["pa1"]' "$work/config.json" > "$work/config-dup.json"
if "$verify" --config "$work/config-dup.json" --members-file "$work/members.txt" --check-config > "$work/out" 2>&1; then
  echo "FAIL (package in two groups): expected rejection" >&2; exit 1
fi
grep -q "more than one group" "$work/out" || { echo "FAIL (package in two groups): wrong message" >&2; cat "$work/out" >&2; exit 1; }
echo "ok: package assigned to two groups"

printf '%s\n' pa1 pa2 pb pnew > "$work/members-new.txt"
if "$verify" --config "$work/config.json" --members-file "$work/members-new.txt" --check-config > "$work/out" 2>&1; then
  echo "FAIL (new member): expected rejection" >&2; exit 1
fi
grep -q "workspace members in no group: pnew" "$work/out" || { echo "FAIL (new member): wrong message" >&2; cat "$work/out" >&2; exit 1; }
echo "ok: workspace member added without a group"

jq '.groups.b.packages += ["pghost"]' "$work/config.json" > "$work/config-ghost.json"
if "$verify" --config "$work/config-ghost.json" --members-file "$work/members.txt" --check-config > "$work/out" 2>&1; then
  echo "FAIL (ghost package): expected rejection" >&2; exit 1
fi
grep -q "packages in no workspace member: pghost" "$work/out" || { echo "FAIL (ghost package): wrong message" >&2; cat "$work/out" >&2; exit 1; }
echo "ok: group package that is not a workspace member"

"$verify" --check-config > "$work/out" 2>&1 || { echo "FAIL: the repository config does not match the workspace" >&2; cat "$work/out" >&2; exit 1; }
echo "ok: repository config covers the workspace members ($(tail -1 "$work/out"))"

# --- shards and artifacts ----------------------------------------------------
make_valid "$t"; rm -rf "$t/inv-a-2"
expect_fail "shard missing" "missing inventory for group a shard 2/2" "$t"

make_valid "$t"; rm -rf "$t/inv-b-1"
expect_fail "group missing" "missing inventory for group b shard 1/1" "$t"

make_valid "$t"; cp -r "$t/inv-a-1" "$t/rerun-a-1"
expect_fail "duplicate shard artifact" "inventory for a/1 appears twice" "$t"

make_valid "$t"; cp -r "$t/inv-a-2" "$t/inv-a-3"
expect_fail "shard beyond the configured count" "inventory inv-a-3 is for shard 3 but group a has only 2 shards" "$t"

make_valid "$t"; mkdir "$t/inv-zzz-1"
expect_fail "unknown group artifact" "unexpected inventory directory" "$t"

make_valid "$t"; rm "$t/inv-b-1/meta.json"
expect_fail "listing error: no meta.json" "b 1/1: no valid meta.json" "$t"

make_valid "$t"; jq '.status = "failed"' "$t/inv-b-1/meta.json" > "$work/m" && mv "$work/m" "$t/inv-b-1/meta.json"
expect_fail "listing error: status failed" "status is 'failed'" "$t"

make_valid "$t"; rm "$t/inv-a-1/io.part.tsv"
expect_fail "partition file missing" "io.part.tsv is missing" "$t"

make_valid "$t"; jq '.sha = "cafe"' "$t/inv-b-1/meta.json" > "$work/m" && mv "$work/m" "$t/inv-b-1/meta.json"
expect_fail "different commit in one shard" "different commits" "$t"

make_valid "$t"; jq '.config_hash = "h2"' "$t/inv-a-2/meta.json" > "$work/m" && mv "$work/m" "$t/inv-a-2/meta.json"
expect_fail "different configuration hash" "different configuration hashes" "$t"

make_valid "$t"; jq '.packages += ["pb"]' "$t/inv-a-1/meta.json" > "$work/m" && mv "$work/m" "$t/inv-a-1/meta.json"
expect_fail "meta packages differ from the config" "packages differ from the config" "$t"

# --- inventory contents ------------------------------------------------------
make_valid "$t"
for f in default cache; do tab pa1 pa1 flaky true >> "$t/inv-a-1/$f.tsv"; LC_ALL=C sort -o "$t/inv-a-1/$f.tsv" "$t/inv-a-1/$f.tsv"; cp "$t/inv-a-1/$f.tsv" "$t/inv-a-2/$f.tsv"; done
tab pa1 pa1 flaky true >> "$t/inv-a-1/all.tsv"; LC_ALL=C sort -o "$t/inv-a-1/all.tsv" "$t/inv-a-1/all.tsv"; cp "$t/inv-a-1/all.tsv" "$t/inv-a-2/all.tsv"
expect_fail "ignored test mixed into default" "ignored tests in default" "$t"

make_valid "$t"
for k in 1 2; do : > "$t/inv-a-$k/remaining.tsv"; : > "$t/inv-a-$k/remaining.part.tsv"; done
expect_fail "a whole suite vanished" "remaining shards run 0|cache \+ io \+ remaining" "$t"

make_valid "$t"
for k in 1 2; do
  : > "$t/inv-a-$k/io.tsv"; : > "$t/inv-a-$k/io.part.tsv"
  grep -v 'pa2	pa2	t3' "$t/inv-a-$k/default.tsv" > "$work/d" || true; mv "$work/d" "$t/inv-a-$k/default.tsv"
  grep -v 'pa2	pa2	t3' "$t/inv-a-$k/all.tsv" > "$work/d" || true; mv "$work/d" "$t/inv-a-$k/all.tsv"
done
expect_fail "empty suite that is not allowed" "suite io is empty and not listed in allow_empty_suites" "$t"

make_valid "$t"
for k in 1 2; do : > "$t/inv-a-$k/default.tsv"; done
expect_fail "default vanished" "default has no tests" "$t"

make_valid "$t"; tab pa1 pa1 t1 false >> "$t/inv-a-1/cache.tsv"
expect_fail "duplicate row" "duplicate rows in cache" "$t"

make_valid "$t"
for k in 1 2; do tab pb pb stray false >> "$t/inv-a-$k/all.tsv"; LC_ALL=C sort -o "$t/inv-a-$k/all.tsv" "$t/inv-a-$k/all.tsv"; done
expect_fail "test of a package outside the group" "belong to packages outside the group" "$t"

make_valid "$t"
for k in 1 2; do tab pa2 pa2 t3 false >> "$t/inv-a-$k/remaining.tsv"; LC_ALL=C sort -o "$t/inv-a-$k/remaining.tsv" "$t/inv-a-$k/remaining.tsv"; done
expect_fail "a test in two suites" "io and remaining share 1" "$t"

make_valid "$t"; tab pa2 pa2 t5 false >> "$t/inv-a-2/default.tsv"; LC_ALL=C sort -o "$t/inv-a-2/default.tsv" "$t/inv-a-2/default.tsv"
expect_fail "shards disagree about the pre-partition listing" "shard 2 lists a different default set" "$t"

# --- shard partitions --------------------------------------------------------
make_valid "$t"; cat "$t/inv-a-1/remaining.part.tsv" >> "$t/inv-a-2/remaining.part.tsv"; LC_ALL=C sort -o "$t/inv-a-2/remaining.part.tsv" "$t/inv-a-2/remaining.part.tsv"
expect_fail "a test runs in two shards" "shards 1 and 2 both run 1 remaining" "$t"

make_valid "$t"; : > "$t/inv-a-1/remaining.part.tsv"
expect_fail "a shard drops its tests" "the remaining shards run 1 tests, the suite has 2" "$t"

make_valid "$t"; : > "$t/inv-b-1/remaining.part.tsv"
expect_fail "a single shard group drops a test" "the remaining shards run 0 tests" "$t"

make_valid "$t"; tab pa1 pa1 phantom false >> "$t/inv-a-2/cache.part.tsv"; LC_ALL=C sort -o "$t/inv-a-2/cache.part.tsv" "$t/inv-a-2/cache.part.tsv"
expect_fail "a shard runs a test that is not in the suite" "the cache shards run 2 tests" "$t"

echo "all verify-test-inventory fixture tests passed"
