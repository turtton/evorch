#!/usr/bin/env bash
# The verdict of the CI `nix-build` aggregate job over its three needs.
#
# Reads the results of the needs from the environment (the `needs` context of the
# job): SCOPE_RESULT and SCOPE_FULL (the `full` output) of nix-scope-check, and
# DEFAULT_RESULT / CORE_RESULT of nix-full-default / nix-full-core. Exits 0 only
# for the two proven cases:
#
#   scope success, full=false  both full jobs skipped
#   scope success, full=true   both full jobs success
#
# Everything else fails: a scope job that failed or was cancelled, a `full` output
# that is neither `true` nor `false` (missing or corrupt), and, once the full
# build is required, a full job that failed, was cancelled or was skipped. A full
# job that ran although the scope says full=false is inconsistent and fails too.
set -euo pipefail

scope_result=${SCOPE_RESULT-}
scope_full=${SCOPE_FULL-}
default_result=${DEFAULT_RESULT-}
core_result=${CORE_RESULT-}

echo "nix-scope-check: result=${scope_result:-<unset>} full=${scope_full:-<unset>}"
echo "nix-full-default: ${default_result:-<unset>}"
echo "nix-full-core: ${core_result:-<unset>}"

fail() {
  echo "::error::nix-build: $1" >&2
  exit 1
}

[ "$scope_result" = success ] || fail "nix-scope-check did not succeed ($scope_result)"

case $scope_full in
  false)
    [ "$default_result" = skipped ] && [ "$core_result" = skipped ] ||
      fail "full=false, but a full Nix job was not skipped (default=$default_result, core=$core_result)"
    echo "nix-build: no Nix-affecting change; the release cargo check of nix-scope-check stands in"
    ;;
  true)
    [ "$default_result" = success ] || fail "nix-full-default did not succeed ($default_result)"
    [ "$core_result" = success ] || fail "nix-full-core did not succeed ($core_result)"
    echo "nix-build: both full Nix builds succeeded"
    ;;
  *)
    fail "the scope decision is neither true nor false ('$scope_full')"
    ;;
esac
