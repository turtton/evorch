#!/usr/bin/env bash
# The capture job's work: draw the new GUI's screens at the base side and at the PR's
# merge result, in this one job, and compare them.
#
# Usage: capture.sh <before-sha> <after-sha> <work-dir>
#   before = the first parent of the PR's merge commit (the base), after = the merge commit.
#   Run it from the root of a checkout that has both commits.
#
# Writes <work-dir>/out:
#   before/run1, before/run2, after/run1, after/run2   two captures per side
#   compare/                                           report.json and the images to show
#   meta.json, timings.tsv
# Environment: FONT_FILE (the one CJK font file), CARGO_TARGET_DIR (relative to the root).
#
# Each side is built from its own commit with its own capture tool. When the before
# commit has no capture tool, nothing is built for it and every screen is reported as
# new ("no baseline"). Everything a side needs comes from that side's checkout; only this
# script's own directory is copied out first, so the later checkout does not change it.
set -euo pipefail

before=${1:?before sha}
after=${2:?after sha}
work=$(realpath -m "${3:?work directory}")
font=${FONT_FILE:?FONT_FILE}
target=${CARGO_TARGET_DIR:-target}
tool=crates/gui/src/bin/core_client_capture.rs
root=$(pwd)

rm -rf "$work/out" "$work/tools"
mkdir -p "$work/out"
cp -r "$root/scripts/core-gui-visual" "$work/tools"
timings="$work/out/timings.tsv"
: > "$timings"

# A step and the seconds it took, for the report of the capture job's cost.
timed() {
  local label=$1 start end
  shift
  start=$(date +%s)
  "$@"
  end=$(date +%s)
  printf '%s\t%s\n' "$label" "$((end - start))" >> "$timings"
}

# Same package and feature selection as the gui test group, so that the dependency
# cache the other jobs share is the one restored here.
selection() {
  jq -r '.groups.gui | [(.packages[] | "-p " + .), (.features[]? | "-F " + .)] | join(" ")' scripts/ci-test-groups.json
}

build() {
  local sel args=(--bin core_client_capture)
  sel=$(selection)
  # Naming one small integration test makes Cargo resolve features with the dev
  # dependencies, as the gui group's test build does; the base may not have it yet.
  [[ -f crates/gui/tests/core_client_visual.rs ]] && args+=(--test core_client_visual)
  # shellcheck disable=SC2086 # $sel is a list of options
  cargo build --locked $sel "${args[@]}"
}

side() { # label sha
  local label=$1 sha=$2
  git checkout -q --detach "$sha"
  timed "build-$label" build
  cp "$root/$target/debug/core_client_capture" "$work/bin-$label"
  for n in 1 2; do
    timed "capture-$label-$n" "$work/tools/run-capture.sh" \
      "$work/bin-$label" "$work/out/$label/run$n" "$work/scratch-$label-$n" "$font"
  done
}

has_baseline=false
if git cat-file -e "$before:$tool" 2>/dev/null; then
  has_baseline=true
  side before "$before"
fi
side after "$after"

# The compare and the repeat checks use the after side's tool: it is the one this PR ships.
bin="$work/bin-after"
timed verify "$bin" identical --first "$work/out/after/run1" --second "$work/out/after/run2"
compare_args=(--after "$work/out/after/run1" --out "$work/out/compare")
if [[ "$has_baseline" == true ]]; then
  "$bin" identical --first "$work/out/before/run1" --second "$work/out/before/run2"
  compare_args+=(--before "$work/out/before/run1")
fi
timed compare "$bin" compare "${compare_args[@]}"

jq -n --arg before "$before" --arg after "$after" --argjson baseline "$has_baseline" \
  '{before_sha: $before, after_sha: $after, baseline: $baseline}' > "$work/out/meta.json"
echo "capture done: baseline=$has_baseline"
