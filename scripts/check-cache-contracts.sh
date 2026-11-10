#!/usr/bin/env bash
# Offline cache correctness gate. No credentials, external API, or paid model calls.
# nextest gives process-global cache tests independent processes and a hang watchdog.
#
# Usage: scripts/check-cache-contracts.sh [--group NAME] [--shard K/N] [--list] [nextest args...]
# Without options it runs the whole workspace. CI runs one crate group (and
# shard) at a time through scripts/run-test-group.sh, which calls this script
# with --group/--shard; the options are those of scripts/run-contract-tests.sh.
set -euo pipefail
cd "$(dirname "$0")/.."
opts=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --group | --shard) opts+=("$1" "${2:?$1 needs a value}"); shift 2 ;;
    --list) opts+=("$1"); shift ;;
    *) break ;;
  esac
done
exec scripts/run-contract-tests.sh "${opts[@]}" cache "$@"
