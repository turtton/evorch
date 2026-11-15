#!/usr/bin/env bash
# CI gate of the single-core migration: a feature PR changes only its own files,
# an integration PR is the only one that changes the central files.
#
# The scope comes from the PR's head branch: `<type>/<scope>` or
# `<type>/<scope>-<slug>`, for example `feat/pr10-settings`, `feat/pr17e-offline`,
# `feat/s0-integration`, `feat/integration-s1`. The scope is
#   pr<N>[a-z0-9]*   a feature PR: scripts/feature-scopes.txt (`pr10`, `pr17e`, ...)
#   s<N> | integration   an integration PR: it may change central files
# A branch with another name is not part of the migration and is not checked.
# The branch name is chosen when the PR is opened and does not change with its
# description, so editing the PR does not rerun CI.
#
# Usage: scripts/ci-scope-gate.sh --head-ref <branch> [--base <ref>]
#        scripts/ci-scope-gate.sh --scope-of <branch>     print the scope and exit
set -euo pipefail
cd "$(dirname "$0")/.."

# Prints the scope of a branch name, nothing for an unscoped branch.
scope_of() {
  local name=${1#refs/heads/} slug
  [[ "$name" == */* ]] || return 0
  slug=${name#*/}
  slug=${slug%%-*}
  case "$slug" in
    integration | s[0-9]*) echo integration ;;
    pr[0-9]*) echo "$slug" ;;
  esac
}

head_ref=""
base=origin/main
while [[ $# -gt 0 ]]; do
  case "$1" in
    --scope-of) scope_of "${2:?--scope-of needs a branch}"; exit 0 ;;
    --head-ref) head_ref="${2:?--head-ref needs a branch}"; shift 2 ;;
    --base) base="${2:?--base needs a ref}"; shift 2 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done
[[ -n "$head_ref" ]] || { echo "expected --head-ref" >&2; exit 2; }

scope=$(scope_of "$head_ref")
if [[ -z "$scope" ]]; then
  echo "branch $head_ref carries no migration scope (<type>/pr<N>-... or <type>/s<N>-...): not checked"
  exit 0
fi
echo "branch $head_ref: scope $scope, base $base"
exec scripts/check-central-files.sh --scope "$scope" --base "$base"
