#!/usr/bin/env bash
# Deterministic offline storage budgets; no timing thresholds or real user DB.
set -euo pipefail
cd "$(dirname "$0")/.."
exec scripts/run-contract-tests.sh io "$@"
