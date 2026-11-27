#!/usr/bin/env bash
# The capture job's step summary: what was compared and how long each step took.
#
# Usage: summary.sh <work-dir>/out
set -euo pipefail
out=${1:?out directory}

echo "### New GUI screen diff"
echo
if [[ -f "$out/meta.json" ]]; then
  jq -r '"before `\(.before_sha[0:12])` (the base), after `\(.after_sha[0:12])` (the merge result); baseline: \(if .baseline then "yes" else "none, every screen is new" end)"' "$out/meta.json"
  echo
fi
if [[ -f "$out/compare/report.json" ]]; then
  echo "| screen | result |"
  echo "|---|---|"
  jq -r '.entries[] | "| \(.id) | \(.status)\(if .status == "changed" then " (\(.changed_pixels) pixels)" else "" end) |"' "$out/compare/report.json"
  echo
fi
if [[ -s "$out/timings.tsv" ]]; then
  echo "| step | seconds |"
  echo "|---|---|"
  awk -F'\t' '{ printf "| %s | %s |\n", $1, $2 }' "$out/timings.tsv"
fi
