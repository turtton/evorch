#!/usr/bin/env bash
# Merge gate for the per-group test inventories that the CI test jobs upload
# (scripts/run-test-group.sh inventory).
#
# Usage: scripts/verify-test-inventory.sh [options] <inventories-dir>
#   --config FILE          group definition (default scripts/ci-test-groups.json)
#   --members-file FILE    workspace member names, one per line (default: the
#                          `name` of every crates/*/Cargo.toml)
#   --expected-sha SHA     every inventory must be for this commit
#   --reference-default F  also require the union of the group `default` sets to
#                          equal this file (a whole-workspace `default` listing)
#   --check-config         only validate the config against the members
#   --summary FILE         write a markdown summary there (also appended to
#                          $GITHUB_STEP_SUMMARY)
# <inventories-dir> holds one directory per group shard, `<anything>-<group>-<k>`
# (an artifact download with pattern + merge-multiple false), each with the
# files documented in run-test-group.sh.
#
# Rejected: a missing, extra or duplicated shard; an inventory whose status is
# not ok or whose group/shard/shard count/package list disagrees with the
# config; mixed commits or config hashes; groups that overlap or do not cover the
# workspace members; shards of a group that disagree about the pre-partition
# listing; duplicate rows; tests of a package outside the group; ignored tests
# in `default` (or a default test missing from `all`); cache/io/remaining that
# are not disjoint or do not add up to `default`; an empty suite that is not
# in allow_empty_suites; shard partitions that overlap or do not add up to the
# suite.
set -euo pipefail
cd "$(dirname "$0")/.."

config=scripts/ci-test-groups.json
members_file=""
expected_sha=""
reference=""
check_config=0
summary_file=""
dir=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --config) config="${2:?--config needs a file}"; shift 2 ;;
    --members-file) members_file="${2:?--members-file needs a file}"; shift 2 ;;
    --expected-sha) expected_sha="${2:?--expected-sha needs a SHA}"; shift 2 ;;
    --reference-default) reference="${2:?--reference-default needs a file}"; shift 2 ;;
    --check-config) check_config=1; shift ;;
    --summary) summary_file="${2:?--summary needs a file}"; shift 2 ;;
    -*) echo "unknown option: $1" >&2; exit 2 ;;
    *) dir="$1"; shift ;;
  esac
done
command -v jq > /dev/null || { echo "verify-test-inventory: jq is required" >&2; exit 2; }
[[ "$check_config" == 1 || -n "$dir" ]] || { echo "expected an inventories directory" >&2; exit 2; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
failures=0
fail() {
  echo "FAIL: $*" >&2
  failures=$((failures + 1))
}
count() { wc -l < "$1" | tr -d ' '; }
sorted_cat() { cat "$@" | LC_ALL=C sort; }
common() { LC_ALL=C comm -12 "$1" "$2" | wc -l | tr -d ' '; }

if [[ -z "$members_file" ]]; then
  members_file="$work/members.txt"
  for manifest in crates/*/Cargo.toml; do
    awk -F'"' '/^name *=/ { print $2; exit }' "$manifest"
  done | LC_ALL=C sort > "$members_file"
else
  LC_ALL=C sort "$members_file" > "$work/members.txt"
  members_file="$work/members.txt"
fi

mapfile -t groups < <(jq -r '.groups | keys[]' "$config")
[[ ${#groups[@]} -gt 0 ]] || { echo "FAIL: $config defines no groups" >&2; exit 1; }

# Config: group packages are disjoint, unique, and exactly the workspace members.
: > "$work/config-packages.txt"
for g in "${groups[@]}"; do
  jq -r --arg g "$g" '.groups[$g].packages[]' "$config" | LC_ALL=C sort > "$work/pkgs-$g.txt"
  [[ "$(count "$work/pkgs-$g.txt")" -gt 0 ]] || fail "group $g has no packages"
  jq -e --arg g "$g" '.groups[$g].shards | type == "number" and . >= 1' "$config" > /dev/null \
    || fail "group $g needs a positive shard count"
  cat "$work/pkgs-$g.txt" >> "$work/config-packages.txt"
done
LC_ALL=C sort "$work/config-packages.txt" > "$work/config-sorted.txt"
dups=$(uniq -d "$work/config-sorted.txt" | tr '\n' ' ')
[[ -z "$dups" ]] || fail "packages assigned to more than one group (or twice): $dups"
extra=$(LC_ALL=C comm -23 <(uniq "$work/config-sorted.txt") "$members_file" | tr '\n' ' ')
[[ -z "$extra" ]] || fail "packages in no workspace member: $extra"
missing=$(LC_ALL=C comm -13 <(uniq "$work/config-sorted.txt") "$members_file" | tr '\n' ' ')
[[ -z "$missing" ]] || fail "workspace members in no group: $missing"

suites=(cache io remaining)
total_all=0
if [[ "$check_config" == 0 ]]; then
  [[ -d "$dir" ]] || { echo "FAIL: $dir is not a directory" >&2; exit 1; }
  declare -A shard_dir=()
  for entry in "$dir"/*/; do
    [[ -d "$entry" ]] || continue
    entry="${entry%/}"
    name="${entry##*/}"
    matched=0
    for g in "${groups[@]}"; do
      if [[ "$name" =~ ^(.*-)?"$g"-([1-9][0-9]*)$ ]]; then
        key="$g/${BASH_REMATCH[2]}"
        if [[ -n "${shard_dir[$key]:-}" ]]; then
          fail "inventory for $key appears twice: ${shard_dir[$key]##*/} and $name"
        fi
        shard_dir[$key]="$entry"
        matched=1
        break
      fi
    done
    [[ "$matched" == 1 ]] || fail "unexpected inventory directory: $name"
  done

  shas="" hashes=""
  for g in "${groups[@]}"; do
    n=$(jq -r --arg g "$g" '.groups[$g].shards' "$config")
    ok_shards=()
    for ((k = 1; k <= n; k++)); do
      d="${shard_dir["$g/$k"]:-}"
      if [[ -z "$d" ]]; then
        fail "missing inventory for group $g shard $k/$n"
        continue
      fi
      meta="$d/meta.json"
      if [[ ! -f "$meta" ]] || ! jq -e . "$meta" > /dev/null 2>&1; then
        fail "$g $k/$n: no valid meta.json (the listing did not finish)"
        continue
      fi
      status=$(jq -r '.status // ""' "$meta")
      [[ "$status" == ok ]] || { fail "$g $k/$n: status is '$status'"; continue; }
      [[ "$(jq -r '.group' "$meta")" == "$g" && "$(jq -r '.shard' "$meta")" == "$k" && "$(jq -r '.shards' "$meta")" == "$n" ]] \
        || { fail "$g $k/$n: meta group/shard disagrees with the directory name"; continue; }
      [[ "$(jq -c '.packages | sort' "$meta")" == "$(jq -c --arg g "$g" '.groups[$g].packages | sort' "$config")" ]] \
        || fail "$g $k/$n: packages differ from the config"
      shas+="$(jq -r '.sha' "$meta")"$'\n'
      hashes+="$(jq -r '.config_hash' "$meta")"$'\n'
      if [[ -n "$expected_sha" && "$(jq -r '.sha' "$meta")" != "$expected_sha" ]]; then
        fail "$g $k/$n: inventory is for $(jq -r '.sha' "$meta"), expected $expected_sha"
      fi
      files_ok=1
      for f in all default cache io remaining cache.part io.part remaining.part; do
        if [[ ! -f "$d/$f.tsv" ]]; then
          fail "$g $k/$n: $f.tsv is missing"
          files_ok=0
        fi
      done
      [[ "$files_ok" == 1 ]] && ok_shards+=("$k")
    done
    printf '%s\n' "${ok_shards[@]:-}" | grep -v '^$' > "$work/ok-$g.txt" || true
  done
  [[ "$(printf '%s' "$shas" | grep -v '^$' | sort -u | wc -l)" -le 1 ]] || fail "inventories are for different commits: $(printf '%s' "$shas" | sort -u | tr '\n' ' ')"
  [[ "$(printf '%s' "$hashes" | grep -v '^$' | sort -u | wc -l)" -le 1 ]] || fail "inventories were produced with different configuration hashes"

  : > "$work/all-defaults.tsv"
  : > "$work/all-part-union.tsv"
  summary_rows=""
  for g in "${groups[@]}"; do
    n=$(jq -r --arg g "$g" '.groups[$g].shards' "$config")
    mapfile -t oks < "$work/ok-$g.txt"
    [[ ${#oks[@]} == "$n" ]] || continue # a missing shard was already reported
    first="${shard_dir["$g/1"]}"
    # Every shard must have listed the same whole-group sets.
    for ((k = 2; k <= n; k++)); do
      for f in all default cache io remaining; do
        cmp -s "$first/$f.tsv" "${shard_dir["$g/$k"]}/$f.tsv" \
          || fail "$g: shard $k lists a different $f set than shard 1"
      done
    done
    for f in all default cache io remaining; do
      u=$(sort "$first/$f.tsv" | uniq -d | wc -l | tr -d ' ')
      [[ "$u" == 0 ]] || fail "$g: $u duplicate rows in $f"
      LC_ALL=C sort -c "$first/$f.tsv" 2> /dev/null || fail "$g: $f is not sorted (row order is part of the format)"
      bad=$(awk -F'\t' 'NF != 4 || ($4 != "true" && $4 != "false")' "$first/$f.tsv" | wc -l | tr -d ' ')
      [[ "$bad" == 0 ]] || fail "$g: $bad malformed rows in $f"
      outside=$(awk -F'\t' 'FNR == NR { ok[$1] = 1; next } !($1 in ok)' "$work/pkgs-$g.txt" "$first/$f.tsv" | wc -l | tr -d ' ')
      [[ "$outside" == 0 ]] || fail "$g: $outside rows of $f belong to packages outside the group"
    done
    [[ "$(count "$first/default.tsv")" -gt 0 ]] || fail "$g: default has no tests"
    ignored=$(awk -F'\t' '$4 == "true"' "$first/default.tsv" | wc -l | tr -d ' ')
    [[ "$ignored" == 0 ]] || fail "$g: $ignored ignored tests in default"
    n_missing=$(LC_ALL=C comm -23 "$first/default.tsv" "$first/all.tsv" | wc -l | tr -d ' ')
    [[ "$n_missing" == 0 ]] || fail "$g: $n_missing default tests are missing from all"
    for pair in "cache io" "cache remaining" "io remaining"; do
      set -- $pair
      c=$(common "$first/$1.tsv" "$first/$2.tsv")
      [[ "$c" == 0 ]] || fail "$g: $1 and $2 share $c tests"
    done
    sorted_cat "$first/cache.tsv" "$first/io.tsv" "$first/remaining.tsv" > "$work/union-$g.tsv"
    cmp -s "$work/union-$g.tsv" "$first/default.tsv" \
      || fail "$g: cache + io + remaining ($(count "$work/union-$g.tsv")) differ from default ($(count "$first/default.tsv"))"
    for s in "${suites[@]}"; do
      if [[ "$(count "$first/$s.tsv")" == 0 ]]; then
        jq -e --arg g "$g" --arg s "$s" '.groups[$g].allow_empty_suites | index($s)' "$config" > /dev/null \
          || fail "$g: suite $s is empty and not listed in allow_empty_suites"
      fi
      parts=()
      for ((k = 1; k <= n; k++)); do parts+=("${shard_dir["$g/$k"]}/$s.part.tsv"); done
      for p in "${parts[@]}"; do
        d=$(sort "$p" | uniq -d | wc -l | tr -d ' ')
        [[ "$d" == 0 ]] || fail "$g: $d duplicate rows in ${p##*/}"
      done
      for ((a = 1; a <= n; a++)); do
        for ((b = a + 1; b <= n; b++)); do
          c=$(common "${shard_dir["$g/$a"]}/$s.part.tsv" "${shard_dir["$g/$b"]}/$s.part.tsv")
          [[ "$c" == 0 ]] || fail "$g: shards $a and $b both run $c $s tests"
        done
      done
      sorted_cat "${parts[@]}" > "$work/partunion-$g-$s.tsv"
      cmp -s "$work/partunion-$g-$s.tsv" "$first/$s.tsv" \
        || fail "$g: the $s shards run $(count "$work/partunion-$g-$s.tsv") tests, the suite has $(count "$first/$s.tsv")"
      cat "$work/partunion-$g-$s.tsv" >> "$work/all-part-union.tsv"
    done
    cat "$first/default.tsv" >> "$work/all-defaults.tsv"
    summary_rows+="| $g | $n | $(count "$first/all.tsv") | $(count "$first/default.tsv") | $(count "$first/cache.tsv") | $(count "$first/io.tsv") | $(count "$first/remaining.tsv") |"$'\n'
  done
  LC_ALL=C sort "$work/all-defaults.tsv" > "$work/defaults-sorted.tsv"
  LC_ALL=C sort "$work/all-part-union.tsv" > "$work/parts-sorted.tsv"
  total_all=$(count "$work/defaults-sorted.tsv")
  [[ "$(uniq -d "$work/defaults-sorted.tsv" | wc -l | tr -d ' ')" == 0 ]] || fail "a test is in the default set of more than one group"
  cmp -s "$work/defaults-sorted.tsv" "$work/parts-sorted.tsv" \
    || fail "executed tests ($(count "$work/parts-sorted.tsv")) differ from the union of the group default sets ($total_all)"
  if [[ -n "$reference" ]]; then
    LC_ALL=C sort "$reference" > "$work/reference-sorted.tsv"
    cmp -s "$work/defaults-sorted.tsv" "$work/reference-sorted.tsv" \
      || fail "union of group defaults ($total_all) differs from the reference ($(count "$work/reference-sorted.tsv"))"
  fi
  {
    echo "### Test inventory merge"
    echo
    echo "| group | shards | all | default | cache | io | remaining |"
    echo "| --- | ---: | ---: | ---: | ---: | ---: | ---: |"
    printf '%s' "$summary_rows"
    echo
    echo "union of group defaults: $total_all tests (sha256 $(sha256sum "$work/defaults-sorted.tsv" | cut -c1-12))"
    echo
    if [[ "$failures" == 0 ]]; then
      echo "Every group and shard is present for one commit; the groups cover the workspace members; the shards of each suite partition it; cache + io + remaining = default."
    else
      echo "**$failures invariant(s) failed.**"
    fi
  } > "$work/summary.md"
  cat "$work/summary.md"
  [[ -z "$summary_file" ]] || cp "$work/summary.md" "$summary_file"
  [[ -z "${GITHUB_STEP_SUMMARY:-}" ]] || cat "$work/summary.md" >> "$GITHUB_STEP_SUMMARY"
fi
if [[ "$failures" != 0 ]]; then
  echo "$failures invariant(s) failed" >&2
  exit 1
fi
[[ "$check_config" == 0 ]] || echo "config ok: ${#groups[@]} groups cover $(count "$members_file") workspace members"
