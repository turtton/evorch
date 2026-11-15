#!/usr/bin/env bash
# Tests for scripts/check-central-files.sh, scripts/ci-scope-gate.sh and
# scripts/check-core-client-refs.sh with hand-written change lists, source trees
# and a throwaway git repository; no cargo.
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

# A scope pattern is not expanded against the files that exist. The sandbox is a
# git repository with real files under the patterns: an unquoted loop would
# replace `offline*` with the names it finds and refuse the nested ones.
repo="$work/repo"
mkdir -p "$repo/scripts" "$repo/crates/storage/src/maintenance/offline" "$repo/docs/config"
cp scripts/check-central-files.sh scripts/central-files.txt scripts/feature-scopes.txt "$repo/scripts/"
printf x > "$repo/crates/storage/src/maintenance/offline.rs"
printf x > "$repo/crates/storage/src/maintenance/offline/lease_tests.rs"
printf x > "$repo/docs/config/evorch-core.md"
sandbox_git() { git -C "$repo" -c user.name=fixture -c user.email=fixture@example.invalid -c commit.gpgsign=false "$@"; }
sandbox_git init -q -b main
sandbox_git add -A
sandbox_git commit -q -m base
sandbox_git checkout -q -b feature
printf y > "$repo/crates/storage/src/maintenance/offline/lease_tests.rs"
printf y > "$repo/docs/config/evorch-core.md"
sandbox_git commit -q -am change
sandbox_check() { (cd "$repo" && scripts/check-central-files.sh "$@"); }
expect ok "a nested file under a glob scope is in scope (pr17e)" sandbox_check --scope pr17e --base main
expect fail "pr10 does not own the nested offline file" sandbox_check --scope pr10 --base main
# A base that cannot be diffed is an error, never an empty change set.
expect fail "an unknown base is an error" sandbox_check --scope pr17e --base no-such-ref
expect fail "an unknown base is an error for an integration PR" sandbox_check --scope integration --base no-such-ref
expect fail "an unreadable file list is an error" sandbox_check --scope pr17e --files-from "$work/missing"
expect fail "an unknown scope in a real diff" sandbox_check --scope nonsense --base main

# The CI gate derives the scope from the head branch.
gate="$PWD/scripts/ci-scope-gate.sh"
scope_is() { [[ "$("$gate" --scope-of "$1")" == "$2" ]]; }
expect ok "feat/pr10-settings is pr10" scope_is feat/pr10-settings pr10
expect ok "feat/pr17e is pr17e" scope_is feat/pr17e pr17e
expect ok "feat/pr16a1-measure is pr16a1" scope_is feat/pr16a1-measure pr16a1
expect ok "feat/s0-integration is an integration PR" scope_is feat/s0-integration integration
expect ok "feat/integration-s1 is an integration PR" scope_is feat/integration-s1 integration
expect ok "a branch without a scope is not checked" scope_is fix/gui-paging ""
expect ok "a branch without a type is not checked" scope_is pr10 ""
cp scripts/ci-scope-gate.sh "$repo/scripts/"
sandbox_git branch -m feat/pr10-settings
expect fail "the gate refuses a pr10 branch that changes pr17e files" bash -c "cd '$repo' && scripts/ci-scope-gate.sh --head-ref feat/pr10-settings --base main"
expect ok "the gate accepts a pr17e branch for the same files" bash -c "cd '$repo' && scripts/ci-scope-gate.sh --head-ref feat/pr17e-offline --base main"
expect ok "the gate leaves an unscoped branch alone" bash -c "cd '$repo' && scripts/ci-scope-gate.sh --head-ref fix/other --base main"
expect fail "the gate fails when the base is missing" bash -c "cd '$repo' && scripts/ci-scope-gate.sh --head-ref feat/pr17e-offline --base origin/main"

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

# Every spelling Rust accepts is found, and look-alikes are not.
printf '' > "$work/baseline.txt"
ref_case() { # <ok|fail> <description> <source line>
  printf '%s\n' "$3" > "$work/src/features/x.rs"
  expect "$1" "$2" "$refs" --root "$work/src" --baseline "$work/baseline.txt"
}
ref_case fail "an absolute path" 'use ::storage::Database;'
ref_case fail "an absolute path in an expression" 'let d = ::runtime::Runtime::new();'
ref_case fail "an alias" 'use runtime as rt;'
ref_case fail "a bare use" 'use storage;'
ref_case fail "a use list" 'use {tools, serde};'
ref_case fail "a use list with a later crate" 'use {serde, sandbox as sb};'
ref_case fail "an extern crate" 'extern crate providers;'
ref_case fail "a nested path" 'let _ = storage::db::open();'
ref_case ok "tokio runtime" 'use tokio::runtime::Builder;'
ref_case ok "an absolute tokio runtime" 'use ::tokio::runtime::Builder;'
ref_case ok "a local module of the same name" 'use super::config;'
ref_case ok "an identifier that only contains a crate name" 'let storage_path = 1; let runtime_id = 2;'
ref_case ok "a comment word" '// the storage layer is behind the protocol'
ref_case fail "a multi-line group with an alias" 'use {
    runtime as rt,
    serde,
};'
ref_case fail "a nested group" 'use {serde, storage::{db, Database}};'
ref_case fail "a path through an alias is counted" 'use runtime as rt;
fn f() { rt::AgentRuntime::new(); }'
ref_case fail "an absolute path in an expression" 'fn f() { let d = ::storage::open(); }'
ref_case fail "a self import with an alias" 'use storage::{self as s};
fn f() { s::open(); }'
ref_case ok "a tokio alias in a group" 'use {tokio::runtime as rt, serde};
fn f() { let _ = rt::Builder::new_multi_thread(); }'
ref_case ok "a local module in a group" 'use {super::config, serde};
fn f() { config::load(); }'
ref_case ok "a multi-line group of other crates" 'use {
    serde::Serialize,
    tokio::runtime::Handle,
};'
ref_case ok "a crate named in a string and a comment" 'const A: &str = "use storage::Db;"; // use runtime as rt;
/* use sandbox::X; */'
ref_case ok "a raw string" 'const A: &str = r#"use storage::Db;"#;'
ref_case ok "a method turbofish" 'fn f(x: X) { x.config::<u8>(); }'
ref_case ok "a lifetime and a character literal" "fn f<'a>(x: &'a str) -> char { '''; '\\'' }"

if (( fails > 0 )); then
  echo "$fails check(s) failed" >&2
  exit 1
fi
echo "central file and core-client reference checks behave as documented"
