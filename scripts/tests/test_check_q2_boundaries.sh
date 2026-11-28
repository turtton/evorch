#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
gate="$PWD/scripts/check-q2-boundaries.sh"
work=$(mktemp -d "${TMPDIR:-/tmp}/q2-boundary.XXXXXX")
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/crates/runtime/src/runtime" "$work/crates/workbench-host/src/bootstrap" "$work/crates/runtime/tests"
printf 'pub struct DisposalAuthority {\n    expected: GraphControlCas,\n}\n' > "$work/crates/runtime/src/runtime/disposal_authority.rs"
bash "$gate" "$work" >/dev/null
printf 'let obsolete: runtime::ownership::OwnerPermit;\n' > "$work/crates/runtime/src/extra.rs"
if bash "$gate" "$work" >/dev/null; then echo 'removed owner type accepted'; exit 1; fi
printf 'runtime.claim_physical_bootstrap_owners();\n' > "$work/crates/runtime/src/extra.rs"
if bash "$gate" "$work" >/dev/null; then echo 'physical owner claim reintroduced'; exit 1; fi
printf 'handle.finish_recovery_graph_control(observation);\n' > "$work/crates/runtime/src/extra.rs"
if bash "$gate" "$work" >/dev/null; then echo 'automatic/query call to manual-only writer accepted'; exit 1; fi
printf 'handle.discard_legacy_recovery_graph(header);\n' > "$work/crates/runtime/src/extra.rs"
if bash "$gate" "$work" >/dev/null; then echo 'legacy discard outside management supervisor accepted'; exit 1; fi
rm "$work/crates/runtime/src/extra.rs"
printf 'pub struct DisposalAuthority {\n    pub(crate) expected: GraphControlCas,\n}\n' > "$work/crates/runtime/src/runtime/disposal_authority.rs"
if bash "$gate" "$work" >/dev/null; then echo 'crate-visible authority field accepted'; exit 1; fi
printf 'pub struct DisposalAuthority {\n    expected: GraphControlCas,\n}\n' > "$work/crates/runtime/src/runtime/disposal_authority.rs"
bash "$gate" "$work" >/dev/null
echo 'Q2 owner-free authority fixtures passed'

# Real reservation/public bodies are the positive fixture for the live hooks.
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
echo 'Q2 live admission boundary fixtures passed'
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
