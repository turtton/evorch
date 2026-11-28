#!/usr/bin/env bash
# Q2/PR20: owner-free disposal still uses the fenced leaf supervisors.
set -euo pipefail
root=${1:-$(cd "$(dirname "$0")/.." && pwd)}
cd "$root"
# ADR 0024 process ownership must not re-enter production. Root maintenance and
# writer locks are independent of these removed Rust symbols.
if [[ -d crates/runtime/src/ownership ]] && [[ -n $(find crates/runtime/src/ownership -type f -print -quit) ]]; then
  echo 'VIOLATION: process ownership module reintroduced'; exit 1
fi
violations=$(
  grep -rn --include='*.rs' -E 'runtime::ownership|crate::ownership|OwnerHost|OwnerPermit|OwnershipConfig|OwnershipEvent|OwnerMutationGuard|PrepareExistingThread|with_legacy_import|claim_physical_bootstrap_owners|retained_physical_owners|RestartOwnershipEvidence|PhysicalClaimReport|finish_recovery_graph_control|write_recovery_graph_control|write_recovery_graph_progress|discard_legacy_recovery_graph|activate_recovery_claim' crates 2>/dev/null | awk '
    {
      file=$0; sub(/:.*/,"",file); text=$0; sub(/^[^:]*:[0-9]+:/,"",text); sub(/^[[:space:]]+/,"",text)
      if (file ~ /(^|\/)tests\// || file ~ /(^|\/)tests\.rs$/ || file ~ /_tests\.rs$/ || text ~ /^\/\//) next
      if (text ~ /runtime::ownership|crate::ownership|OwnerHost|OwnerPermit|OwnershipConfig|OwnershipEvent|OwnerMutationGuard|PrepareExistingThread|with_legacy_import|claim_physical_bootstrap_owners|retained_physical_owners|RestartOwnershipEvidence|PhysicalClaimReport/) print "VIOLATION: removed process ownership: "file ": "text
      if (text ~ /finish_recovery_graph_control|write_recovery_graph_control|write_recovery_graph_progress|discard_legacy_recovery_graph/) {
        if (file=="crates/storage/src/writer/recovery.rs" && text ~ /pub fn /) next
        if (file=="crates/runtime/src/runtime/disposal_support.rs" || file ~ /^crates\/runtime\/src\/runtime\/recovery_disposal(\.rs|\/)/ || file ~ /^crates\/storage\/src\/recovery\/owned_graphs(\.rs|\/)/ || file ~ /^crates\/evorch-core\/src\/recovery_admin(\.rs|\/)/ || file ~ /^crates\/workbench-host\/src\/bootstrap\/recovery_disposal(\.rs|\/)/) next
        print "VIOLATION: manual-only writer seam used outside leaf supervisor: "file ": "text
      }
      if (index(text,"activate_recovery_claim")) {
        if (file=="crates/storage/src/writer/recovery.rs" && text ~ /pub fn /) next
        if (file!="crates/runtime/src/runtime/recovery_activation/activate.rs") print "VIOLATION: activation writer bypasses reserved supervisor: "file
      }
    }' || true
)
# The sibling disposal supervisor may share its private retry cache, but callers
# outside that leaf cannot construct or mutate authority fields.
visibility=$(awk '
  /pub struct DisposalAuthority/ { inside=1; next }
  inside && /^}/ { inside=0 }
  inside && /pub[ (]/ && !/pub\(super\) resets:/ { print "VIOLATION: DisposalAuthority field must stay private: " $0 }
' crates/runtime/src/runtime/disposal_authority.rs)
if [[ -n $visibility ]]; then printf '%s\n' "$visibility"; exit 1; fi
if [[ -n $violations ]]; then printf '%s\n' "$violations"; exit 1; fi
# The live collector is part of the product path, not a UI-only wrapper.
# Presence checks intentionally fail when a new reservation entrance drops its fence.
control=crates/runtime/src/runtime/recovery_control.rs
if [[ -f $control ]]; then
  grep -q 'begin_graph_change(manifest, automatic)' "$control" || { echo 'VIOLATION: graph reservation bypasses live fence'; exit 1; }
  grep -q 'reserve_graph(manifest_id, true)' "$control" || { echo 'VIOLATION: automatic reservation bypasses live fence'; exit 1; }
  grep -q 'reserve_graph(manifest_id, false)' "$control" || { echo 'VIOLATION: manual reservation bypasses live fence'; exit 1; }
fi
public=crates/evorch-core/src/restart/public/implementation.rs
if [[ -f $public ]]; then
  closure=$(awk '/self.graphs.admit/ { inside=1 } inside { print } inside && /\}\)\?;/ { exit }' "$public")
  if ! grep -q validate_restart_preflight <<<"$closure" || grep -Eq 'preflight_restart_capture|prepare_restart_quiesce|with_database|Database::|std::fs|host.lock' <<<"$closure"; then
    echo 'VIOLATION: restart admission must consume only the memory preflight'; exit 1
  fi
fi
# The opaque proof validator itself must not reopen the request-time DB path.
proof=crates/runtime/src/runtime/restart_transaction.rs
if [[ -f $proof ]]; then
  memory=$(awk '/pub fn validate_restart_preflight/ { inside=1 } inside && /pub fn / && !/validate_restart_preflight/ { exit } inside { print }' "$proof")
  if ! grep -q restart_recipe_metadata <<<"$memory" || grep -Eq 'preflight_restart_capture|prepare_restart_quiesce|with_database|Database::|std::fs' <<<"$memory"; then
    echo 'VIOLATION: opaque restart proof validation must be memory-only'; exit 1
  fi
fi
echo 'Q2 owner-free disposal boundaries passed'
