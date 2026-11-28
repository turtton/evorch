#!/usr/bin/env bash
# CI benchmark (DO NOT MERGE): start sccache for the remaining steps of the job.
# Runs after the shared rust-cache restore so that RUSTC_WRAPPER does not enter
# that action's cache key (it hashes the CARGO*/RUST* environment).
set -euo pipefail
export SCCACHE_DIR="$HOME/.cache/sccache"
mkdir -p "$SCCACHE_DIR"
{
  echo "RUSTC_WRAPPER=sccache"
  echo "SCCACHE_DIR=$SCCACHE_DIR"
  echo "SCCACHE_CACHE_SIZE=2G"
  echo "SCCACHE_IDLE_TIMEOUT=0"
} >> "$GITHUB_ENV"
export SCCACHE_CACHE_SIZE=2G SCCACHE_IDLE_TIMEOUT=0
sccache --version | tee -a "$M2_OUT/env.txt"
{
  echo "restored sccache dir: $(du -sb "$SCCACHE_DIR" | cut -f1) bytes, $(find "$SCCACHE_DIR" -type f | wc -l) files"
  echo "CARGO_INCREMENTAL=${CARGO_INCREMENTAL-<unset>} (after rust-cache)"
} | tee -a "$M2_OUT/env.txt"
sccache --start-server
sccache --zero-stats > /dev/null
