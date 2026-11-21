#!/usr/bin/env bash
# The gui integration tests that only a main push runs (suite `main-only-gui`).
# scripts/ci-main-only-gui-tests.json registers them one integration-test binary
# at a time; this script is the one place that reads it.
#
# Usage: scripts/ci-main-only-gui.sh [--manifest FILE] [--metadata FILE] <command>
#   filter         print the nextest filter that selects exactly the registered
#                  binaries (package, kind and binary name, all exact matches)
#   binaries       print the nextest binary id of every registered binary
#   validate       the registry is well formed, and every registered binary is an
#                  integration test of the gui package that cargo discovered
#                  (tests/<name>.rs or tests/<name>/main.rs)
#   check-sources  validate, then require that no source of a registered binary
#                  names `core_client`
# --metadata FILE is a `cargo metadata --no-deps` document (default: run cargo).
#
# What a registration is. Only a binary: never a test name, a module selector or a
# filter expression, so a test added to, renamed in or removed from a registered
# binary needs no change here, and a binary that is not registered stays in the
# pull request run. The binary's kind, source path and nextest id are derived from
# Cargo, not repeated in the registry. Registering anything but an integration test
# of the gui package is rejected.
#
# What check-sources reads. The root file of the binary, every .rs file under its
# directory (`tests/<name>/`), and every file that code of those names with
# `#[path = "..."]` or `include!("...")`, transitively. A reference that cannot be
# resolved to an existing file is a failure, not a pass. Comments are dropped
# first. `include_str!` and `include_bytes!` read text, not code, and are not
# followed. This is a guard against the new core-client desktop (ADR 0045) entering
# a binary that a pull request no longer runs; it does not prove that a registered
# test is not a core-client test, so a test of the new system goes into a binary
# that is not registered.
set -euo pipefail
cd "$(dirname "$0")/.."

manifest=scripts/ci-main-only-gui-tests.json
metadata=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --manifest) manifest="${2:?--manifest needs a file}"; shift 2 ;;
    --metadata) metadata="${2:?--metadata needs a file}"; shift 2 ;;
    -*) echo "unknown option: $1" >&2; exit 2 ;;
    *) break ;;
  esac
done
command_name="${1:?expected filter, binaries, validate or check-sources}"
command -v jq > /dev/null || { echo "ci-main-only-gui: jq is required" >&2; exit 2; }

fail() { echo "FAIL: $*" >&2; exit 1; }

check_manifest() {
  [[ -f "$manifest" ]] || fail "$manifest does not exist"
  jq -e '
    if type != "object" then error("the registry must be an object") else . end
    | if (keys | sort) != ["binaries", "package"] then error("the registry has exactly the keys package and binaries") else . end
    | if .package != "gui" then error("package must be gui") else . end
    | if (.binaries | type) != "array" or (.binaries | length) == 0 then error("binaries must be a non-empty array") else . end
    | (.binaries | map(
        if type != "object" or ((keys | sort) != ["reason", "target"]) then error("each binary has exactly the keys target and reason") else . end
        | if (.target | type) != "string" or ((.target | test("^[a-z][a-z0-9_]*$")) | not) then error("a target is a lowercase integration-test name") else . end
        | if (.reason | type) != "string" or (.reason | length) == 0 then error("every registration needs a reason") else . end)
        | length) as $registered
    | if ([.binaries[].target] | length) != ([.binaries[].target] | unique | length) then error("a binary is registered twice") else . end
    | true' "$manifest" > /dev/null || fail "$manifest is not a valid registry"
}

targets() { jq -r '.binaries[].target' "$manifest" | LC_ALL=C sort; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

load_metadata() {
  if [[ -n "$metadata" ]]; then
    cp "$metadata" "$work/metadata.json"
  else
    cargo metadata --no-deps --format-version 1 --locked > "$work/metadata.json"
  fi
  tests_dir=$(jq -er '.packages[] | select(.name == "gui") | .manifest_path | sub("/Cargo.toml$"; "/tests")' "$work/metadata.json") \
    || fail "the metadata has no gui package"
}

# Sets src to the source path of the registered binary, or fails.
source_of() {
  local target=$1 found kind
  found=$(jq -r --arg t "$target" '
    [.packages[] | select(.name == "gui") | .targets[] | select(.name == $t)] as $m
    | if ($m | length) != 1 then "none" else ($m[0] | (.kind | join(",")) + "\t" + .src_path) end' "$work/metadata.json")
  [[ "$found" != none ]] || fail "registered binary $target is not a target of the gui package"
  kind=${found%%$'\t'*}
  [[ "$kind" == test ]] || fail "registered binary $target has kind '$kind': only integration tests (kind test) can be registered"
  src=${found#*$'\t'}
  [[ "$src" == "$tests_dir/$target.rs" || "$src" == "$tests_dir/$target/main.rs" ]] \
    || fail "registered binary $target has source $src: expected $tests_dir/$target.rs or $tests_dir/$target/main.rs"
  [[ -f "$src" ]] || fail "registered binary $target: $src does not exist"
}

# Source text without comments (strings are respected, so a URL stays intact).
strip_comments() {
  awk '
    BEGIN { in_block = 0 }
    {
      line = $0; out = ""; n = length(line); in_str = 0; i = 1
      while (i <= n) {
        c = substr(line, i, 1); d = substr(line, i, 2)
        if (in_block) { if (d == "*/") { in_block = 0; i += 2 } else { i++ }; continue }
        if (in_str) {
          out = out c
          if (c == "\\") { out = out substr(line, i + 1, 1); i += 2; continue }
          if (c == "\"") in_str = 0
          i++; continue
        }
        if (d == "//") break
        if (d == "/*") { in_block = 1; i += 2; continue }
        if (c == "\"") in_str = 1
        out = out c; i++
      }
      print out
    }' "$1"
}

check_sources() {
  local target src base file ref resolved dir stripped hits n_path n_path_ok n_inc n_inc_ok files_total=0
  local -a queue
  declare -A seen
  for target in $(targets); do
    source_of "$target"
    queue=()
    seen=()
    add() { if [[ -z "${seen[$1]:-}" ]]; then seen[$1]=1; queue+=("$1"); fi; }
    add "$src"
    if [[ "${src##*/}" == main.rs ]]; then base=${src%/*}; else base=${src%.rs}; fi
    if [[ -d "$base" ]]; then
      while IFS= read -r -d '' file; do add "$file"; done < <(find "$base" -type f -name '*.rs' -print0 | LC_ALL=C sort -z)
    fi
    while ((${#queue[@]} > 0)); do
      file=${queue[0]}
      queue=("${queue[@]:1}")
      files_total=$((files_total + 1))
      stripped="$work/stripped.rs"
      strip_comments "$file" > "$stripped"
      hits=$(grep -nE '(^|[^A-Za-z0-9_])core_client([^A-Za-z0-9_]|$)' "$stripped" || true)
      if [[ -n "$hits" ]]; then
        echo "FAIL: binary $target: $file names core_client:" >&2
        sed 's/^/  /' <<< "$hits" >&2
        exit 1
      fi
      dir=${file%/*}
      n_path=$(grep -cE '#\[path' "$stripped" || true)
      n_path_ok=$({ grep -oE '#\[path *= *"[^"]*" *\]' "$stripped" || true; } | wc -l | tr -d ' ')
      n_inc=$({ grep -oE '(^|[^A-Za-z0-9_])include!' "$stripped" || true; } | wc -l | tr -d ' ')
      n_inc_ok=$({ grep -oE '(^|[^A-Za-z0-9_])include! *\( *"[^"]*" *\)' "$stripped" || true; } | wc -l | tr -d ' ')
      if [[ "$n_path" != "$n_path_ok" ]]; then
        fail "binary $target: $file has a #[path] that is not a string literal; its target cannot be checked"
      fi
      if [[ "$n_inc" != "$n_inc_ok" ]]; then
        fail "binary $target: $file has an include! that is not a string literal; its target cannot be checked"
      fi
      while IFS= read -r ref; do
        [[ -n "$ref" ]] || continue
        resolved="$dir/$ref"
        [[ -f "$resolved" ]] || fail "binary $target: $file refers to $ref, which does not exist"
        resolved=$(cd "${resolved%/*}" && pwd -P)/${resolved##*/}
        add "$resolved"
      done < <({
        { grep -oE '#\[path *= *"[^"]*" *\]' "$stripped" || true; } | sed -E 's/^#\[path *= *"([^"]*)" *\]$/\1/'
        { grep -oE '(^|[^A-Za-z0-9_])include! *\( *"[^"]*" *\)' "$stripped" || true; } | sed -E 's/^.*include! *\( *"([^"]*)" *\)$/\1/'
      })
    done
  done
  echo "main-only GUI sources: $(targets | wc -l | tr -d ' ') binaries, $files_total files read, no core_client reference"
}

check_manifest
case "$command_name" in
  filter)
    printf 'package(=gui) & kind(test) & (%s)\n' \
      "$(targets | sed 's/.*/binary(=&)/' | paste -sd'|' | sed 's/|/ | /g')"
    ;;
  binaries) targets | sed 's/^/gui::/' ;;
  validate)
    load_metadata
    n=0
    for t in $(targets); do source_of "$t"; n=$((n + 1)); done
    echo "main-only GUI registry ok: $n integration-test binaries of the gui package"
    ;;
  check-sources)
    load_metadata
    check_sources
    ;;
  *) echo "unknown command: $command_name" >&2; exit 2 ;;
esac
