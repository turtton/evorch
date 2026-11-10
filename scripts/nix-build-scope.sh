#!/usr/bin/env bash
# Decide whether the CI nix-build job must run the full Nix package build.
#
# Usage: scripts/nix-build-scope.sh [BASE_REV [HEAD_REV]]
#
# Prints `full=true|false` and `reason=...` to stdout (GITHUB_OUTPUT format);
# the changed paths that decided it go to stderr. Anything this script cannot
# decide (not a pull request, no merge commit, an unresolvable revision, a failed
# diff) answers `full=true`: skipping the Nix build is only ever the proven case.
#
# Only pull requests are narrowed. A push to main always builds in full, and so
# does a pull request that changes anything Nix reads other than Rust sources:
#   flake.nix, flake.lock, *.nix, nix/   the packaging itself
#   rust-toolchain.toml                  fenix pins the toolchain from it
#   Cargo.toml, Cargo.lock,              the dependency graph and its features: a
#   crates/*/Cargo.toml                  new native library, a feature that
#                                        unifies differently under -p, a cold
#                                        dependency build (cached in attic)
#   crates/*/build.rs, .cargo/, vendor/  build scripts and sources that build.rs
#                                        or [patch.crates-io] compile
#   crates/browser-cef/                  the CEF helper and libcef linkage that
#                                        the CEF install check inspects
#   scripts/push-attic.sh                what the PR and main steps upload with
#   this script and its test             the decision itself
#   .github/workflows/ci.yml             only the top-level keys other than
#                                        `jobs` (`on`, `concurrency`, `env`,
#                                        wherever placed) and the nix-build
#                                        job; the other jobs do not reach Nix
# Rust sources of the crates are deliberately not listed: the ci job tests them
# and the non-test cargo check of the same -p/--bin selection (the nix-build
# job's substitute) compiles them as the Nix build does.
set -euo pipefail

workflow=.github/workflows/ci.yml

emit() {
  echo "full=$1"
  echo "reason=$2"
}

full() {
  echo "nix-build-scope: $1 -> full Nix build" >&2
  emit true "$1"
  exit 0
}

if [ "${EVENT_NAME:-pull_request}" != pull_request ]; then
  full "event ${EVENT_NAME} always builds in full"
fi

head=${2:-HEAD}
if [ $# -ge 1 ]; then
  base=$1
else
  # actions/checkout checks out the pull request's test merge commit; its first
  # parent is the base the pull request would merge into. A checkout that is not
  # a merge commit cannot say what the pull request changed.
  git rev-parse --verify -q "$head^2" > /dev/null 2>&1 || full "HEAD is not a merge commit"
  base=$(git rev-parse --verify -q "$head^1") || full "cannot resolve the merge base"
fi
git rev-parse --verify -q "$base^{commit}" > /dev/null 2>&1 || full "cannot resolve base $base"
git rev-parse --verify -q "$head^{commit}" > /dev/null 2>&1 || full "cannot resolve head $head"

# --no-renames lists both sides of a rename, so a file moved out of a listed
# path still matches.
changed=$(git diff --name-only --no-renames "$base" "$head") || full "git diff failed"

# The workflow compared without comments and blank lines: every top-level key
# other than `jobs` (`on`, `concurrency`, and an `env` or `defaults` placed
# before or after the jobs) and the nix-build job's block.
nix_workflow_scope() {
  awk '
    /^[[:space:]]*(#|$)/ { next }
    /^[^[:space:]]/ { in_jobs = ($1 == "jobs:"); current = 0; print; next }
    !in_jobs { print; next }
    /^  [A-Za-z0-9_-]+:/ { current = ($1 == "nix-build:") }
    current { print }
  '
}

nix_workflow_changed() {
  local old new
  old=$(git show "$base:$workflow" 2> /dev/null | nix_workflow_scope) || return 0
  new=$(git show "$head:$workflow" 2> /dev/null | nix_workflow_scope) || return 0
  # An empty scope means the structure was not recognized.
  [ -n "$old" ] && [ -n "$new" ] || return 0
  [ "$old" != "$new" ]
}

matched=()
while IFS= read -r path; do
  [ -n "$path" ] || continue
  case $path in
    flake.nix | flake.lock | *.nix | nix/* | rust-toolchain.toml | \
      Cargo.toml | Cargo.lock | crates/*/Cargo.toml | \
      crates/*/build.rs | .cargo/* | vendor/* | crates/browser-cef/* | \
      scripts/push-attic.sh | scripts/nix-build-scope.sh | scripts/tests/test_nix_build_scope.sh)
      matched+=("$path")
      ;;
    "$workflow")
      if nix_workflow_changed; then
        matched+=("$path (header or nix-build job)")
      fi
      ;;
  esac
done <<< "$changed"

if [ "${#matched[@]}" -gt 0 ]; then
  printf 'nix-build-scope: Nix-affecting: %s\n' "${matched[@]}" >&2
  emit true "Nix-affecting change: ${matched[0]}$([ "${#matched[@]}" -gt 1 ] && echo " (+$((${#matched[@]} - 1)) more)")"
else
  echo "nix-build-scope: no Nix-affecting change among $(printf '%s\n' "$changed" | grep -c . || true) changed paths -> cargo check only" >&2
  emit false "no Nix-affecting change"
fi
