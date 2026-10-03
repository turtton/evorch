#!/usr/bin/env bash
# Offline cache correctness gate. No credentials, external API, or paid model calls.
# nextest gives process-global cache tests independent processes and a hang watchdog.
set -euo pipefail
cd "$(dirname "$0")/.."
exec scripts/run-contract-tests.sh cache "$@"
