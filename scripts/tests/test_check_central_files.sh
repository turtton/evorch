#!/usr/bin/env bash
# Tests for scripts/check-central-files.sh and scripts/check-core-client-refs.sh
# with hand-written change lists and source trees; no cargo and no git history.
set -euo pipefail
cd "$(dirname "$0")/../.."
central="$PWD/scripts/check-central-files.sh"
refs="$PWD/scripts/check-core-client-refs.sh"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

fails=0
expect() { # <ok|fail> <description> <command...>
  local want=$1 what=$2
  shift 2
  if "$@" > "$work/out" 2>&1; then got=ok; else got=fail; fi
  if [[ "$got" != "$want" ]]; then
    echo "FAIL: $what (wanted $want, got $got)" >&2
    sed 's/^/  | /' "$work/out" >&2
    fails=$((fails + 1))
  fi
}
files() { printf '%s\n' "$@" > "$work/files"; }

"$central" --check-data > /dev/null || { echo "FAIL: the data files are inconsistent" >&2; exit 1; }

# A feature PR inside its own area passes.
files crates/workbench-host/src/areas/settings.rs \
  crates/workbench-host/src/areas/settings/save.rs \
  crates/workbench-protocol/src/namespaces/settings.rs \
  crates/workbench-protocol/tests/golden/areas/settings.json \
  crates/gui/src/core_client/features/auth.rs \
  crates/evorch-core/tests/core_settings_cache.rs
expect ok "pr10 changes only its own files" "$central" --scope pr10 --files-from "$work/files"

# A central file is refused whatever the scope would allow.
for central_file in crates/workbench-host/src/areas/mod.rs Cargo.lock \
  crates/workbench-protocol/src/namespaces/mod.rs crates/gui/src/core_client/features/mod.rs \
  scripts/contract-test-filters.sh crates/evorch/src/main.rs; do
  files crates/workbench-host/src/areas/settings.rs "$central_file"
  expect fail "$central_file is central" "$central" --scope pr10 --files-from "$work/files"
done
# The shared golden files are central, the golden files of an area are not.
files crates/workbench-protocol/tests/golden/commands.json
expect fail "a shared golden file is central" "$central" --scope pr10 --files-from "$work/files"

# Another PR's area is out of scope; so is an unrelated file.
files crates/workbench-host/src/areas/memory.rs
expect fail "pr10 does not own memory" "$central" --scope pr10 --files-from "$work/files"
expect ok "pr13 owns memory" "$central" --scope pr13 --files-from "$work/files"
files crates/runtime/src/lib.rs
expect fail "a file outside every scope" "$central" --scope pr13 --files-from "$work/files"

# 17d and 17e do not share a parser.
files crates/evorch/src/backfill.rs
expect ok "17d owns the online backfill" "$central" --scope pr17d --files-from "$work/files"
expect fail "17e does not own the online backfill" "$central" --scope pr17e --files-from "$work/files"
files crates/evorch/src/maintenance.rs
expect ok "17e owns the offline tool" "$central" --scope pr17e --files-from "$work/files"
expect fail "17d does not own the offline tool" "$central" --scope pr17d --files-from "$work/files"

# An integration PR may change central files.
files Cargo.lock crates/workbench-host/src/areas/mod.rs
expect ok "an integration PR may change central files" "$central" --scope integration --files-from "$work/files"
expect fail "an unknown scope" "$central" --scope nonsense --files-from "$work/files"

# core-client references: a new one fails, the baseline passes.
mkdir -p "$work/src/features"
printf 'use workbench_protocol::ids::*;\nuse tokio::runtime::Builder;\n' > "$work/src/mod.rs"
printf '# none\n' > "$work/baseline.txt"
expect ok "no internal crate in core_client" "$refs" --root "$work/src" --baseline "$work/baseline.txt"
printf 'use storage::Database;\n' > "$work/src/features/x.rs"
expect fail "a new internal crate reference" "$refs" --root "$work/src" --baseline "$work/baseline.txt"
printf '%s 1\n' "$work/src/features/x.rs" > "$work/baseline.txt"
expect ok "a baselined reference" "$refs" --root "$work/src" --baseline "$work/baseline.txt"
printf 'use storage::A;\nuse runtime::B;\n' > "$work/src/features/x.rs"
expect fail "more references than the baseline" "$refs" --root "$work/src" --baseline "$work/baseline.txt"
expect ok "the real tree is within its baseline" "$refs"

if (( fails > 0 )); then
  echo "$fails check(s) failed" >&2
  exit 1
fi
echo "central file and core-client reference checks behave as documented"
