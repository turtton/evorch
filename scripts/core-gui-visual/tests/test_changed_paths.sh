#!/usr/bin/env bash
# The paths filter of the capture job: what counts, what does not, and that a diff it
# cannot compute is an error.
set -euo pipefail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
script="$here/../changed-paths.sh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cd "$tmp"
git init -q .
git config user.email test@example.invalid
git config user.name test
git config commit.gpgsign false
mkdir -p crates/gui/src docs scripts/core-gui-visual .github/workflows
echo a > crates/gui/src/lib.rs
echo a > docs/readme.md
echo a > scripts/other.sh
git add -A && git commit -qm base
base=$(git rev-parse HEAD)

fail=0
expect() { # name expected path... (the paths are written, committed, diffed from base)
  local name=$1 expected=$2
  shift 2
  git checkout -q --detach "$base"
  for path in "$@"; do
    mkdir -p "$(dirname "$path")"
    echo "$name" >> "$path"
  done
  git add -A && git commit -qm "$name"
  local got
  got=$("$script" "$base" HEAD | head -1)
  if [[ "$got" != "relevant=$expected" ]]; then
    echo "FAIL $name: got $got, expected relevant=$expected" >&2
    fail=1
  fi
}

expect docs-only false docs/readme.md
expect unrelated-script false scripts/other.sh
expect crate-source true crates/gui/src/lib.rs
expect new-crate-file true crates/new/src/lib.rs
expect lockfile true Cargo.lock
expect workflow true .github/workflows/core-gui-visual.yml
expect other-workflow false .github/workflows/ci.yml
expect own-tooling true scripts/core-gui-visual/capture.sh
expect cef-setup true scripts/setup-cef.sh

# A rename out of a relevant path counts through its old name.
git checkout -q --detach "$base"
git mv crates/gui/src/lib.rs docs/moved.rs
git commit -qm rename
if [[ $("$script" "$base" HEAD | head -1) != "relevant=true" ]]; then
  echo "FAIL rename out of crates is not relevant" >&2
  fail=1
fi

# A diff that cannot be computed is an error and says nothing about relevance.
if out=$("$script" "$base" 0000000000000000000000000000000000000001 2>/dev/null); then
  echo "FAIL an unknown commit gave: $out" >&2
  fail=1
fi
exit "$fail"
