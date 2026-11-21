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
#     read-only permissions that wait needs. docs-schema is the one consumer
#     that does both: it needs the producer (so its runner does not idle waiting
#     for it) and keeps the wait, the source check and the install.
#  3. The producer's upload of `product-core` replaces an older same-name artifact
#     (`overwrite: true`), so a re-run after a failed attempt cannot leave two.
#  4. The shared dependency cache has one writer. Every job that restores the
#     `ci-workspace-v2` key only restores it (`save-if: false`), only cache-save
#     saves it, readers carry the job-level env that is part of the key, and the
#     browser and CEF jobs build into the shared target. Every `prepare` and
#     `cargo build` line of those jobs is also in cache-save, and each cef-e2e step
#     that builds or runs belongs to a cell.
#  5. Scheduling: docs-schema starts after the producer, browser-e2e after
#     quick-checks, every other job keeps its needs; a PR supersedes older
#     runs, a main run does not.
#  6. cef-e2e is one job with the seven real-CEF invocations, each with its
#     prerequisite builds and --no-tests fail.
#  7. cache-save restores the shared cache with an id and runs every step
#     after the restore (except the disk report) only when the restore was not an
#     exact hit; the condition behaves as a truth table over the output.
#  8. Only a main push runs offscreen-gate, gui-evidence and the main-only-gui
#     suite; ci runs ci-remaining, never the local remaining suite; no step may
#     continue on error; the pull request screenshot producer is gone and the GUI
#     PR evidence publisher is paused.
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
    if [[ "$job" == docs-schema ]]; then
      if [[ "$needs_producer" != true || "$waits" != true ]]; then
        echo "docs-schema must need product-artifact and keep the wait (needs=$needs_producer, waits=$waits)" >&2
        bad=1
        continue
      fi
    elif [[ "$needs_producer" == "$waits" ]]; then
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

cache_key='${{ runner.os }}-ci-workspace-v2'
cargo_step_re='(guard-cargo\.sh |cargo (build|test|run|nextest|clippy|check) )'
readers=(lint docs-schema otel tests offscreen-gate gui-evidence browser-e2e cef-e2e)

# Prints "<job> <shared-key> <save-if>" (tab separated) for each rust-cache step.
cache_steps() { # <ci.yml>
  local dir f
  dir=$(mktemp -d "$work/cache.XXXXXX")
  split_jobs "$1" "$dir"
  for f in "$dir"/*; do
    awk -v job="${f##*/}" '
      /uses: Swatinem\/rust-cache@/ { on = 1; key = ""; save = ""; next }
      on && /^          shared-key: / { key = $0; sub(/^          shared-key: /, "", key) }
      on && /^          save-if: / { save = $0; sub(/^          save-if: /, "", save) }
      on && key != "" && save != "" { print job "\t" key "\t" save; on = 0 }
    ' "$f"
  done
}

job_env() { # <job file> <KEY>: value of a job-level env entry
  awk -v key="$2" '
    /^    env:/ { on = 1; next }
    on && /^      [A-Za-z_]+: / { k = $1; sub(/:$/, "", k); if (k == key) { sub(/^      [A-Za-z_]+: /, ""); print; exit } next }
    on { exit }
  ' "$1"
}

check_cache_wiring() { # <ci.yml>
  local ci=$1 bad=0 dir job key save writer_env reader_env k steps writers=0
  dir=$(mktemp -d "$work/wiring.XXXXXX")
  split_jobs "$ci" "$dir"
  steps=$(cache_steps "$ci")
  while IFS=$'\t' read -r job key save; do
    [[ "$key" == "$cache_key" ]] || continue
    if [[ "$job" == cache-save ]]; then
      [[ "$save" == true ]] || { echo "cache-save must save the shared key (save-if: true)" >&2; bad=1; }
      writers=$((writers + 1))
    elif [[ "$save" != false ]]; then
      echo "$job restores the shared key and must not save it (save-if: $save)" >&2
      bad=1
    fi
  done <<< "$steps"
  [[ "$writers" == 1 ]] || { echo "the shared key must have exactly one writer, found $writers" >&2; bad=1; }
  for job in "${readers[@]}"; do
    grep -q -F "$job	$cache_key	false" <<< "$steps" || { echo "$job must restore the shared key without saving it" >&2; bad=1; }
  done
  # The key hashes the job-level CARGO_* and RUSTFLAGS: a reader that differs from
  # the writer restores nothing.
  for k in CARGO_PROFILE_DEV_DEBUG CARGO_PROFILE_TEST_DEBUG RUSTFLAGS; do
    writer_env=$(job_env "$dir/cache-save" "$k")
    [[ -n "$writer_env" ]] || { echo "cache-save has no job-level $k" >&2; bad=1; continue; }
    for job in "${readers[@]}"; do
      reader_env=$(job_env "$dir/$job" "$k")
      [[ "$reader_env" == "$writer_env" ]] || { echo "$job: job-level $k differs from cache-save" >&2; bad=1; }
    done
  done
  for job in "${readers[@]}" cache-save; do
    [[ -z "$(job_env "$dir/$job" CARGO_TARGET_DIR)" ]] || { echo "$job: CARGO_TARGET_DIR belongs on cargo steps, not the job" >&2; bad=1; }
  done
  return "$bad"
}

# The cargo steps of browser-e2e and cef-e2e build into the shared target, and no
# line names the default target directory.
check_e2e_target() { # <ci.yml>
  local ci=$1 dir job bad=0
  dir=$(mktemp -d "$work/target.XXXXXX")
  split_jobs "$ci" "$dir"
  for job in browser-e2e cef-e2e; do
    awk -v job="$job" -v re="$cargo_step_re" '
      function flush() { if (step ~ re && step !~ /CARGO_TARGET_DIR: target\/ci-shared\n/) { print job ": a cargo step without CARGO_TARGET_DIR: target/ci-shared:" step; bad = 1 } }
      /^      - / { flush(); step = "" }
      { step = step "\n" $0 }
      END { flush(); exit bad }
    ' "$dir/$job" >&2 || bad=1
    if grep -q 'target/debug' "$dir/$job"; then
      echo "$job: a line names target/debug instead of the shared target" >&2
      bad=1
    fi
  done
  return "$bad"
}

recipe_lines() { # <job file>: the prepare selections, cargo build and check lines, target installs
  sed -n -e 's/^ *scripts\/guard-cargo\.sh prepare "\$warm\/[a-z0-9-]*" /prepare /p' -e 's/^ *\(cargo build .*\)$/\1/p' -e 's/^ *\(cargo check .*\)$/\1/p' -e 's/^ *\(rustup target add .*\)$/\1/p' "$1" | sort -u
}

# Every prepare selection, cargo build and cargo check of the readers is also built by cache-save.
check_recipe_covers_readers() { # <ci.yml>
  local ci=$1 dir job bad=0 missing
  dir=$(mktemp -d "$work/recipe.XXXXXX")
  split_jobs "$ci" "$dir"
  for job in lint browser-e2e cef-e2e; do
    [[ -n "$(recipe_lines "$dir/$job")" ]] || { echo "$job: no prepare, cargo build or check lines found" >&2; bad=1; continue; }
    missing=$(comm -23 <(recipe_lines "$dir/$job") <(recipe_lines "$dir/cache-save"))
    if [[ -n "$missing" ]]; then
      echo "cache-save does not build what $job builds:" >&2
      sed 's/^/  /' <<< "$missing" >&2
      bad=1
    fi
  done
  return "$bad"
}

# cef-e2e is one job: no matrix, the seven real-CEF invocations in order, each in a
# step that also holds what the invocation needs (the helper and renderer builds,
# the binaries it is pointed at) and fails when it selects no test.
check_cef_single() { # <ci.yml>
  local dir f bad=0 i prev=0 line steps invocation want
  dir=$(mktemp -d "$work/cef.XXXXXX")
  split_jobs "$1" "$dir"
  f="$dir/cef-e2e"
  [[ -s "$f" ]] || { echo "no cef-e2e job" >&2; return 1; }
  if grep -Eq '^    strategy:|matrix' "$f"; then
    echo "cef-e2e must be a single job without a matrix" >&2
    bad=1
  fi
  if grep -q 'name: cef-e2e (' "$f"; then
    echo "cef-e2e must not carry a cell name" >&2
    bad=1
  fi
  local -a invocations=(
    'scripts/guard-cargo.sh run -p browser-cef --features cef --run-ignored all'
    'scripts/guard-cargo.sh run -p evorch-renderer --run-ignored all'
    'scripts/guard-cargo.sh run -p evorch-core --features renderer-test-helper --test renderer_owner_cef --run-ignored only'
    'scripts/guard-cargo.sh run -p core-client-e2e --features gui/cef --test preview_cef --run-ignored only'
    "scripts/guard-cargo.sh run -p gui --lib -E 'test(/^html_preview::tests::/)' --run-ignored only"
    'scripts/guard-cargo.sh run -p evorch-core --test f4_product_cef --test agent_observation_restart --test public_restart_product --run-ignored only'
    'scripts/guard-cargo.sh run -p core-client-e2e --features gui/cef --test agent_observation_gui --run-ignored only'
  )
  local helper='cargo build -p browser-cef --features cef --bin evorch-cef-helper' renderer='cargo build -p evorch-renderer'
  # needs[i]: what the step of invocation i must contain besides the invocation
  # itself, separated by |.
  local -a needs=(
    ''
    "$helper"
    "$renderer|EVORCH_RENDERER_EXE=|renderer_owner_cef"
    "$renderer|$helper|EVORCH_RENDERER_EXE="
    "$helper|EVORCH_CEF_HELPER="
    ''
    ''
  )
  for i in "${!invocations[@]}"; do
    invocation=${invocations[$i]}
    if [[ "$(grep -cF -- "$invocation" "$f")" != 1 ]]; then
      echo "cef-e2e must run invocation $((i + 1)) exactly once: $invocation" >&2
      bad=1
      continue
    fi
    line=$(grep -nF -- "$invocation" "$f" | cut -d: -f1)
    if ((line <= prev)); then
      echo "cef-e2e invocation $((i + 1)) is out of order" >&2
      bad=1
    fi
    prev=$line
    steps=$(awk -v needle="$invocation" '
      function flush() { if (index(step, needle)) { print step; found = 1 } }
      /^      - / { flush(); step = "" }
      { step = step "\n" $0 }
      END { flush(); exit !found }
    ' "$f") || { echo "cef-e2e: no step holds invocation $((i + 1))" >&2; bad=1; continue; }
    if ! grep -qF -- "$invocation --success-output immediate --no-tests fail" <<< "$steps"; then
      echo "cef-e2e invocation $((i + 1)) must keep --success-output immediate --no-tests fail" >&2
      bad=1
    fi
    if [[ -n "${needs[$i]}" ]]; then
      while IFS= read -r want; do
        grep -qF -- "$want" <<< "$steps" || { echo "cef-e2e invocation $((i + 1)): its step lacks $want" >&2; bad=1; }
      done < <(tr '|' '\n' <<< "${needs[$i]}")
    fi
  done
  return "$bad"
}

job_needs() { # <job file>: the job-level needs, normalised to a sorted word list
  grep -E '^    needs:' "$1" | sed -E 's/^    needs: *//; s/[][,]/ /g' | tr -s ' ' '\n' | grep -v '^$' | sort | tr '\n' ' '
}

check_scheduling() { # <ci.yml>
  local ci=$1 dir bad=0 job want
  dir=$(mktemp -d "$work/sched.XXXXXX")
  split_jobs "$ci" "$dir"
  # docs-schema starts after the producer, browser-e2e after quick-checks; every
  # other job keeps the needs it had.
  declare -A expected=(
    [ci]='docs-schema lint otel product-artifact quick-checks scope-gate tests '
    [docs-schema]='product-artifact '
    [browser-e2e]='quick-checks scope-gate '
    [tests]='scope-gate '
    [cef-e2e]='scope-gate '
    [offscreen-gate]='scope-gate '
    [gui-evidence]='scope-gate '
    [cache-save]='product-artifact '
  )
  for job in "${!expected[@]}"; do
    want=${expected[$job]}
    if [[ "$(job_needs "$dir/$job")" != "$want" ]]; then
      echo "$job: needs are '$(job_needs "$dir/$job")', expected '$want'" >&2
      bad=1
    fi
  done
  grep -q -F "cancel-in-progress: \${{ github.event_name == 'pull_request' }}" "$ci" \
    || { echo "only pull request runs may be cancelled by a newer run" >&2; bad=1; }
  return "$bad"
}

# cache-save: the restore step has an id; the steps before it never read its
# output; every step after it except the disk report runs only without an exact hit.
cache_hit_condition="if: steps.workspace-cache.outputs.cache-hit != 'true'"
check_cache_hit() { # <ci.yml>
  local dir f bad=0
  dir=$(mktemp -d "$work/hit.XXXXXX")
  split_jobs "$1" "$dir"
  f="$dir/cache-save"
  awk -v cond="$cache_hit_condition" '
    function flush() {
      if (step == "") return
      is_restore = (step ~ /Swatinem\/rust-cache@/)
      has_cond = (index(step, "\n        " cond "\n") > 0)
      mentions = (step ~ /workspace-cache/)
      if (is_restore) {
        restored = 1
        if (step !~ /\n        id: workspace-cache\n/) { print "the restore step has no id: workspace-cache"; bad = 1 }
        if (step ~ /\n        if:/) { print "the restore step must always run"; bad = 1 }
        if (step !~ /\n          save-if: true(\n|$)/) { print "the restore step must keep save-if: true"; bad = 1 }
        if (step ~ /lookup-only/) { print "the restore step must not be lookup-only"; bad = 1 }
      } else if (!restored) {
        if (mentions) { print "a step before the restore reads its output:" step; bad = 1 }
      } else if (step ~ /name: Report disk usage/) {
        if (mentions) { print "the disk report must not depend on the restore"; bad = 1 }
      } else if (!has_cond) {
        print "a step after the restore lacks the cache-hit condition:" step; bad = 1
      }
    }
    /^      - / { flush(); step = "" }
    { step = step "\n" $0 }
    END { flush(); if (!restored) { print "no restore step"; bad = 1 } exit bad }
  ' "$f" >&2 || bad=1
  [[ "$(grep -c 'rust-cache@' "$f")" == 1 ]] || { echo "cache-save restores the cache more than once" >&2; bad=1; }
  return "$bad"
}

# The condition as a truth table over the action's output: only the exact string
# true skips the steps; false, empty and anything unknown run them.
check_cache_hit_truth() { # <ci.yml>
  local dir f cond op lit value want got bad=0
  dir=$(mktemp -d "$work/truth.XXXXXX")
  split_jobs "$1" "$dir"
  f="$dir/cache-save"
  cond=$(sed -n "s/^        if: steps\.workspace-cache\.outputs\.cache-hit \(!=\|==\) '\([a-z]*\)'\$/\1 \2/p" "$f" | sort -u)
  [[ "$(wc -l <<< "$cond" | tr -d ' ')" == 1 && -n "$cond" ]] || { echo "cache-save does not use one cache-hit comparison" >&2; return 1; }
  op=${cond%% *}
  lit=${cond#* }
  for value in true false '' unknown TRUE 1; do
    if [[ "$op" == '!=' ]]; then
      if [[ "$value" != "$lit" ]]; then got=run; else got=skip; fi
    else
      if [[ "$value" == "$lit" ]]; then got=run; else got=skip; fi
    fi
    want=run
    [[ "$value" != true ]] || want=skip
    if [[ "$got" != "$want" ]]; then
      echo "cache-hit '$value': the recipe steps would $got, expected $want" >&2
      bad=1
    fi
  done
  return "$bad"
}

main_push_if="if: github.event_name == 'push' && github.ref == 'refs/heads/main'"
check_main_only() { # <ci.yml> <gui-pr-evidence.yml>
  local ci=$1 publisher=$2 dir bad=0 job
  dir=$(mktemp -d "$work/main.XXXXXX")
  split_jobs "$ci" "$dir"
  for job in offscreen-gate gui-evidence; do
    if [[ "$(grep -cxF "    $main_push_if" "$dir/$job")" != 1 ]]; then
      echo "$job must run on a push to main only (job-level $main_push_if)" >&2
      bad=1
    fi
  done
  if grep -qE '^    if:' "$dir/tests"; then
    echo "the tests job must run on every event (no job-level if)" >&2
    bad=1
  fi
  awk -v want="        $main_push_if && matrix.group == 'gui'" '
    function flush() {
      if (step ~ /name: Main-only GUI tests/) {
        found = 1
        if (index(step, "\n" want "\n") == 0) { print "the main-only step needs exactly: " want; bad = 1 }
        if (step !~ / main-only-gui "\$EVORCH_TEST_INVENTORY_DIR"(\n|$)/) { print "the main-only step must run the main-only-gui suite"; bad = 1 }
      }
      if (step ~ /name: Remaining tests/) {
        rfound = 1
        if (step !~ / ci-remaining "\$EVORCH_TEST_INVENTORY_DIR"(\n|$)/) { print "Remaining tests must run ci-remaining"; bad = 1 }
        if (step ~ /\n *if:/) { print "Remaining tests must run on every event"; bad = 1 }
      }
      if (step ~ /run-test-group\.sh run .* remaining "/) { print "a step runs the local remaining suite"; bad = 1 }
    }
    /^      - / { flush(); step = "" }
    { step = step "\n" $0 }
    END { flush(); if (!found || !rfound) { print "the tests job lacks the Remaining tests or the main-only step"; bad = 1 } exit bad }
  ' "$dir/tests" >&2 || bad=1
  if grep -q 'continue-on-error' "$ci"; then
    echo "no step may continue on error" >&2
    bad=1
  fi
  if grep -q 'gui-pr-preview' "$ci"; then
    echo "ci must not produce the pull request screenshots any more" >&2
    bad=1
  fi
  grep -q '^    if: always()$' "$dir/ci" || { echo "ci must keep if: always()" >&2; bad=1; }
  if [[ "$(awk '/^  publish:/ { on = 1; next } on && /^    if: / { sub(/^    if: /, ""); print; exit }' "$publisher")" != '${{ false }}' ]]; then
    echo "the GUI PR evidence publisher must be paused with if: \${{ false }}" >&2
    bad=1
  fi
  grep -q 'workflows: \[ci\]' "$publisher" || { echo "the publisher must keep following the ci workflow" >&2; bad=1; }
  return "$bad"
}

ci=.github/workflows/ci.yml
publisher=.github/workflows/gui-pr-evidence.yml
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
ok "the shared dependency cache has one writer and matching readers" check_cache_wiring "$ci"
ok "browser-e2e and cef-e2e build into the shared target" check_e2e_target "$ci"
ok "cache-save builds what browser-e2e and cef-e2e build" check_recipe_covers_readers "$ci"
ok "cef-e2e is one job with the seven invocations" check_cef_single "$ci"
ok "docs-schema, browser-e2e and the other jobs have the intended needs" check_scheduling "$ci"
ok "cache-save skips its recipe steps only on an exact hit" check_cache_hit "$ci"
ok "the cache-hit condition runs the recipe unless the output is exactly true" check_cache_hit_truth "$ci"
ok "only a main push runs the former GUI checks, and the PR screenshots are paused" check_main_only "$ci" "$publisher"

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
mutate ci-cef.yml 's/group: gui, shard: 1, shards: 1, cef: true/group: gui, shard: 1, shards: 1, cef: false/' "$ci"
rejects "a gui cell without CEF" check_matrix "$work/ci-cef.yml" "$groups"
jq '.groups.runtime.shards = 5' "$groups" > "$work/groups-more.json"
rejects "a JSON shard count the matrix does not repeat" check_matrix "$ci" "$work/groups-more.json"
jq '.groups.extra = .groups.support' "$groups" > "$work/groups-extra.json"
rejects "a JSON group the matrix lacks" check_matrix "$ci" "$work/groups-extra.json"

# tests is a consumer that waits for the producer without needing it.
consumer=tests
ok "the workflow has a consumer to mutate" grep -q '^  tests:' "$ci"
awk -v job="$consumer" '
  $0 ~ "^  " job ":[[:space:]]*$" { on = 1 } on && /^    needs: / && !done { print "    needs: product-artifact"; done = 1; next } { print }
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

mutate ci-reader-key.yml '0,/shared-key: ${{ runner.os }}-ci-workspace-v2/s//shared-key: ${{ runner.os }}-ci-workspace/' "$ci"
rejects "a reader on another cache key" check_cache_wiring "$work/ci-reader-key.yml"
awk '/^  browser-e2e:/ { on = 1 } on && /save-if: false/ && !done { sub(/false/, "true"); done = 1 } { print }' "$ci" > "$work/ci-reader-saves.yml"
rejects "a reader that saves the shared cache" check_cache_wiring "$work/ci-reader-saves.yml"
awk '/^  browser-e2e:/ { on = 1 } on && /CARGO_PROFILE_TEST_DEBUG/ && !done { done = 1; next } { print }' "$ci" > "$work/ci-reader-env.yml"
rejects "a reader without the writer's job-level env" check_cache_wiring "$work/ci-reader-env.yml"
awk '/^  cef-e2e:/ { on = 1 } on && /CARGO_TARGET_DIR: target\/ci-shared/ && !done { done = 1; next } { print }' "$ci" > "$work/ci-cef-target.yml"
rejects "a cef-e2e cargo step outside the shared target" check_e2e_target "$work/ci-cef-target.yml"
mutate ci-recipe-gap.yml '/prepare "\$warm\/browser-5"/d' "$ci"
rejects "a browser selection cache-save does not build" check_recipe_covers_readers "$work/ci-recipe-gap.yml"
awk '/^  cache-save:/ { on = 1 } on && /cargo check --target aarch64-apple-darwin/ { next } { print }' "$ci" > "$work/ci-lint-gap.yml"
rejects "a lint check cache-save does not build" check_recipe_covers_readers "$work/ci-lint-gap.yml"
# docs-schema needs the producer and keeps the wait; no other consumer may need it.
mutate ci-docs-no-needs.yml 's/^    needs: product-artifact$/    needs: scope-gate/' "$ci"
rejects "docs-schema without needs: product-artifact" check_consumers "$work/ci-docs-no-needs.yml"
rejects "docs-schema with other needs" check_scheduling "$work/ci-docs-no-needs.yml"
awk '/^  docs-schema:/ { on = 1 } on && /scripts\/wait-product-artifact.sh$/ && !done { done = 1; print "        run: true"; next } { print }' "$ci" > "$work/ci-docs-nowait.yml"
rejects "docs-schema that does not wait" check_consumers "$work/ci-docs-nowait.yml"
awk '/^  browser-e2e:/ { on = 1 } on && /^    needs: / && !done { print "    needs: [scope-gate, product-artifact]"; done = 1; next } { print }' "$ci" > "$work/ci-browser-needs.yml"
rejects "browser-e2e that needs the producer and waits" check_consumers "$work/ci-browser-needs.yml"
mutate ci-browser-quick.yml 's/needs: \[scope-gate, quick-checks\]/needs: scope-gate/' "$ci"
rejects "browser-e2e without quick-checks" check_scheduling "$work/ci-browser-quick.yml"
awk '/^  tests:/ { on = 1 } on && /^    needs: scope-gate$/ && !done { print "    needs: [scope-gate, quick-checks]"; done = 1; next } { print }' "$ci" > "$work/ci-tests-needs.yml"
rejects "another job gaining a needs" check_scheduling "$work/ci-tests-needs.yml"
mutate ci-agg-needs.yml 's/needs: \[quick-checks, scope-gate, lint, docs-schema, otel, product-artifact, tests\]/needs: [quick-checks, scope-gate, lint, docs-schema, otel, product-artifact]/' "$ci"
rejects "an aggregate that stops needing tests" check_scheduling "$work/ci-agg-needs.yml"

# cef-e2e: one job, seven invocations, their prerequisites.
awk '/^  cef-e2e:/ { print; print "    strategy:"; print "      matrix:"; print "        cell: [engine, client]"; next } { print }' "$ci" > "$work/ci-cef-matrix.yml"
rejects "a matrix on cef-e2e" check_cef_single "$work/ci-cef-matrix.yml"
mutate ci-cef-restart.yml 's/ --test public_restart_product//' "$ci"
rejects "the public_restart_product test dropped from cef-e2e" check_cef_single "$work/ci-cef-restart.yml"
mutate ci-cef-gui.yml '/--test agent_observation_gui --run-ignored only/d' "$ci"
rejects "an invocation dropped from cef-e2e" check_cef_single "$work/ci-cef-gui.yml"
mutate ci-cef-notests.yml '/--test preview_cef --run-ignored only/s/ --no-tests fail//' "$ci"
rejects "an invocation that may select no test" check_cef_single "$work/ci-cef-notests.yml"
mutate ci-cef-feature.yml 's/-p core-client-e2e --features gui\/cef --test agent_observation_gui/-p core-client-e2e --test agent_observation_gui/' "$ci"
rejects "an invocation with changed features" check_cef_single "$work/ci-cef-feature.yml"
awk '/^  cef-e2e:/ { on = 1 } on && /export EVORCH_RENDERER_EXE=/ && !done { done = 1; next } { print }' "$ci" > "$work/ci-cef-env.yml"
rejects "a CEF step without its renderer binary" check_cef_single "$work/ci-cef-env.yml"
awk '/^  cef-e2e:/ { on = 1 } on && /cargo build -p browser-cef --features cef --bin evorch-cef-helper/ { n++; if (n == 4) next } { print }' "$ci" > "$work/ci-cef-helper.yml"
rejects "a CEF step without its helper build" check_cef_single "$work/ci-cef-helper.yml"
awk '/^  cef-e2e:/ { on = 1 } on && /scripts\/guard-cargo.sh run -p browser-cef --features cef --run-ignored all/ { hold = $0; next } on && /scripts\/guard-cargo.sh run -p gui --lib/ && hold != "" { print; print hold; hold = ""; next } { print }' "$ci" > "$work/ci-cef-order.yml"
rejects "invocations out of order" check_cef_single "$work/ci-cef-order.yml"

# cache-save: the cache-hit condition on every recipe step, and nothing else.
drop_condition() { # <out> <step-start regexp>: remove the line after the matching step's first line, inside cache-save
  awk -v re="$2" '/^  cache-save:/ { on = 1 } on && $0 ~ re { print; getline; next } { print }' "$ci" > "$work/$1"
}
drop_condition ci-hit-warm.yml 'name: Build the dependencies of the fixture, browser and CEF test selections'
rejects "a warm-up step that always runs" check_cache_hit "$work/ci-hit-warm.yml"
drop_condition ci-hit-build.yml 'name: Build the dependencies of every other job'
rejects "the main recipe step that always runs" check_cache_hit "$work/ci-hit-build.yml"
drop_condition ci-hit-nextest.yml 'uses: taiki-e/install-action'
rejects "cargo-nextest installed on every run" check_cache_hit "$work/ci-hit-nextest.yml"
drop_condition ci-hit-download.yml 'uses: actions/download-artifact'
rejects "the product download on every run" check_cache_hit "$work/ci-hit-download.yml"
drop_condition ci-hit-install.yml 'name: Install the sealed product artifact'
rejects "the product install on every run" check_cache_hit "$work/ci-hit-install.yml"
awk '/^  cache-save:/ { on = 1 } on && /^        id: workspace-cache$/ { next } { print }' "$ci" > "$work/ci-hit-id.yml"
rejects "a restore step without an id" check_cache_hit "$work/ci-hit-id.yml"
awk '/^  cache-save:/ { on = 1 } on && /^          save-if: true$/ { print; print "          lookup-only: true"; next } { print }' "$ci" > "$work/ci-hit-lookup.yml"
rejects "a lookup-only restore" check_cache_hit "$work/ci-hit-lookup.yml"
mutate ci-hit-eq.yml "s/cache-hit != 'true'/cache-hit == 'true'/" "$ci"
rejects "an inverted cache-hit condition" check_cache_hit_truth "$work/ci-hit-eq.yml"
mutate ci-hit-false.yml "s/cache-hit != 'true'/cache-hit != 'false'/" "$ci"
rejects "a condition that skips on a partial restore" check_cache_hit_truth "$work/ci-hit-false.yml"

# Main-only conditions, the suite names and the paused publisher.
awk '/^  offscreen-gate:/ { on = 1 } on && /^    if: / && !done { done = 1; next } { print }' "$ci" > "$work/ci-main-offscreen.yml"
rejects "offscreen-gate on every event" check_main_only "$work/ci-main-offscreen.yml" "$publisher"
awk '/^  gui-evidence:/ { on = 1 } on && /^    if: / && !done { print "    if: github.event_name == '"'"'push'"'"'"; done = 1; next } { print }' "$ci" > "$work/ci-main-evidence.yml"
rejects "gui-evidence on any push" check_main_only "$work/ci-main-evidence.yml" "$publisher"
mutate ci-main-step.yml "s/ && matrix.group == 'gui'\$//" "$ci"
rejects "a main-only step without the gui group condition" check_main_only "$work/ci-main-step.yml" "$publisher"
mutate ci-main-suite.yml 's/ main-only-gui "\$EVORCH_TEST_INVENTORY_DIR"/ remaining "$EVORCH_TEST_INVENTORY_DIR"/' "$ci"
rejects "the main-only step running the local remaining suite" check_main_only "$work/ci-main-suite.yml" "$publisher"
mutate ci-main-remaining.yml 's/ ci-remaining "\$EVORCH_TEST_INVENTORY_DIR"/ remaining "$EVORCH_TEST_INVENTORY_DIR"/' "$ci"
rejects "the shards running the local remaining suite" check_main_only "$work/ci-main-remaining.yml" "$publisher"
mutate ci-main-coe.yml 's/^\(  *\)- name: Main-only GUI tests (main push)$/\1- name: Main-only GUI tests (main push)\n\1  continue-on-error: true/' "$ci"
rejects "a main-only step that continues on error" check_main_only "$work/ci-main-coe.yml" "$publisher"
{ cat "$ci"; printf '# gui-pr-preview\n'; } > "$work/ci-preview.yml"
rejects "the pull request screenshots producer" check_main_only "$work/ci-preview.yml" "$publisher"
mutate publisher-active.yml "s/^    if: \\\${{ false }}\$/    if: github.event.workflow_run.conclusion == 'success'/" "$publisher"
rejects "an active GUI PR evidence publisher" check_main_only "$ci" "$work/publisher-active.yml"

mutate ci-cancel.yml 's/cancel-in-progress: .*/cancel-in-progress: true/' "$ci"
rejects "cancelling main runs" check_scheduling "$work/ci-cancel.yml"

if ((fails > 0)); then
  echo "$fails failure(s)" >&2
  exit 1
fi
echo "ci workflow configuration: passed"
