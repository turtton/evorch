#!/usr/bin/env bash
# Deterministic offline storage budgets; no timing thresholds or real user DB.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo test -p storage --test io_contracts -- --nocapture
cargo test -p runtime --lib ownership::tests
cargo test -p gui --lib storage_bridge
cargo test -p gui --bin evorch-gui
cargo test -p gui --test storage_bridge --test diagnostic_ledger
