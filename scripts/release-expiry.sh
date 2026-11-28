#!/usr/bin/env bash
# The real planned-shutdown expiry cells of the `release-expiry` CI job: the
# release test binaries `core_expiry` and `core_browser_expiry` of evorch-core
# (B3-07, B3-08b). They are ignored tests, so no debug inventory run selects
# them; this is their only run. They are not product-ELF checks (5a-V T3).
#
# Usage: scripts/release-expiry.sh check-inventory <metadata-dir>
#        scripts/release-expiry.sh run <metadata-dir> <group> <evidence-root>
#
# <metadata-dir> is the output of `scripts/guard-cargo.sh prepare` for both test
# binaries (release profile), made before the sealed product artifact is
# installed. Nothing here compiles: `run` executes the prepared binaries through
# scripts/guard-cargo.sh, which binds them to the sealed helper bundle.
#
# The cells are partitioned into groups by the environment they need:
#   core      the two worker cells of core_expiry
#   chromium  the two Chromium cells of core_browser_expiry (CHROME, certutil)
#   cef       the two CEF cells of core_browser_expiry (the real renderer and
#             helper, EVORCH_RENDERER_EXE, CEF on the loader path)
#
# check-inventory   Lists the ignored tests of both binaries and fails unless they
#                   are exactly the six expected cells (expected_cells below) and
#                   the groups are non-empty, disjoint and together exactly that
#                   list: a cell that vanishes, one nobody selects and one selected
#                   twice are all errors.
#                   It writes <metadata-dir>/cells/<group> (one cell per line).
# run               Runs one group with `--run-ignored only --test-threads 1
#                   --no-tests fail` (the nextest timeout overrides of
#                   .config/nextest.toml own hang detection) and then requires
#                   the evidence of every cell of the group: one death JSON per
#                   cell in <evidence-root>/<group>. In CI a group other than
#                   cef must not see the CEF distribution on its environment.
set -euo pipefail
cd "$(dirname "$0")/.."

guard_cargo=${EVORCH_RELEASE_EXPIRY_GUARD_CARGO:-scripts/guard-cargo.sh}
groups=(core chromium cef)

# The ignored cells the binaries must list, `binary-id<TAB>test`: all six, by name.
# A cell that vanishes (or appears) is a change to make here, not a silent pass.
expected_cells() {
  printf '%s\t%s\n' \
    evorch-core::core_expiry expiry_capture_first \
    evorch-core::core_expiry expiry_signal_first \
    evorch-core::core_browser_expiry expiry_chromium_t \
    evorch-core::core_browser_expiry expiry_chromium_stopped_t \
    evorch-core::core_browser_expiry expiry_cef_t \
    evorch-core::core_browser_expiry expiry_cef_stopped_t \
    | LC_ALL=C sort
}

die() { echo "release-expiry: $*" >&2; exit 1; }

filter_of() { # <group>: the nextest filterset of the group
  case "$1" in
    core) echo 'binary(=core_expiry)' ;;
    chromium) echo 'binary(=core_browser_expiry) & (test(=expiry_chromium_t) | test(=expiry_chromium_stopped_t))' ;;
    cef) echo 'binary(=core_browser_expiry) & (test(=expiry_cef_t) | test(=expiry_cef_stopped_t))' ;;
    *) echo "unknown group: $1 (expected one of: ${groups[*]})" >&2; return 2 ;;
  esac
}

# The tests nextest selects from the prepared metadata, one `binary<TAB>test`
# line each, sorted. Listing executes the test binaries with --list only.
list_cells() { # <metadata-dir> [filterset]
  local dir=$1 extra=()
  [[ -z "${2:-}" ]] || extra=(-E "$2")
  cargo nextest list --cargo-metadata "$dir/cargo.json" --binaries-metadata "$dir/default-binaries.json" \
    --run-ignored only "${extra[@]}" --message-format json \
    | jq -r '
        .["rust-suites"][] | . as $suite
        | .testcases | to_entries[]
        | select(.value["filter-match"].status == "matches")
        | [$suite["binary-id"], .key] | @tsv' | LC_ALL=C sort
}

check_inventory() { # <metadata-dir>
  local dir=${1:?expected the metadata directory} group all union
  [[ -f "$dir/cargo.json" && -f "$dir/default-binaries.json" ]] || die "no prepared metadata in $dir"
  rm -rf "$dir/cells"
  mkdir -p "$dir/cells"
  all=$(list_cells "$dir")
  [[ -n "$all" ]] || die 'the release test binaries list no ignored cell'
  echo "ignored cells of core_expiry and core_browser_expiry:"
  sed 's/^/  /' <<< "$all"
  for group in "${groups[@]}"; do
    list_cells "$dir" "$(filter_of "$group")" > "$dir/cells/$group.tsv"
    [[ -s "$dir/cells/$group.tsv" ]] || die "group $group selects no cell"
    cut -f2 "$dir/cells/$group.tsv" > "$dir/cells/$group"
    echo "group $group: $(wc -l < "$dir/cells/$group" | tr -d ' ') cells"
  done
  union=$(cat "$dir"/cells/*.tsv | LC_ALL=C sort)
  if [[ -n "$(sort <<< "$union" | uniq -d)" ]]; then
    echo "cells selected by more than one group:" >&2
    sort <<< "$union" | uniq -d | sed 's/^/  /' >&2
    die 'the groups overlap'
  fi
  if [[ "$union" != "$all" ]]; then
    echo "cells no group selects:" >&2
    comm -23 <(sort <<< "$all") <(sort <<< "$union") | sed 's/^/  /' >&2
    echo "cells a group selects that the binaries do not list:" >&2
    comm -13 <(sort <<< "$all") <(sort <<< "$union") | sed 's/^/  /' >&2
    die 'the groups do not cover the ignored cells exactly'
  fi
  echo "the groups cover the $(wc -l <<< "$all" | tr -d ' ') ignored cells exactly once"
  if [[ "$all" != "$(expected_cells)" ]]; then
    echo "expected cells the binaries do not list:" >&2
    comm -13 <(sort <<< "$all") <(expected_cells) | sed 's/^/  /' >&2
    echo "cells the binaries list that are not expected:" >&2
    comm -23 <(sort <<< "$all") <(expected_cells) | sed 's/^/  /' >&2
    die 'the ignored cells are not the expected six'
  fi
}

run_group() { # <metadata-dir> <group> <evidence-root>
  local dir=${1:?expected the metadata directory} group=${2:?expected a group} root=${3:?expected the evidence root}
  local filter cell
  filter=$(filter_of "$group")
  [[ -s "$dir/cells/$group" ]] || die "no cell list for $group in $dir: run check-inventory first"
  echo "group $group: LD_LIBRARY_PATH=${LD_LIBRARY_PATH:-<unset>} CEF_PATH=${CEF_PATH:-<unset>}"
  if [[ -n "${GITHUB_ACTIONS:-}" && "$group" != cef ]]; then
    if [[ "${LD_LIBRARY_PATH:-}" == *cef* || -n "${CEF_PATH:-}" ]]; then
      die "group $group must not see the CEF distribution (LD_LIBRARY_PATH, CEF_PATH)"
    fi
  fi
  export EVORCH_B3_EVIDENCE_DIR="$root/$group"
  rm -rf "$EVORCH_B3_EVIDENCE_DIR"
  mkdir -p "$EVORCH_B3_EVIDENCE_DIR"
  "$guard_cargo" run --cargo-metadata "$dir/cargo.json" --binaries-metadata "$dir/default-binaries.json" \
    -E "$filter" --run-ignored only --test-threads 1 --success-output immediate --no-tests fail
  while read -r cell; do
    compgen -G "$EVORCH_B3_EVIDENCE_DIR/*-$cell-*-death.json" > /dev/null \
      || die "cell $cell passed without its death evidence in $EVORCH_B3_EVIDENCE_DIR"
  done < "$dir/cells/$group"
  echo "group $group: $(wc -l < "$dir/cells/$group" | tr -d ' ') cells passed with evidence"
}

case "${1:-}" in
  check-inventory) shift; check_inventory "$@" ;;
  run) shift; run_group "$@" ;;
  *) echo "usage: release-expiry.sh check-inventory <metadata-dir> | run <metadata-dir> <group> <evidence-root>" >&2; exit 2 ;;
esac
