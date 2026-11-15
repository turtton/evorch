#!/usr/bin/env bash
# The core-client desktop (crates/gui/src/core_client) reaches the core only
# through the protocol (ADR 0045). This counts, per file, the references to an
# internal crate (storage, runtime, sandbox, ...) and fails when a file has more
# than scripts/core-client-refs-baseline.txt allows. The baseline only shrinks:
# lower a count in the PR that removes the references.
#
# The references are found by scripts/check-core-client-refs.pl, which reads the
# `use` declarations as trees (multi-line groups, `as` aliases, `::crate::`
# paths, `extern crate`) instead of matching lines of text; see that file.
#
# Usage: scripts/check-core-client-refs.sh [--root DIR] [--baseline FILE]
set -euo pipefail
cd "$(dirname "$0")/.."

root=crates/gui/src/core_client
baseline=scripts/core-client-refs-baseline.txt
while [[ $# -gt 0 ]]; do
  case "$1" in
    --root) root="${2:?--root needs a directory}"; shift 2 ;;
    --baseline) baseline="${2:?--baseline needs a file}"; shift 2 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

# `browser` is left out: it is also the name of a feature module.
internal='storage|runtime|sandbox|tools|providers|routing|config|eval|event_bus|recorder|agents|catalog|browser_cef|model|secret_guard|workbench_host'

declare -A allowed
while read -r file count; do
  [[ -z "${file:-}" || "$file" == '#'* ]] && continue
  allowed[$file]=$count
done < "$baseline"

mapfile -t files < <(find "$root" -name '*.rs' | LC_ALL=C sort)
hits_file=$(mktemp)
trap 'rm -f "$hits_file"' EXIT
if (( ${#files[@]} > 0 )); then
  perl scripts/check-core-client-refs.pl "$internal" "${files[@]}" > "$hits_file"
fi

failures=0
for file in "${files[@]}"; do
  count=$(awk -F'\t' -v f="$file" '$1 == f { n++ } END { print n + 0 }' "$hits_file")
  limit=${allowed[$file]:-0}
  if (( count > limit )); then
    echo "FAIL: $file has $count reference(s) to internal crates; the baseline allows $limit. Reach the core through the protocol:" >&2
    awk -F'\t' -v f="$file" '$1 == f { printf "  %s:%s: %s\n", $1, $2, $3 }' "$hits_file" >&2
    failures=$((failures + 1))
  elif (( count < limit )); then
    echo "warning: $file is down to $count reference(s); lower its baseline from $limit" >&2
  fi
done
for file in "${!allowed[@]}"; do
  [[ -f "$file" ]] || echo "warning: baseline names a file that does not exist: $file" >&2
done
[[ "$failures" == 0 ]] || exit 1
echo "core-client references are within the baseline"
