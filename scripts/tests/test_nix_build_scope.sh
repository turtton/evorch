#!/usr/bin/env bash
# Fixture tests for scripts/nix-build-scope.sh. Each case builds a throwaway git
# repository, so no workspace build is needed.
set -euo pipefail

scope=$(cd "$(dirname "$0")/.." && pwd)/nix-build-scope.sh
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
failures=0

workflow_v1='name: ci
on:
  pull_request:
concurrency:
  group: x
jobs:
  ci:
    runs-on: ubuntu-latest
    steps:
      - run: echo ci
  # nix comment
  nix-build:
    runs-on: ubuntu-latest
    steps:
      - run: nix build .#default
  offscreen-gate:
    runs-on: ubuntu-latest
    steps:
      - run: echo gate
'

new_repo() {
  repo="$work/$1"
  mkdir -p "$repo/.github/workflows"
  git -C "$repo" init -q -b main
  git -C "$repo" config user.email test@example.invalid
  git -C "$repo" config user.name test
  git -C "$repo" config commit.gpgsign false
  printf '%s' "$workflow_v1" > "$repo/.github/workflows/ci.yml"
  echo base > "$repo/README.md"
  git -C "$repo" add -A
  git -C "$repo" commit -q -m base
}

# commit_change <repo> <path> [content]: one commit on a topic branch.
commit_change() {
  git -C "$1" checkout -q -B topic main
  mkdir -p "$(dirname "$1/$2")"
  printf '%s\n' "${3:-changed}" > "$1/$2"
  git -C "$1" add -A
  git -C "$1" commit -q -m change
}

# expect <name> <full|false> <repo> [env...]: runs the script on the repo's
# topic branch against main.
expect() {
  local name=$1 want=$2 repo=$3 got
  shift 3
  got=$(cd "$repo" && env "$@" "$scope" main topic 2> /dev/null | sed -n 's/^full=//p')
  if [ "$got" = "$want" ]; then
    echo "ok   $name"
  else
    echo "FAIL $name: want full=$want, got '$got'"
    failures=$((failures + 1))
  fi
}

case_path() {
  local name=$1 want=$2 path=$3
  new_repo "$name"
  commit_change "$repo" "$path"
  expect "$name ($path)" "$want" "$repo" EVENT_NAME=pull_request
}

# Nix-affecting paths.
case_path flake true flake.nix
case_path lock true flake.lock
case_path nixfile true nix/overlay.nix
case_path toolchain true rust-toolchain.toml
case_path cargo-lock true Cargo.lock
case_path cargo-root true Cargo.toml
case_path crate-manifest true crates/gui/Cargo.toml
case_path build-script true crates/btls/build.rs
case_path cargo-config true .cargo/config.toml
case_path vendor true vendor/egui_commonmark/src/lib.rs
case_path cef true crates/browser-cef/src/lib.rs
case_path push-attic true scripts/push-attic.sh
case_path scope-script true scripts/nix-build-scope.sh

# Paths the Nix build does not read, or that cargo check and the ci job cover.
case_path rust-source false crates/gui/src/lib.rs
case_path docs false docs/readme.md
case_path other-workflow false .github/workflows/gui-pr-evidence.yml
case_path bench-lock false examples/benchmark/clamp/Cargo.lock
case_path other-script false scripts/check-harness.sh

# A mix: one Nix path decides.
new_repo mixed
commit_change "$repo" crates/gui/src/lib.rs
echo more > "$repo/flake.lock"
git -C "$repo" add -A
git -C "$repo" commit -q -m more
expect "mixed source and flake.lock" true "$repo" EVENT_NAME=pull_request

# A rename out of a listed path still counts.
new_repo rename
git -C "$repo" checkout -q main
echo a > "$repo/flake.lock"
git -C "$repo" add -A
git -C "$repo" commit -q -m lock
git -C "$repo" checkout -q -B topic
git -C "$repo" mv flake.lock lock.txt
git -C "$repo" commit -q -m rename
expect "rename out of flake.lock" true "$repo" EVENT_NAME=pull_request

# Workflow: only the header and the nix-build job count.
workflow_case() {
  local name=$1 want=$2 from=$3 to=$4
  new_repo "$name"
  git -C "$repo" checkout -q -B topic main
  sed "s|$from|$to|" "$repo/.github/workflows/ci.yml" > "$repo/ci.tmp"
  mv "$repo/ci.tmp" "$repo/.github/workflows/ci.yml"
  git -C "$repo" add -A
  git -C "$repo" commit -q -m wf
  expect "$name" "$want" "$repo" EVENT_NAME=pull_request
}
workflow_case "workflow ci job edit" false 'echo ci' 'echo ci2'
workflow_case "workflow other job edit" false 'echo gate' 'echo gate2'
workflow_case "workflow nix-build edit" true 'nix build .#default' 'nix build .#evorch'
workflow_case "workflow header edit" true 'group: x' 'group: y'
workflow_case "workflow comment edit" false '# nix comment' '# other comment'

# Fail-safe cases.
new_repo push
commit_change "$repo" docs/readme.md
expect "push event" true "$repo" EVENT_NAME=push
got=$(cd "$repo" && EVENT_NAME=pull_request "$scope" no-such-rev topic 2> /dev/null | sed -n 's/^full=//p')
if [ "$got" = true ]; then echo "ok   unresolvable base"; else echo "FAIL unresolvable base: '$got'"; failures=$((failures + 1)); fi

# Default revisions: HEAD must be a merge commit, and HEAD^1 is the base.
new_repo merge
commit_change "$repo" crates/gui/src/lib.rs
git -C "$repo" checkout -q main
echo side > "$repo/side.txt"
git -C "$repo" add -A
git -C "$repo" commit -q -m side
git -C "$repo" checkout -q -B test-merge main
git -C "$repo" merge -q --no-ff topic -m merge
got=$(cd "$repo" && EVENT_NAME=pull_request "$scope" 2> /dev/null | sed -n 's/^full=//p')
if [ "$got" = false ]; then echo "ok   merge commit default revisions"; else echo "FAIL merge commit: '$got'"; failures=$((failures + 1)); fi
# The base's own change (side.txt) is not the pull request's: a Nix file added
# only on the base side must not count.
git -C "$repo" checkout -q main
echo n > "$repo/flake.nix"
git -C "$repo" add -A
git -C "$repo" commit -q -m base-nix
git -C "$repo" checkout -q -B test-merge2 main
git -C "$repo" merge -q --no-ff topic -m merge2
got=$(cd "$repo" && EVENT_NAME=pull_request "$scope" 2> /dev/null | sed -n 's/^full=//p')
if [ "$got" = false ]; then echo "ok   base-side Nix change ignored"; else echo "FAIL base-side change: '$got'"; failures=$((failures + 1)); fi
git -C "$repo" checkout -q topic
got=$(cd "$repo" && EVENT_NAME=pull_request "$scope" 2> /dev/null | sed -n 's/^full=//p')
if [ "$got" = true ]; then echo "ok   non-merge HEAD"; else echo "FAIL non-merge HEAD: '$got'"; failures=$((failures + 1)); fi

if [ "$failures" -ne 0 ]; then
  echo "$failures failure(s)"
  exit 1
fi
echo "all nix-build-scope cases passed"
