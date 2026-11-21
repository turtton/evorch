#!/usr/bin/env bash
# Truth table of scripts/nix-build-verdict.sh, the verdict of the `nix-build`
# aggregate job, and the wiring of that job in the workflow. No network.
# shellcheck disable=SC2016 # the workflow expressions are matched literally
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
verdict=$root/scripts/nix-build-verdict.sh
workflow=$root/.github/workflows/ci.yml
failures=0

# expect <name> <pass|fail> <scope result> <full> <default result> <core result>
expect() {
  local name=$1 want=$2 status=0
  SCOPE_RESULT=$3 SCOPE_FULL=$4 DEFAULT_RESULT=$5 CORE_RESULT=$6 "$verdict" > /dev/null 2>&1 || status=$?
  local got=pass
  [ "$status" -eq 0 ] || got=fail
  if [ "$got" = "$want" ]; then
    echo "ok   $name"
  else
    echo "FAIL $name: want $want, got $got (exit $status)"
    failures=$((failures + 1))
  fi
}

# The two proven cases.
expect "no Nix change: both skipped" pass success false skipped skipped
expect "full: both success" pass success true success success

# The scope job itself is not trusted unless it succeeded.
for r in failure cancelled skipped ''; do
  expect "scope ${r:-unset} with full=false" fail "$r" false skipped skipped
  expect "scope ${r:-unset} with full=true" fail "$r" true success success
done

# A `full` output that is missing or corrupt is never the cargo-check-only case.
for f in '' True FALSE 1 yes 'true '; do
  expect "full='$f'" fail success "$f" skipped skipped
  expect "full='$f' with both success" fail success "$f" success success
done

# Once the full build is required, every non-success of either side fails.
for r in failure cancelled skipped ''; do
  expect "full: default $r" fail success true "$r" success
  expect "full: core $r" fail success true success "$r"
  expect "full: both $r" fail success true "$r" "$r"
done

# A full job that ran although the scope says full=false is inconsistent.
for r in success failure cancelled ''; do
  expect "no Nix change: default $r" fail success false "$r" skipped
  expect "no Nix change: core $r" fail success false skipped "$r"
done

# Unset variables fail closed.
status=0
env -u SCOPE_RESULT -u SCOPE_FULL -u DEFAULT_RESULT -u CORE_RESULT "$verdict" > /dev/null 2>&1 || status=$?
if [ "$status" -ne 0 ]; then echo "ok   nothing set"; else echo "FAIL nothing set passed"; failures=$((failures + 1)); fi

# The workflow: the aggregate keeps the check name and runs the script on the
# needs of the three jobs, and the full jobs start on the scope job's output alone.
job_block() { # <job>
  awk -v job="  $1:" '
    /^[^[:space:]]/ { in_jobs = ($0 == "jobs:"); next }
    !in_jobs { next }
    /^  [A-Za-z0-9_-]+:[[:space:]]*$/ { current = ($0 == job); next }
    current && $0 !~ /^[[:space:]]*#/ { print }
  ' "$workflow"
}
require() { # <name> <job> <fixed string>
  if job_block "$2" | grep -qF -- "$3"; then
    echo "ok   $1"
  else
    echo "FAIL $1: '$3' not found in job $2"
    failures=$((failures + 1))
  fi
}
forbid() { # <name> <job> <fixed string>
  if job_block "$2" | grep -qF -- "$3"; then
    echo "FAIL $1: '$3' found in job $2"
    failures=$((failures + 1))
  else
    echo "ok   $1"
  fi
}
require "aggregate always runs" nix-build "if: always()"
require "aggregate needs the three jobs" nix-build "needs: [nix-scope-check, nix-full-default, nix-full-core]"
require "aggregate runs the verdict script" nix-build "run: scripts/nix-build-verdict.sh"
for env_line in 'SCOPE_RESULT: ${{ needs.nix-scope-check.result }}' \
  'SCOPE_FULL: ${{ needs.nix-scope-check.outputs.full }}' \
  'DEFAULT_RESULT: ${{ needs.nix-full-default.result }}' \
  'CORE_RESULT: ${{ needs.nix-full-core.result }}'; do
  require "aggregate passes ${env_line%%:*}" nix-build "$env_line"
done
for job in nix-full-default nix-full-core; do
  require "$job needs only the scope job" "$job" "needs: nix-scope-check"
  require "$job starts on the scope output and survives a failed need" "$job" "if: \${{ !cancelled() && needs.nix-scope-check.result == 'success' && needs.nix-scope-check.outputs.full == 'true' }}"
  forbid "$job is not tied to quick-checks" "$job" quick-checks
  forbid "$job is not tied to otel" "$job" otel
done

if [ "$failures" -ne 0 ]; then
  echo "$failures failure(s)"
  exit 1
fi
echo "all nix-build-verdict cases passed"
