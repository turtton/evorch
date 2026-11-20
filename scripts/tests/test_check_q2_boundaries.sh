#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
gate="$PWD/scripts/check-q2-boundaries.sh"
work=$(mktemp -d "${TMPDIR:-/tmp}/q2-boundary.XXXXXX")
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/crates/runtime/src/runtime" "$work/crates/workbench-host/src/bootstrap" "$work/crates/runtime/tests"
printf 'pub fn claim_physical_bootstrap_owners($evidence: RestartOwnershipEvidence) {}\n' > "$work/crates/runtime/src/runtime/physical_owners.rs"
printf 'runtime.claim_physical_bootstrap_owners(owner, evidence, callback);\n' > "$work/crates/workbench-host/src/bootstrap/planned.rs"
printf 'runtime.claim_physical_bootstrap_owners(owner, evidence, callback);\nretained_physical_owners;\n' > "$work/crates/runtime/tests/negative.rs"
bash "$gate" "$work" >/dev/null
printf 'runtime.claim_physical_bootstrap_owners(owner, evidence, callback);\n' > "$work/crates/runtime/src/extra.rs"
if bash "$gate" "$work" >/dev/null; then echo 'extra product call accepted'; exit 1; fi
printf 'retained_physical_owners.sealed = Some(fake);\n' > "$work/crates/runtime/src/extra.rs"
if bash "$gate" "$work" >/dev/null; then echo 'external retained-set write accepted'; exit 1; fi
printf 'handle.finish_recovery_graph_control(observation);\n' > "$work/crates/runtime/src/extra.rs"
if bash "$gate" "$work" >/dev/null; then echo 'automatic/query call to manual-only writer accepted'; exit 1; fi
rm "$work/crates/runtime/src/extra.rs"
printf 'pub struct DisposalAuthority {\n    expected: GraphControlCas,\n}\n' >> "$work/crates/runtime/src/runtime/physical_owners.rs"
bash "$gate" "$work" >/dev/null
printf 'pub struct DisposalAuthority {\n    pub(crate) expected: GraphControlCas,\n}\n' >> "$work/crates/runtime/src/runtime/physical_owners.rs"
if bash "$gate" "$work" >/dev/null; then echo 'crate-visible authority field accepted'; exit 1; fi
printf 'pub fn claim_physical_bootstrap_owners($evidence: RestartOwnershipEvidence) {}\n' > "$work/crates/runtime/src/runtime/physical_owners.rs"
: > "$work/crates/workbench-host/src/bootstrap/planned.rs"
if bash "$gate" "$work" >/dev/null; then echo 'missing bootstrap call accepted'; exit 1; fi
echo 'Q2 boundary fixtures passed'
