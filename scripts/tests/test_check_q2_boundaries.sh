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
# Only this real sealed-bootstrap fixture is exempt; its directory is not.
mkdir -p "$work/crates/runtime/shared-test-support"
printf 'runtime.claim_physical_bootstrap_owners(owner, evidence, callback);\n' > "$work/crates/runtime/shared-test-support/physical_ownership.rs"
bash "$gate" "$work" >/dev/null
cp "$work/crates/runtime/shared-test-support/physical_ownership.rs" "$work/crates/runtime/shared-test-support/product.rs"
if bash "$gate" "$work" >/dev/null; then echo 'non-allowlisted shared support call accepted'; exit 1; fi
rm "$work/crates/runtime/shared-test-support/product.rs"
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

# Real reservation/public bodies are the positive fixture for the live hooks.
printf 'runtime.claim_physical_bootstrap_owners(owner, evidence, callback);\n' > "$work/crates/workbench-host/src/bootstrap/planned.rs"
cp crates/runtime/src/runtime/recovery_control.rs "$work/crates/runtime/src/runtime/recovery_control.rs"
mkdir -p "$work/crates/evorch-core/src/restart/public"
cp crates/evorch-core/src/restart/public/implementation.rs "$work/crates/evorch-core/src/restart/public/implementation.rs"
bash "$gate" "$work" >/dev/null
printf 'no fence before the reservation\n' > "$work/crates/runtime/src/runtime/recovery_control.rs"
if bash "$gate" "$work" >/dev/null; then echo 'missing live reservation hook accepted'; exit 1; fi
cp crates/runtime/src/runtime/recovery_control.rs "$work/crates/runtime/src/runtime/recovery_control.rs"
printf 'self.graphs.admit(revision, &mut || {\n runtime.preflight_restart_capture();\n})?;\n' > "$work/crates/evorch-core/src/restart/public/implementation.rs"
if bash "$gate" "$work" >/dev/null; then echo 'I/O preflight in commit accepted'; exit 1; fi
cp crates/evorch-core/src/restart/public/implementation.rs "$work/crates/evorch-core/src/restart/public/implementation.rs"
printf 'runtime.owned_graph_ownership_readback(cas);\n' > "$work/crates/runtime/src/extra.rs"
if bash "$gate" "$work" >/dev/null; then echo 'unobserved owner read outside collector accepted'; exit 1; fi
echo 'Q2 live admission boundary fixtures passed'
rm "$work/crates/runtime/src/extra.rs"
printf 'writer.activate_recovery_claim(request);\n' > "$work/crates/runtime/src/extra.rs"
if bash "$gate" "$work" >/dev/null; then echo 'unfenced activation writer accepted'; exit 1; fi
printf 'writer.write_recovery_graph_control(request);\n' > "$work/crates/runtime/src/extra.rs"
if bash "$gate" "$work" >/dev/null; then echo 'unfenced graph-control writer accepted'; exit 1; fi
echo 'Q2 durable writer bypass fixtures passed'

rm "$work/crates/runtime/src/extra.rs"
cp crates/runtime/src/runtime/restart_transaction.rs "$work/crates/runtime/src/runtime/restart_transaction.rs"
bash "$gate" "$work" >/dev/null
printf 'pub fn validate_restart_preflight() {
 self.preflight_restart_capture(recipes);
 restart_recipe_metadata();
}
' > "$work/crates/runtime/src/runtime/restart_transaction.rs"
if bash "$gate" "$work" >/dev/null; then echo 'DB preflight revived behind opaque validator accepted'; exit 1; fi
echo 'Q2 memory-proof boundary fixtures passed'
