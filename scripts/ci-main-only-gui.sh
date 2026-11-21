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
#   check-depinfo BINARIES_METADATA
#                  require that no source file the compiler read for a registered
#                  binary names `core_client` (see below)
# --metadata FILE is a `cargo metadata --no-deps` document (default: run cargo).
# --root DIR is the directory the paths in a dependency-info file are relative to
# (default: the repository root, where cargo runs rustc).
#
# What a registration is. Only a binary: never a test name, a module selector or a
# filter expression, so a test added to, renamed in or removed from a registered
# binary needs no change here, and a binary that is not registered stays in the
# pull request run. The binary's kind, source path and nextest id are derived from
# Cargo, not repeated in the registry. Registering anything but an integration test
# of the gui package is rejected.
#
# What check-depinfo reads. The compiler's own list of the files it read for each
# registered binary: the `<binary>.d` dependency-info file that rustc writes next
# to the test binary (cargo nextest's binaries metadata, the one the inventory
# builds from, gives the binary's path). Module resolution is therefore the
# compiler's, whatever the nesting, the module names or the `#[path]` and
# `include!` in use, and no part of it is re-implemented here. Every file in the
# list is read, whatever its extension (`include!("body.inc")` takes any file as
# Rust source), and searched as it is for the plain text `core_client`, binaries
# included. Nothing is parsed: not comments (a raw string such as r#"/*"# makes a
# comment stripper discard real code), not word boundaries. One file that contains
# the text fails, so a comment, a string or a longer identifier such as
# core_client_e2e is rejected too; the check errs on the side of rejecting and has
# no exception list. A dependency-info file that is missing, a binary the metadata
# does not know, a listed file that does not exist, or a list without any file is a
# failure, never a pass. The list also holds the files read with `include_str!` and
# `include_bytes!`, so those are checked too. This is a guard against
# the new core-client desktop (ADR 0045) entering a binary that a pull request no
# longer runs; it does not prove that a registered test is not a core-client test,
# so a test of the new system goes into a binary that is not registered.
# scripts/run-test-group.sh runs it in the gui group's inventory, right after the
# build that produced the dependency-info.
set -euo pipefail
cd "$(dirname "$0")/.."

manifest=scripts/ci-main-only-gui-tests.json
metadata=""
root=$PWD
while [[ $# -gt 0 ]]; do
  case "$1" in
    --manifest) manifest="${2:?--manifest needs a file}"; shift 2 ;;
    --metadata) metadata="${2:?--metadata needs a file}"; shift 2 ;;
    --root) root="${2:?--root needs a directory}"; shift 2 ;;
    -*) echo "unknown option: $1" >&2; exit 2 ;;
    *) break ;;
  esac
done
command_name="${1:?expected filter, binaries, validate or check-depinfo}"
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

# The files listed in a dependency-info file (Makefile syntax: `target: dep...`,
# a space in a name written `\ `, then one `dep:` rule per file). Only the first
# line carries the list.
depinfo_sources() { # <file.d>
  local line
  IFS= read -r line < "$1" || true
  [[ "$line" == *": "* ]] || { echo "FAIL: $1 is not a dependency-info file" >&2; return 1; }
  line=${line#*: }
  line=${line//\\ /$'\001'}
  printf '%s\n' "$line" | tr ' ' '\n' | tr '\001' ' '
}

check_depinfo() {
  local binaries_metadata=$1 target id bin dep src hits files_total=0 n=0
  [[ -f "$binaries_metadata" ]] || fail "$binaries_metadata does not exist"
  for target in $(targets); do
    id="gui::$target"
    bin=$(jq -er --arg id "$id" '.["rust-binaries"][$id]["binary-path"]' "$binaries_metadata") \
      || fail "$binaries_metadata has no binary $id: cannot find its dependency-info"
    if [[ "$bin" != /* ]]; then
      bin="$(jq -er '.["rust-build-meta"]["target-directory"]' "$binaries_metadata")/$bin"
    fi
    dep="$bin.d"
    [[ -f "$dep" ]] || fail "binary $target: no dependency-info file $dep"
    n=$((n + 1))
    local listed=0
    while IFS= read -r src; do
      [[ -n "$src" ]] || continue
      [[ "$src" == /* ]] || src="$root/$src"
      [[ -f "$src" ]] || fail "binary $target: $dep lists $src, which does not exist"
      listed=$((listed + 1))
      files_total=$((files_total + 1))
      hits=$(grep -anF 'core_client' "$src" | cut -c1-200 || true)
      if [[ -n "$hits" ]]; then
        echo "FAIL: binary $target: $src names core_client:" >&2
        sed 's/^/  /' <<< "$hits" >&2
        exit 1
      fi
    done < <(depinfo_sources "$dep")
    ((listed > 0)) || fail "binary $target: $dep lists no file"
  done
  echo "main-only GUI sources (dependency-info): $n binaries, $files_total files read, no core_client reference"
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
  check-depinfo)
    check_depinfo "${2:?check-depinfo needs the nextest binaries metadata file}"
    ;;
  *) echo "unknown command: $command_name" >&2; exit 2 ;;
esac
