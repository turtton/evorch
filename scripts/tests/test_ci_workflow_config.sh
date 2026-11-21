#!/usr/bin/env bash
# Static regression tests of .github/workflows/ci.yml against
# scripts/ci-test-groups.json. No cargo, no network.
#
# Two contracts are checked, neither as a hard-coded count:
#  1. The `tests` matrix repeats the groups and shard counts of
#     scripts/ci-test-groups.json: the same groups, every shard 1..N exactly once,
#     the shard count of each cell equal to the JSON, and the CEF attribute set
#     exactly for the groups that build `gui`. (run-test-group.sh and the merge
#     gate of `ci` only catch a drift once a run has started.)
#  2. A job that installs the sealed product artifact reaches it in one of two
#     ways: it `needs: product-artifact` (the cache-save job), or it is a
#     consumer that starts next to the producer, waits for the producer's
#     success with scripts/wait-product-artifact.sh, and holds exactly the
#     read-only permissions that wait needs.
#  3. The producer's upload of `product-core` replaces an older same-name artifact
#     (`overwrite: true`), so a re-run after a failed attempt cannot leave two.
# Each check also fails on a mutated copy of the real files.
# shellcheck disable=SC2016 # the workflow expression is matched literally
set -euo pipefail
cd "$(dirname "$0")/../.."
work=$(mktemp -d "${TMPDIR:-/tmp}/test-ci-workflow-config.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT

# Writes one file per job (comment lines dropped) into the output directory.
split_jobs() { # <ci.yml> <outdir>
  mkdir -p "$2"
  awk -v dir="$2" '
    /^jobs:/ { in_jobs = 1; next }
    /^[^[:space:]]/ { in_jobs = 0 }
    in_jobs && /^  [a-z0-9-]+:[[:space:]]*$/ { name = $1; sub(/:$/, "", name); out = dir "/" name; printf "" > out; next }
    in_jobs && out && $0 !~ /^[[:space:]]*#/ { print > out }
  ' "$1"
}

check_matrix() { # <ci.yml> <groups.json>
  local ci=$1 groups=$2 cells group n want_cef bad=0
  cells=$(sed -n 's/^ *- { group: \([a-z-]*\), shard: \([0-9]*\), shards: \([0-9]*\), cef: \(true\|false\) }$/\1 \2 \3 \4/p' "$ci")
  [[ -n "$cells" ]] || { echo "no tests matrix cells found" >&2; return 1; }
  if [[ "$(awk '{ print $1 }' <<< "$cells" | sort -u)" != "$(jq -r '.groups | keys[]' "$groups" | sort)" ]]; then
    echo "matrix groups differ from $groups" >&2
    bad=1
  fi
  while read -r group; do
    n=$(jq -r --arg g "$group" '.groups[$g].shards' "$groups")
    if [[ "$(awk -v g="$group" '$1 == g { print $2 }' <<< "$cells" | sort -n | tr '\n' ' ')" != "$(seq -s ' ' 1 "$n") " ]]; then
      echo "group $group: shards are not exactly 1..$n" >&2
      bad=1
    fi
    if [[ "$(awk -v g="$group" '$1 == g { print $3 }' <<< "$cells" | sort -u | tr '\n' ' ')" != "$n " ]]; then
      echo "group $group: a cell's shard count is not $n" >&2
      bad=1
    fi
    want_cef=false
    if jq -e --arg g "$group" '.groups[$g].packages | index("gui")' "$groups" > /dev/null; then want_cef=true; fi
    if [[ "$(awk -v g="$group" '$1 == g { print $4 }' <<< "$cells" | sort -u | tr '\n' ' ')" != "$want_cef " ]]; then
      echo "group $group: cef must be $want_cef" >&2
      bad=1
    fi
  done < <(jq -r '.groups | keys[]' "$groups")
  return "$bad"
}

line_of() { grep -n -m1 -- "$1" "$2" | cut -d: -f1; }

check_consumers() { # <ci.yml>
  local ci=$1 dir bad=0 job f needs_producer waits wait_line source_line install_line
  dir=$(mktemp -d "$work/jobs.XXXXXX")
  split_jobs "$ci" "$dir"
  for f in "$dir"/*; do
    job=${f##*/}
    [[ "$job" != product-artifact ]] || continue
    grep -q 'scripts/product-artifact.sh install' "$f" || continue
    needs_producer=false
    waits=false
    if grep -E '^    needs:' "$f" | grep -q 'product-artifact'; then needs_producer=true; fi
    if grep -q 'scripts/wait-product-artifact.sh' "$f"; then waits=true; fi
    if [[ "$needs_producer" == "$waits" ]]; then
      echo "$job: must either need product-artifact or wait for it with scripts/wait-product-artifact.sh (needs=$needs_producer, waits=$waits)" >&2
      bad=1
      continue
    fi
    [[ "$waits" == true ]] || continue
    if [[ "$(awk '/^    permissions:/ { on = 1; next } on && /^      [a-z-]+: / { print $1 $2; next } on { exit }' "$f" | sort | tr '\n' ' ')" != "actions:read contents:read " ]]; then
      echo "$job: permissions must be exactly contents: read and actions: read" >&2
      bad=1
    fi
    if ! grep -q '^          GITHUB_TOKEN: \${{ github.token }}$' "$f"; then
      echo "$job: the wait step must receive the workflow token" >&2
      bad=1
    fi
    wait_line=$(line_of 'scripts/wait-product-artifact.sh$' "$f")
    source_line=$(line_of 'wait-product-artifact.sh check-source' "$f")
    install_line=$(line_of 'scripts/product-artifact.sh install' "$f")
    if ! [[ -n "$wait_line" && -n "$source_line" && -n "$install_line" ]] \
      || ! ((wait_line < source_line && source_line < install_line)); then
      echo "$job: wait, source check and install must come in that order" >&2
      bad=1
    fi
  done
  return "$bad"
}

check_producer_overwrite() { # <ci.yml>
  local dir
  dir=$(mktemp -d "$work/producer.XXXXXX")
  split_jobs "$1" "$dir"
  [[ -f "$dir/product-artifact" ]] || { echo "no product-artifact job" >&2; return 1; }
  # The upload step is the one that names product-core.
  awk '
    /^      - / { if (step ~ /actions\/upload-artifact/ && step ~ /name: product-core/) { found = 1; if (step ~ /\n *overwrite: true\n/) good = 1 } step = "" }
    { step = step "\n" $0 }
    END { if (step ~ /actions\/upload-artifact/ && step ~ /name: product-core/) { found = 1; if (step ~ /\n *overwrite: true(\n|$)/) good = 1 } exit !(found && good) }
  ' "$dir/product-artifact" || { echo "the product-core upload must set overwrite: true" >&2; return 1; }
}

ci=.github/workflows/ci.yml
groups=scripts/ci-test-groups.json
fails=0
ok() { # <description> <command...>
  local what=$1
  shift
  if ! "$@" > "$work/out" 2>&1; then
    echo "FAIL: $what" >&2
    sed 's/^/  | /' "$work/out" >&2
    fails=$((fails + 1))
  fi
}
rejects() { # <description> <command...>
  local what=$1
  shift
  if "$@" > "$work/out" 2>&1; then
    echo "FAIL: $what was accepted" >&2
    fails=$((fails + 1))
  fi
}

ok "the tests matrix repeats ci-test-groups.json" check_matrix "$ci" "$groups"
ok "every product artifact consumer waits for the producer or needs it" check_consumers "$ci"
ok "the producer's product-core upload overwrites an older artifact" check_producer_overwrite "$ci"

# The same checks reject drift.
mutate() { # <name> <sed-script> <file>
  sed "$2" "$3" > "$work/$1"
}
mutate ci-fewer-shards.yml '/group: runtime, shard: 4, shards: 4/d' "$ci"
rejects "a missing shard cell" check_matrix "$work/ci-fewer-shards.yml" "$groups"
mutate ci-shard-count.yml 's/group: support, shard: 1, shards: 1/group: support, shard: 1, shards: 2/' "$ci"
rejects "a cell with another shard count" check_matrix "$work/ci-shard-count.yml" "$groups"
mutate ci-dup-shard.yml 's/group: runtime, shard: 3, shards: 4/group: runtime, shard: 2, shards: 4/' "$ci"
rejects "a duplicated shard" check_matrix "$work/ci-dup-shard.yml" "$groups"
mutate ci-cef.yml 's/group: gui, shard: 2, shards: 2, cef: true/group: gui, shard: 2, shards: 2, cef: false/' "$ci"
rejects "a gui cell without CEF" check_matrix "$work/ci-cef.yml" "$groups"
jq '.groups.runtime.shards = 5' "$groups" > "$work/groups-more.json"
rejects "a JSON shard count the matrix does not repeat" check_matrix "$ci" "$work/groups-more.json"
jq '.groups.extra = .groups.support' "$groups" > "$work/groups-extra.json"
rejects "a JSON group the matrix lacks" check_matrix "$ci" "$work/groups-extra.json"

# Every job here that installs the artifact and is not cache-save is a consumer.
consumer=$(sed -n '/^  [a-z0-9-]*:[[:space:]]*$/{s/[: ]//g;h};/scripts\/wait-product-artifact.sh$/{g;p}' "$ci" | head -n 1)
ok "the workflow has a consumer to mutate" test -n "$consumer"
awk -v job="$consumer" '
  $0 ~ "^  " job ":[[:space:]]*$" { on = 1 } on && /^    needs: scope-gate$/ && !done { print "    needs: product-artifact"; done = 1; next } { print }
' "$ci" > "$work/ci-needs.yml"
rejects "a consumer that both needs the producer and waits for it" check_consumers "$work/ci-needs.yml"
awk -v job="$consumer" '
  $0 ~ "^  " job ":[[:space:]]*$" { on = 1 } on && /^      actions: read$/ && !done { done = 1; next } { print }
' "$ci" > "$work/ci-perm.yml"
rejects "a consumer without actions: read" check_consumers "$work/ci-perm.yml"
awk -v job="$consumer" '
  $0 ~ "^  " job ":[[:space:]]*$" { on = 1 } on && /^      contents: read$/ && !done { print; print "      packages: write"; done = 1; next } { print }
' "$ci" > "$work/ci-wide.yml"
rejects "a consumer with an extra permission" check_consumers "$work/ci-wide.yml"
awk -v job="$consumer" '
  $0 ~ "^  " job ":[[:space:]]*$" { on = 1 } on && /scripts\/wait-product-artifact.sh$/ && !done { done = 1; print "        run: true"; next } { print }
' "$ci" > "$work/ci-nowait.yml"
rejects "a consumer that does not wait" check_consumers "$work/ci-nowait.yml"
awk -v job="$consumer" '
  $0 ~ "^  " job ":[[:space:]]*$" { on = 1 } on && /check-source/ && !done { done = 1; print "        run: true"; next } { print }
' "$ci" > "$work/ci-nosource.yml"
rejects "a consumer that skips the source check" check_consumers "$work/ci-nosource.yml"

mutate ci-no-overwrite.yml '/^          overwrite: true$/d' "$ci"
rejects "a product-core upload without overwrite" check_producer_overwrite "$work/ci-no-overwrite.yml"

if ((fails > 0)); then
  echo "$fails failure(s)" >&2
  exit 1
fi
echo "ci workflow configuration: passed"
