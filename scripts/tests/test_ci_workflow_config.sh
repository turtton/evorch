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
#  6. cef-e2e is one job with the six real-CEF invocations, each with its
#     prerequisite builds and --no-tests fail.
#  7. cache-save restores the shared cache with an id and runs every step
#     after the restore (except the disk report) only when the restore was not an
#     exact hit; the condition behaves as a truth table over the output.
#  8. Nothing is left to a main push: the former embedded-GUI jobs (offscreen-gate,
#     gui-evidence), the main-only-gui suite, the PR screenshot producer and its
#     publisher workflow are gone; every event runs cache, io and remaining; no
#     step may continue on error.
#  9. The release expiry cells (core_expiry and core_browser_expiry) run in one
#     job, release-expiry, that the `ci` gate needs: both release test binaries
#     are prepared once and their listing is checked against the three groups
#     before the product is installed, nothing compiles after the install, the
#     six cells run once each from that one prepared metadata, and the CEF
#     distribution reaches only the CEF helper build and the CEF cells' step (never
#     GITHUB_ENV; core and Chromium cells run without it). The shards, browser-e2e
#     and cef-e2e no longer run them, and their SIGKILL cells stay.
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
cargo_step_re='(guard-cargo\.sh |release-expiry\.sh |cargo (build|test|run|nextest|clippy|check) )'
readers=(lint docs-schema otel tests browser-e2e cef-e2e release-expiry)

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
  for job in browser-e2e cef-e2e release-expiry; do
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
  sed -n -e 's/^ *scripts\/guard-cargo\.sh prepare "[^"]*" /prepare /p' -e 's/^ *\(cargo build .*\)$/\1/p' -e 's/^ *\(cargo check .*\)$/\1/p' -e 's/^ *\(rustup target add .*\)$/\1/p' "$1" | sort -u
}

# Every prepare selection, cargo build and cargo check of the readers is also built by cache-save.
check_recipe_covers_readers() { # <ci.yml>
  local ci=$1 dir job bad=0 missing
  dir=$(mktemp -d "$work/recipe.XXXXXX")
  split_jobs "$ci" "$dir"
  for job in lint browser-e2e cef-e2e release-expiry; do
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

# cef-e2e is one job: no matrix, the six real-CEF invocations in order, each in a
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
    [ci]='docs-schema lint otel product-artifact quick-checks release-expiry scope-gate tests '
    [release-expiry]='scope-gate '
    [docs-schema]='product-artifact '
    [browser-e2e]='quick-checks scope-gate '
    [tests]='scope-gate '
    [cef-e2e]='scope-gate '
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
# Models the condition the way Actions evaluates it: string `==` and `!=` compare
# ignoring case, and a missing output is the empty string. The producer's range
# is "true" or "false" (Swatinem/rust-cache sets cache-hit from a boolean); the
# other values only pin that anything but an exact hit runs the recipe.
check_cache_hit_truth() { # <ci.yml>
  local dir f cond op lit value want got bad=0 lower
  dir=$(mktemp -d "$work/truth.XXXXXX")
  split_jobs "$1" "$dir"
  f="$dir/cache-save"
  cond=$(sed -n "s/^        if: steps\.workspace-cache\.outputs\.cache-hit \(!=\|==\) '\([a-z]*\)'\$/\1 \2/p" "$f" | sort -u)
  [[ "$(wc -l <<< "$cond" | tr -d ' ')" == 1 && -n "$cond" ]] || { echo "cache-save does not use one cache-hit comparison" >&2; return 1; }
  op=${cond%% *}
  lit=${cond#* }
  for value in true false '' unknown TRUE 1; do
    lower=${value,,}
    if [[ "$op" == '!=' ]]; then
      if [[ "$lower" != "$lit" ]]; then got=run; else got=skip; fi
    else
      if [[ "$lower" == "$lit" ]]; then got=run; else got=skip; fi
    fi
    want=run
    [[ "${value,,}" != true ]] || want=skip
    if [[ "$got" != "$want" ]]; then
      echo "cache-hit '$value': the recipe steps would $got, expected $want" >&2
      bad=1
    fi
  done
  return "$bad"
}

# --- release-expiry ---------------------------------------------------------
expiry_prepare='scripts/guard-cargo.sh prepare "$RUNNER_TEMP/release-expiry" -p evorch-core --release --features bootstrap-test-support --test core_expiry --test core_browser_expiry --locked'
expiry_run() { # <group>
  printf 'scripts/release-expiry.sh run "$RUNNER_TEMP/release-expiry" %s "$RUNNER_TEMP/release-expiry-evidence"' "$1"
}

# The first step of a job file that contains the needle (comments already dropped).
step_with() { # <job file> <needle>
  awk -v needle="$2" '
    function flush() { if (!found && index(step, needle)) { print step; found = 1 } }
    /^      - / { flush(); step = "" }
    { step = step "\n" $0 }
    END { flush(); exit !found }
  ' "$1"
}

check_release_expiry() { # <ci.yml>
  local dir f bad=0 line prev=0 group step label
  dir=$(mktemp -d "$work/expiry.XXXXXX")
  split_jobs "$1" "$dir"
  f="$dir/release-expiry"
  [[ -s "$f" ]] || { echo "no release-expiry job" >&2; return 1; }
  if ! grep -E '^    needs:' "$dir/ci" | grep -q 'release-expiry'; then
    echo "the ci job must need release-expiry" >&2
    bad=1
  fi
  # One prepare of both release test binaries, then the inventory check, the
  # helper builds, the product and the three groups, in that order; nothing
  # compiles after the install.
  if [[ "$(grep -cF -- "$expiry_prepare" "$f")" != 1 ]]; then
    echo "release-expiry must prepare both release test binaries exactly as: $expiry_prepare" >&2
    bad=1
  fi
  local -a order=(
    "$expiry_prepare"
    'scripts/release-expiry.sh check-inventory "$RUNNER_TEMP/release-expiry"'
    'cargo build -p browser-cef --features cef --bin evorch-cef-helper'
    'cargo build -p evorch-renderer'
    'run: scripts/wait-product-artifact.sh'
    'wait-product-artifact.sh check-source'
    'scripts/product-artifact.sh install'
    "$(expiry_run core)"
    "$(expiry_run chromium)"
    "$(expiry_run cef)"
  )
  for label in "${order[@]}"; do
    line=$(grep -nF -m1 -- "$label" "$f" | cut -d: -f1 || true)
    if [[ -z "$line" ]]; then
      echo "release-expiry lacks: $label" >&2
      bad=1
      continue
    fi
    if ((line <= prev)); then
      echo "release-expiry: out of order: $label" >&2
      bad=1
    fi
    prev=$line
  done
  for group in core chromium cef; do
    if [[ "$(grep -cF -- "$(expiry_run "$group")" "$f")" != 1 ]]; then
      echo "release-expiry must run group $group exactly once" >&2
      bad=1
    fi
  done
  line=$(line_of 'scripts/product-artifact.sh install' "$f")
  if [[ -n "$line" ]] && tail -n +"$((line + 1))" "$f" | grep -Eq '(^|[^-A-Za-z_])cargo |guard-cargo\.sh|nextest|scripts/release-expiry\.sh (prepare|check-inventory)'; then
    echo "release-expiry: a cargo invocation follows the product install" >&2
    bad=1
  fi
  # The environment: GITHUB_ENV is written once, by the product install. The CEF
  # paths reach only the CEF helper build and the CEF cells' step, the browser only
  # the Chromium cells' step.
  if [[ "$(grep -c 'GITHUB_ENV' "$f")" != 1 ]] || ! grep -q 'product-artifact.sh install .* >> "\$GITHUB_ENV"' "$f"; then
    echo "release-expiry may write GITHUB_ENV only through the product install" >&2
    bad=1
  fi
  step=$(step_with "$f" 'uses: ./.github/actions/setup-cef') || step=''
  if ! grep -q '^        id: cef$' <<< "$step" || ! grep -q '^          runtime-path: full$' <<< "$step" \
    || ! grep -q '^          export-env: "false"$' <<< "$step"; then
    echo "release-expiry must call setup-cef with id: cef, runtime-path: full and export-env: \"false\"" >&2
    bad=1
  fi
  if [[ -n "$(job_env "$f" LD_LIBRARY_PATH)$(job_env "$f" CEF_PATH)$(job_env "$f" CHROME)" ]]; then
    echo "release-expiry: the CEF paths and the browser do not belong on the job" >&2
    bad=1
  fi
  for label in 'Build the CEF helper and renderer' 'Real CEF navigation then ten-second expiry'; do
    step=$(step_with "$f" "name: $label") || step=''
    [[ -n "$step" ]] || { echo "release-expiry lacks the step: $label" >&2; bad=1; continue; }
    grep -qF 'CEF_PATH: ${{ steps.cef.outputs.cef-path }}' <<< "$step" \
      || { echo "release-expiry: '$label' lacks CEF_PATH from the setup-cef output" >&2; bad=1; }
  done
  step=$(step_with "$f" "$(expiry_run cef)") || step=''
  grep -qF 'LD_LIBRARY_PATH: ${{ steps.cef.outputs.loader-path }}' <<< "$step" \
    || { echo "release-expiry: the CEF cells' step lacks the CEF loader path" >&2; bad=1; }
  grep -q 'export EVORCH_RENDERER_EXE="\$PWD/target/ci-shared/debug/evorch-renderer"' <<< "$step" \
    || { echo "release-expiry: the CEF cells' step lacks EVORCH_RENDERER_EXE" >&2; bad=1; }
  step=$(step_with "$f" "$(expiry_run chromium)") || step=''
  grep -qF 'CHROME: ${{ steps.chrome.outputs.path }}' <<< "$step" \
    || { echo "release-expiry: the Chromium cells' step lacks CHROME" >&2; bad=1; }
  awk '
    function flush() {
      if (step ~ /(LD_LIBRARY_PATH|CEF_PATH|steps\.cef\.outputs)/ \
          && step !~ /name: Build the CEF helper and renderer/ \
          && step !~ /name: Real CEF navigation then ten-second expiry/ \
          && step !~ /actions\/setup-cef/) { print "a step other than the CEF ones names the CEF environment:" step; bad = 1 }
      if (step ~ /(CHROME:|steps\.chrome\.outputs)/ && step !~ /name: Real Chromium navigation then ten-second expiry/) {
        print "a step other than the Chromium cells names CHROME:" step; bad = 1
      }
    }
    /^      - / { flush(); step = "" }
    { step = step "\n" $0 }
    END { flush(); exit bad }
  ' "$f" >&2 || bad=1
  step=$(step_with "$f" 'name: release-expiry-evidence') || step=''
  if ! grep -q 'if: always()' <<< "$step" || ! grep -q 'if-no-files-found: error' <<< "$step" \
    || ! grep -qF 'path: ${{ runner.temp }}/release-expiry-evidence' <<< "$step"; then
    echo "release-expiry must always upload release-expiry-evidence and fail when it is empty" >&2
    bad=1
  fi
  # The cells moved: no other job runs or builds them, the SIGKILL cells stay.
  for label in tests browser-e2e cef-e2e; do
    if grep -Eq 'core_expiry|core_browser_expiry|--release' "$dir/$label"; then
      echo "$label still names the expiry cells or a release build" >&2
      bad=1
    fi
  done
  [[ "$(grep -c -- '--test core_browser_sigkill' "$dir/browser-e2e")" == 1 ]] || { echo "browser-e2e lost the Chromium SIGKILL cell" >&2; bad=1; }
  [[ "$(grep -c -- '--test core_browser_sigkill' "$dir/cef-e2e")" == 1 ]] || { echo "cef-e2e lost the CEF SIGKILL cell" >&2; bad=1; }
  grep -q 'name: core-sigkill-evidence-' "$dir/tests" || { echo "tests lost the SIGKILL evidence upload" >&2; bad=1; }
  grep -q 'Prepare real caller SIGKILL matrix dependencies' "$dir/tests" || { echo "tests lost the SIGKILL matrix preparation" >&2; bad=1; }
  for label in core_expiry core_browser_expiry; do
    grep -q "^filter = 'binary(=$label)'\$" .config/nextest.toml || { echo ".config/nextest.toml lost the override of $label" >&2; bad=1; }
  done
  return "$bad"
}

# The script keeps every flag of the cells and the one definition of the groups.
check_release_expiry_script() { # <release-expiry.sh>
  local s=$1 bad=0 want
  local -a wants=(
    '-E "$filter" --run-ignored only --test-threads 1 --success-output immediate --no-tests fail'
    "core) echo 'binary(=core_expiry)' ;;"
    "chromium) echo 'binary(=core_browser_expiry) & (test(=expiry_chromium_t) | test(=expiry_chromium_stopped_t))' ;;"
    "cef) echo 'binary(=core_browser_expiry) & (test(=expiry_cef_t) | test(=expiry_cef_stopped_t))' ;;"
    '--cargo-metadata "$dir/cargo.json" --binaries-metadata "$dir/default-binaries.json"'
    '--run-ignored only "${extra[@]}" --message-format json'
    '*-$cell-*-death.json'
    'evorch-core::core_expiry expiry_capture_first \'
    'evorch-core::core_expiry expiry_signal_first \'
    'evorch-core::core_browser_expiry expiry_chromium_t \'
    'evorch-core::core_browser_expiry expiry_chromium_stopped_t \'
    'evorch-core::core_browser_expiry expiry_cef_t \'
    'evorch-core::core_browser_expiry expiry_cef_stopped_t \'
    '[[ "$all" != "$(expected_cells)" ]]'
  )
  for want in "${wants[@]}"; do
    grep -qF -- "$want" "$s" || { echo "release-expiry.sh lacks: $want" >&2; bad=1; }
  done
  if grep -Eq -- '--release|cargo (build|test|run)' "$s"; then
    echo "release-expiry.sh must not compile or select a profile" >&2
    bad=1
  fi
  return "$bad"
}

# setup-cef puts CEF_PATH and LD_LIBRARY_PATH on GITHUB_ENV (the whole job) only
# for export-env true, which stays the default; the outputs carry them either way.
# Only release-expiry turns the export off.
check_setup_cef_action() { # <action.yml> <ci.yml>
  local a=$1 workflow=$2 bad=0 n_case n_true n_env n_false
  awk '/^  export-env:/ { on = 1; next } on && /^    default: / { print $2; exit } on && /^[a-z]/ { exit }' "$a" | grep -qx '"true"' \
    || { echo "export-env must default to \"true\"" >&2; bad=1; }
  { grep -q '^  cef-path:$' "$a" && grep -q '^  loader-path:$' "$a"; } || { echo "setup-cef must output cef-path and loader-path" >&2; bad=1; }
  [[ "$(grep -c '>> "\$GITHUB_ENV"' "$a")" == 1 ]] || { echo "setup-cef must write GITHUB_ENV exactly once" >&2; bad=1; }
  n_case=$(line_of 'case "$EXPORT_ENV" in' "$a")
  n_true=$(line_of '^          true)$' "$a")
  n_env=$(line_of '>> "\$GITHUB_ENV"' "$a")
  n_false=$(line_of '^          false) ;;$' "$a")
  if ! [[ -n "$n_case" && -n "$n_true" && -n "$n_env" && -n "$n_false" ]] \
    || ! ((n_case < n_true && n_true < n_env && n_env < n_false)); then
    echo "the GITHUB_ENV write must sit in the export-env true arm only" >&2
    bad=1
  fi
  grep -qF 'EXPORT_ENV: ${{ inputs.export-env }}' "$a" || { echo "the export-env input must reach the script" >&2; bad=1; }
  [[ "$(grep -c 'export-env:' "$workflow")" == 1 ]] || { echo "only release-expiry may turn the environment export off" >&2; bad=1; }
  return "$bad"
}

# Every event runs the same suites, and the jobs and files of the former embedded GUI
# verification are gone, not paused.
check_every_event() { # <ci.yml>
  local ci=$1 dir bad=0 job gone
  dir=$(mktemp -d "$work/main.XXXXXX")
  split_jobs "$ci" "$dir"
  for job in offscreen-gate gui-evidence; do
    if [[ -e "$dir/$job" ]]; then
      echo "$job belonged to the embedded GUI and must be gone" >&2
      bad=1
    fi
  done
  if grep -qE '^    if:' "$dir/tests"; then
    echo "the tests job must run on every event (no job-level if)" >&2
    bad=1
  fi
  awk '
    function flush() {
      if (step ~ /name: Remaining tests/) {
        rfound = 1
        if (step !~ / remaining "\$EVORCH_TEST_INVENTORY_DIR"(\n|$)/) { print "Remaining tests must run the remaining suite"; bad = 1 }
        if (step ~ /\n *if:/) { print "Remaining tests must run on every event"; bad = 1 }
      }
      if (step ~ /run-test-group\.sh run .* (ci-remaining|main-only-gui) "/) { print "a step runs a removed suite"; bad = 1 }
      if (step ~ /event_name == .push./ && step ~ /run-test-group\.sh/) { print "a test step is left to a main push"; bad = 1 }
    }
    /^      - / { flush(); step = "" }
    { step = step "\n" $0 }
    END { flush(); if (!rfound) { print "the tests job lacks the Remaining tests step"; bad = 1 } exit bad }
  ' "$dir/tests" >&2 || bad=1
  for gone in main-only-gui ci-remaining gui-evidence-matrix check-gui-native check-gui-wayland-idle headless_capture native_qa_window gui-pr-preview html_preview; do
    if grep -q -- "$gone" "$ci"; then
      echo "ci must not name the removed $gone any more" >&2
      bad=1
    fi
  done
  if grep -q 'continue-on-error' "$ci"; then
    echo "no step may continue on error" >&2
    bad=1
  fi
  grep -q '^    if: always()$' "$dir/ci" || { echo "ci must keep if: always()" >&2; bad=1; }
  for gone in .github/workflows/gui-pr-evidence.yml scripts/ci-main-only-gui.sh scripts/ci-main-only-gui-tests.json \
    scripts/gui-evidence-matrix.sh scripts/gui-plan-evidence.sh scripts/check-gui-native.sh scripts/check-gui-wayland-idle.sh \
    scripts/post-gui-pr-evidence.sh; do
    if [[ -e "$gone" ]]; then
      echo "$gone belonged to the embedded GUI verification and must be gone" >&2
      bad=1
    fi
  done
  return "$bad"
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
ok "the shared dependency cache has one writer and matching readers" check_cache_wiring "$ci"
ok "browser-e2e and cef-e2e build into the shared target" check_e2e_target "$ci"
ok "cache-save builds what browser-e2e and cef-e2e build" check_recipe_covers_readers "$ci"
ok "cef-e2e is one job with the six invocations" check_cef_single "$ci"
ok "docs-schema, browser-e2e and the other jobs have the intended needs" check_scheduling "$ci"
ok "the release expiry cells run in one job, the ci gate needs it, CEF stays off the other cells" check_release_expiry "$ci"
ok "release-expiry.sh keeps the expiry selection and evidence" check_release_expiry_script scripts/release-expiry.sh
ok "setup-cef exports its environment only when asked to" check_setup_cef_action .github/actions/setup-cef/action.yml "$ci"
ok "cache-save skips its recipe steps only on an exact hit" check_cache_hit "$ci"
ok "the cache-hit condition runs the recipe unless the output is exactly true" check_cache_hit_truth "$ci"
ok "every event runs the same suites and the embedded GUI verification is gone" check_every_event "$ci"

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
mutate ci-agg-needs.yml 's/needs: \[quick-checks, scope-gate, lint, docs-schema, otel, product-artifact, tests, release-expiry\]/needs: [quick-checks, scope-gate, lint, docs-schema, otel, product-artifact, release-expiry]/' "$ci"
rejects "an aggregate that stops needing tests" check_scheduling "$work/ci-agg-needs.yml"
mutate ci-agg-expiry.yml 's/needs: \[quick-checks, scope-gate, lint, docs-schema, otel, product-artifact, tests, release-expiry\]/needs: [quick-checks, scope-gate, lint, docs-schema, otel, product-artifact, tests]/' "$ci"
rejects "an aggregate that stops needing release-expiry" check_scheduling "$work/ci-agg-expiry.yml"
rejects "an aggregate that stops needing release-expiry (job check)" check_release_expiry "$work/ci-agg-expiry.yml"

# release-expiry: one job, one prepare, the groups, the CEF environment, the gate.
mutate ci-expiry-release.yml 's/ --release --features bootstrap-test-support --test core_expiry --test core_browser_expiry/ --features bootstrap-test-support --test core_expiry --test core_browser_expiry/' "$ci"
rejects "a debug (not --release) expiry prepare" check_release_expiry "$work/ci-expiry-release.yml"
mutate ci-expiry-feature.yml 's/ --release --features bootstrap-test-support --test core_expiry --test core_browser_expiry/ --release --test core_expiry --test core_browser_expiry/' "$ci"
rejects "an expiry prepare without bootstrap-test-support" check_release_expiry "$work/ci-expiry-feature.yml"
mutate ci-expiry-one-binary.yml 's/ --test core_expiry --test core_browser_expiry --locked$/ --test core_expiry --locked/' "$ci"
rejects "an expiry prepare of one binary only" check_release_expiry "$work/ci-expiry-one-binary.yml"
awk '/^  release-expiry:/ { on = 1 } on && /scripts\/release-expiry.sh check-inventory/ && !done { done = 1; next } { print }' "$ci" > "$work/ci-expiry-noinv.yml"
rejects "an expiry job without the inventory check" check_release_expiry "$work/ci-expiry-noinv.yml"
awk '/^  release-expiry:/ { on = 1 } on && /scripts\/release-expiry.sh run .* chromium / && !done { done = 1; next } { print }' "$ci" > "$work/ci-expiry-nochromium.yml"
rejects "an expiry job without the Chromium cells" check_release_expiry "$work/ci-expiry-nochromium.yml"
awk '/^  release-expiry:/ { on = 1 } on && /scripts\/release-expiry.sh run .* core "/ && !done { done = 1; next } { print }' "$ci" > "$work/ci-expiry-nocore.yml"
rejects "an expiry job without the core cells" check_release_expiry "$work/ci-expiry-nocore.yml"
awk '/^  release-expiry:/ { on = 1 } on && /scripts\/release-expiry.sh run .* cef / && !done { done = 1; next } { print }' "$ci" > "$work/ci-expiry-nocef.yml"
rejects "an expiry job without the CEF cells" check_release_expiry "$work/ci-expiry-nocef.yml"
awk '/^  release-expiry:/ { on = 1 } on && /scripts\/release-expiry.sh run .* chromium / && !done { done = 1; print; print; next } { print }' "$ci" > "$work/ci-expiry-twice.yml"
rejects "an expiry group that runs twice" check_release_expiry "$work/ci-expiry-twice.yml"
awk '/^  release-expiry:/ { on = 1 } on && /scripts\/product-artifact.sh install/ && !done { done = 1; print; print "      - run: cargo build -p evorch-core"; next } { print }' "$ci" > "$work/ci-expiry-late-cargo.yml"
rejects "a cargo build after the product install" check_release_expiry "$work/ci-expiry-late-cargo.yml"
awk '/^  release-expiry:/ { on = 1 } on && /export-env: "false"/ && !done { done = 1; next } { print }' "$ci" > "$work/ci-expiry-export.yml"
rejects "setup-cef exporting CEF to the whole expiry job" check_release_expiry "$work/ci-expiry-export.yml"
awk '/^  release-expiry:/ { on = 1 } on && /^      RUSTFLAGS:/ && !done { done = 1; print; print "      LD_LIBRARY_PATH: /opt/cef"; next } { print }' "$ci" > "$work/ci-expiry-ld-job.yml"
rejects "the CEF loader path on the expiry job" check_release_expiry "$work/ci-expiry-ld-job.yml"
awk '/^  release-expiry:/ { on = 1 } on && /scripts\/release-expiry.sh run .* core "/ && !done { done = 1; print "        env:"; print "          LD_LIBRARY_PATH: ${{ steps.cef.outputs.loader-path }}" } { print }' "$ci" > "$work/ci-expiry-ld-core.yml"
rejects "the CEF loader path on a non-CEF cell" check_release_expiry "$work/ci-expiry-ld-core.yml"
awk '/^  release-expiry:/ { on = 1 } on && /LD_LIBRARY_PATH: \$\{\{ steps.cef.outputs.loader-path \}\}/ && !done { done = 1; next } { print }' "$ci" > "$work/ci-expiry-no-ld-cef.yml"
rejects "CEF cells without the CEF loader path" check_release_expiry "$work/ci-expiry-no-ld-cef.yml"
awk '/^  release-expiry:/ { on = 1 } on && /export EVORCH_RENDERER_EXE=/ && !done { done = 1; next } { print }' "$ci" > "$work/ci-expiry-no-renderer.yml"
rejects "CEF cells without their renderer binary" check_release_expiry "$work/ci-expiry-no-renderer.yml"
awk '/^  release-expiry:/ { on = 1 } on && /CHROME: \$\{\{ steps.chrome.outputs.path \}\}/ && !done { done = 1; next } { print }' "$ci" > "$work/ci-expiry-no-chrome.yml"
rejects "Chromium cells without CHROME" check_release_expiry "$work/ci-expiry-no-chrome.yml"
awk '/^  release-expiry:/ { on = 1 } on && /echo "path=\$CHROME_PATH" >> "\$GITHUB_OUTPUT"/ && !done { done = 1; print "          echo \"CHROME=$CHROME_PATH\" >> \"$GITHUB_ENV\""; next } { print }' "$ci" > "$work/ci-expiry-chrome-env.yml"
rejects "CHROME exported to the whole expiry job" check_release_expiry "$work/ci-expiry-chrome-env.yml"
awk '/^  release-expiry:/ { on = 1 } on && /if-no-files-found: error/ && !done { done = 1; print "          if-no-files-found: warn"; next } { print }' "$ci" > "$work/ci-expiry-evidence.yml"
rejects "an optional expiry evidence upload" check_release_expiry "$work/ci-expiry-evidence.yml"
awk '/^  tests:/ { on = 1 } on && /name: Remaining tests/ && !done { done = 1; print "      - run: scripts/guard-cargo.sh run -p evorch-core --release --features bootstrap-test-support --locked --test core_expiry --run-ignored only" } { print }' "$ci" > "$work/ci-expiry-in-tests.yml"
rejects "the expiry cells back in a runtime shard" check_release_expiry "$work/ci-expiry-in-tests.yml"
awk '/^  browser-e2e:/ { on = 1 } on && /name: Run web workbench browser test/ && !done { done = 1; print "      - run: scripts/guard-cargo.sh run -p evorch-core --release --test core_browser_expiry -E test(=expiry_chromium_t)" } { print }' "$ci" > "$work/ci-expiry-in-browser.yml"
rejects "the Chromium expiry cells back in browser-e2e" check_release_expiry "$work/ci-expiry-in-browser.yml"
mutate ci-expiry-no-sigkill.yml 's/--test core_browser_sigkill/--test core_browser_x/' "$ci"
rejects "a dropped SIGKILL cell" check_release_expiry "$work/ci-expiry-no-sigkill.yml"
mutate ci-expiry-recipe.yml '/prepare "\$warm\/expiry"/d' "$ci"
rejects "an expiry selection cache-save does not build" check_recipe_covers_readers "$work/ci-expiry-recipe.yml"
mutate ci-expiry-recipe-sel.yml '/prepare "\$warm\/expiry"/s/ --test core_browser_expiry//' "$ci"
rejects "an expiry selection cache-save builds differently" check_recipe_covers_readers "$work/ci-expiry-recipe-sel.yml"
awk '/^  [a-z0-9-]+:[[:space:]]*$/ { on = ($1 == "cache-save:") } on && /^          cargo build -p evorch-renderer$/ { next } { print }' "$ci" > "$work/ci-expiry-helper-recipe.yml"
rejects "a renderer build cache-save does not make" check_recipe_covers_readers "$work/ci-expiry-helper-recipe.yml"
awk '/^  release-expiry:/ { on = 1 } on && /shared-key: / && !done { done = 1; sub(/-v2$/, ""); } { print }' "$ci" > "$work/ci-expiry-reader-key.yml"
rejects "release-expiry on another cache key" check_cache_wiring "$work/ci-expiry-reader-key.yml"
awk '/^  release-expiry:/ { on = 1 } on && /save-if: false/ && !done { sub(/false/, "true"); done = 1 } { print }' "$ci" > "$work/ci-expiry-saves.yml"
rejects "release-expiry saving the shared cache" check_cache_wiring "$work/ci-expiry-saves.yml"
awk '/^  release-expiry:/ { on = 1 } on && /CARGO_PROFILE_TEST_DEBUG/ && !done { done = 1; next } { print }' "$ci" > "$work/ci-expiry-env.yml"
rejects "release-expiry without the writer's job-level env" check_cache_wiring "$work/ci-expiry-env.yml"
awk '/^  release-expiry:/ { on = 1 } on && /CARGO_TARGET_DIR: target\/ci-shared/ && !done { done = 1; next } { print }' "$ci" > "$work/ci-expiry-target.yml"
rejects "a release-expiry cargo step outside the shared target" check_e2e_target "$work/ci-expiry-target.yml"
awk '/^  release-expiry:/ { on = 1 } on && /^    needs: scope-gate$/ && !done { done = 1; print "    needs: [scope-gate, product-artifact]"; next } { print }' "$ci" > "$work/ci-expiry-needs.yml"
rejects "release-expiry that needs the producer and waits" check_consumers "$work/ci-expiry-needs.yml"
rejects "release-expiry with other needs" check_scheduling "$work/ci-expiry-needs.yml"
awk '/^  release-expiry:/ { on = 1 } on && /wait-product-artifact.sh check-source/ && !done { done = 1; print "        run: true"; next } { print }' "$ci" > "$work/ci-expiry-nosource.yml"
rejects "release-expiry that skips the source check" check_consumers "$work/ci-expiry-nosource.yml"
awk '/^  release-expiry:/ { on = 1 } on && /^      actions: read$/ && !done { done = 1; next } { print }' "$ci" > "$work/ci-expiry-perm.yml"
rejects "release-expiry without actions: read" check_consumers "$work/ci-expiry-perm.yml"

# release-expiry.sh: the flags and the group definitions.
script=scripts/release-expiry.sh
mutate expiry-no-threads.sh 's/ --test-threads 1//' "$script"
rejects "an expiry run that may use several threads" check_release_expiry_script "$work/expiry-no-threads.sh"
mutate expiry-no-notests.sh 's/ --no-tests fail//' "$script"
rejects "an expiry run that may select no test" check_release_expiry_script "$work/expiry-no-notests.sh"
mutate expiry-no-ignored.sh 's/ --run-ignored only --test-threads/ --test-threads/' "$script"
rejects "an expiry run that skips the ignored cells" check_release_expiry_script "$work/expiry-no-ignored.sh"
mutate expiry-filter.sh 's/ | test(=expiry_cef_stopped_t)//' "$script"
rejects "a CEF group that lost a cell" check_release_expiry_script "$work/expiry-filter.sh"
mutate expiry-compile.sh 's/^  cargo nextest list /  cargo build --release\n  cargo nextest list /' "$script"
rejects "an expiry script that compiles" check_release_expiry_script "$work/expiry-compile.sh"
for cell in expiry_capture_first expiry_signal_first expiry_chromium_t expiry_chromium_stopped_t expiry_cef_t expiry_cef_stopped_t; do
  mutate "expiry-expected-$cell.sh" "/^    evorch-core::core_[a-z_]* $cell \\\\$/d" "$script"
  rejects "an expected-cell list without $cell" check_release_expiry_script "$work/expiry-expected-$cell.sh"
done
mutate expiry-no-expected.sh 's/\[\[ "\$all" != "\$(expected_cells)" \]\]/false/' "$script"
rejects "an inventory check that does not compare the expected cells" check_release_expiry_script "$work/expiry-no-expected.sh"
mutate expiry-evidence.sh 's/-death.json/-x.json/' "$script"
rejects "an expiry script that does not need the death evidence" check_release_expiry_script "$work/expiry-evidence.sh"

# setup-cef: the environment export is opt-out per call and on by default.
action=.github/actions/setup-cef/action.yml
mutate action-default.yml '/^  export-env:/,/^outputs:/ s/default: "true"/default: "false"/' "$action"
rejects "setup-cef not exporting by default" check_setup_cef_action "$work/action-default.yml" "$ci"
mutate action-always.yml 's/^          true)$/          true | false)/' "$action"
rejects "setup-cef exporting for every value" check_setup_cef_action "$work/action-always.yml" "$ci"
mutate action-noout.yml '/^  loader-path:$/d' "$action"
rejects "setup-cef without the loader-path output" check_setup_cef_action "$work/action-noout.yml" "$ci"
mutate action-input.yml 's/^        EXPORT_ENV: .*$/        EXPORT_ENV: true/' "$action"
rejects "setup-cef ignoring its export-env input" check_setup_cef_action "$work/action-input.yml" "$ci"
{ cat "$ci"; printf '#   - uses: ./.github/actions/setup-cef\n#     with:\n#       export-env: "false"\n'; } > "$work/ci-export-twice.yml"
rejects "another job turning the export off" check_setup_cef_action "$action" "$work/ci-export-twice.yml"

# cef-e2e: one job, six invocations, their prerequisites.
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
awk '/^  cef-e2e:/ { on = 1 } on && /cargo build -p browser-cef --features cef --bin evorch-cef-helper/ { n++; if (n == 3) next } { print }' "$ci" > "$work/ci-cef-helper.yml"
rejects "a CEF step without its helper build" check_cef_single "$work/ci-cef-helper.yml"
awk '/^  cef-e2e:/ { on = 1 } on && /scripts\/guard-cargo.sh run -p browser-cef --features cef --run-ignored all/ { hold = $0; next } on && /scripts\/guard-cargo.sh run -p core-client-e2e --features gui\/cef --test preview_cef/ && hold != "" { print; print hold; hold = ""; next } { print }' "$ci" > "$work/ci-cef-order.yml"
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

# Nothing is left to a main push; the removed jobs, suites and scripts stay removed.
awk '/^  cache-save:/ { print "  offscreen-gate:"; print "    runs-on: ubuntu-latest"; print "    steps: []" } { print }' "$ci" > "$work/ci-main-offscreen.yml"
rejects "an offscreen-gate job back" check_every_event "$work/ci-main-offscreen.yml"
awk '/^  tests:/ { on = 1 } on && /^    needs: scope-gate$/ && !done { print; print "    if: github.event_name == '"'"'push'"'"'"; done = 1; next } { print }' "$ci" > "$work/ci-main-tests.yml"
rejects "the tests job left to a push" check_every_event "$work/ci-main-tests.yml"
mutate ci-main-remaining.yml 's/ remaining "\$EVORCH_TEST_INVENTORY_DIR"/ ci-remaining "$EVORCH_TEST_INVENTORY_DIR"/' "$ci"
rejects "the shards running a removed suite" check_every_event "$work/ci-main-remaining.yml"
{ cat "$ci"; printf '# main-only-gui\n'; } > "$work/ci-main-name.yml"
rejects "the main-only-gui suite named again" check_every_event "$work/ci-main-name.yml"
mutate ci-main-coe.yml 's/^\(  *\)- name: Remaining tests$/\1- name: Remaining tests\n\1  continue-on-error: true/' "$ci"
rejects "a test step that continues on error" check_every_event "$work/ci-main-coe.yml"
{ cat "$ci"; printf '# gui-pr-preview\n'; } > "$work/ci-preview.yml"
rejects "the pull request screenshots producer" check_every_event "$work/ci-preview.yml"

mutate ci-cancel.yml 's/cancel-in-progress: .*/cancel-in-progress: true/' "$ci"
rejects "cancelling main runs" check_scheduling "$work/ci-cancel.yml"

if ((fails > 0)); then
  echo "$fails failure(s)" >&2
  exit 1
fi
echo "ci workflow configuration: passed"
