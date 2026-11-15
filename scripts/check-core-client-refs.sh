#!/usr/bin/env bash
# The core-client desktop (crates/gui/src/core_client) reaches the core only
# through the protocol (ADR 0045). This counts, per file, the references to an
# internal crate (storage, runtime, sandbox, ...) and fails when a file has more
# than scripts/core-client-refs-baseline.txt allows. The baseline only shrinks:
# lower a count in the PR that removes the references.
#
# The references are found by crates/core-client-refs, which reads each file with
# syn: `use` trees (groups, aliases, `::crate::`, raw identifiers), paths in
# expressions and types, and scopes. See the crate for what it counts.
#
# Usage: scripts/check-core-client-refs.sh [--root DIR] [--baseline FILE]
set -euo pipefail
cd "$(dirname "$0")/.."
exec cargo run --quiet --locked -p core-client-refs -- "$@"
