#!/usr/bin/env bash
# The core-client desktop (crates/gui/src/core_client) reaches the core only
# through the protocol (ADR 0045). This counts, per file, the lines that name an
# internal crate path (storage::, runtime::, sandbox::, ...) and fails when a
# file has more than scripts/core-client-refs-baseline.txt allows. The baseline
# only shrinks: lower a count in the PR that removes the references.
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

# A reference to one of these crates, in any form Rust accepts (`browser` is left
# out: it is also the name of a feature module):
#   storage::X            a path from the crate (not tokio::runtime::, whose name
#                         follows a `:` or an identifier character)
#   ::storage::X          an absolute path
#   use storage;  use storage as s;  use storage, ...;  use {storage, x};
#   extern crate storage;
internal='(storage|runtime|sandbox|tools|providers|routing|config|eval|event_bus|recorder|agents|catalog|browser_cef|model|secret_guard|workbench_host)'
path_ref="(^|[^[:alnum:]_:])(::)?${internal}::"
absolute_ref="(^|[^[:alnum:]_>])::${internal}([^[:alnum:]_]|$)"
use_ref="(^|[^[:alnum:]_])(use|extern[[:space:]]+crate)[[:space:]]+(::)?(\{([^}]*[^[:alnum:]_])?)?${internal}([[:space:]]*(;|,|\}|[[:space:]]as[[:space:]])|[[:space:]]+as[[:space:]])"
pattern="(${path_ref})|(${absolute_ref})|(${use_ref})"

declare -A allowed
while read -r file count; do
  [[ -z "${file:-}" || "$file" == '#'* ]] && continue
  allowed[$file]=$count
done < "$baseline"

failures=0
while IFS= read -r file; do
  count=$(grep -Ec "$pattern" "$file" || true)
  limit=${allowed[$file]:-0}
  if (( count > limit )); then
    echo "FAIL: $file names internal crates on $count line(s); the baseline allows $limit. Reach the core through the protocol:" >&2
    grep -nE "$pattern" "$file" >&2 || true
    failures=$((failures + 1))
  elif (( count < limit )); then
    echo "warning: $file is down to $count line(s); lower its baseline from $limit" >&2
  fi
done < <(find "$root" -name '*.rs' | LC_ALL=C sort)
for file in "${!allowed[@]}"; do
  [[ -f "$file" ]] || echo "warning: baseline names a file that does not exist: $file" >&2
done
[[ "$failures" == 0 ]] || exit 1
echo "core-client references are within the baseline"
