#!/usr/bin/env bash
# Q2: the physical bootstrap pass is the ONLY retention entrance.
set -euo pipefail
root=${1:-$(cd "$(dirname "$0")/.." && pwd)}
cd "$root"
violations=$(
  grep -rn --include='*.rs' -E 'claim_physical_bootstrap_owners|retained_physical_owners|finish_recovery_graph_control|write_recovery_graph_control|write_recovery_graph_progress|activate_recovery_claim' crates 2>/dev/null | awk '
    {
      file=$0; sub(/:.*/,"",file); text=$0; sub(/^[^:]*:[0-9]+:/,"",text); sub(/^[[:space:]]+/,"",text)
      if (file == "crates/runtime/shared-test-support/physical_ownership.rs") next
      if (file ~ /(^|\/)tests\// || file ~ /(^|\/)tests\.rs$/ || file ~ /_tests\.rs$/ || text ~ /^\/\//) next
      if (index(text,"claim_physical_bootstrap_owners")) {
        if (file=="crates/runtime/src/runtime/physical_owners.rs" && text ~ /pub fn /) definitions++
        else if(file=="crates/workbench-host/src/bootstrap/planned.rs") calls++
        else print "VIOLATION: extra physical retention entrance: "file ": "text
      }
      if (text ~ /finish_recovery_graph_control|write_recovery_graph_control|write_recovery_graph_progress/) {
        if (file=="crates/storage/src/writer/recovery.rs" && text ~ /pub fn /) next
        if (file=="crates/runtime/src/runtime/disposal_support.rs" || file ~ /^crates\/runtime\/src\/runtime\/recovery_disposal(\.rs|\/)/ || file ~ /^crates\/storage\/src\/recovery\/owned_graphs(\.rs|\/)/ || file ~ /^crates\/evorch-core\/src\/recovery_admin(\.rs|\/)/ || file ~ /^crates\/workbench-host\/src\/bootstrap\/recovery_disposal(\.rs|\/)/) next
        print "VIOLATION: manual-only writer seam used outside leaf supervisor: "file ": "text
      }
      if (index(text,"activate_recovery_claim")) {
        if (file=="crates/storage/src/writer/recovery.rs" && text ~ /pub fn /) next
        if (file!="crates/runtime/src/runtime/recovery_activation/activate.rs") print "VIOLATION: activation writer bypasses reserved supervisor: "file
      }
      if (index(text,"retained_physical_owners") && file!="crates/runtime/src/runtime.rs" && file!="crates/runtime/src/runtime/physical_owners.rs") print "VIOLATION: external retained-set access: "file ": "text
    }
    END {
      if(definitions!=1) print "VIOLATION: expected one concrete retention definition, found "definitions+0
      if(calls!=1) print "VIOLATION: expected one planned bootstrap retention call, found "calls+0
    }' || true
)
# Visibility is a same-crate leaf boundary too, not just an external Rust API.
visibility=$(awk '
  /pub struct DisposalAuthority/ { inside=1; next }
  inside && /^}/ { inside=0 }
  inside && /pub[ (]/ { print "VIOLATION: DisposalAuthority field must stay private: " $0 }
' crates/runtime/src/runtime/physical_owners.rs)
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
# Only the collector can bypass the diagnostic observation hook for readback.
bypass=$(grep -rn --include='*.rs' 'owned_graph_ownership_readback' crates | awk '
  { file=$0; sub(/:.*/,"",file); text=$0; sub(/^[^:]*:[0-9]+:/,"",text); sub(/^[[:space:]]+/,"",text)
    if(file ~ /(^|\/)tests\// || file ~ /_tests.rs$/ || text ~ /^\/\//) next
    if(file != "crates/runtime/src/runtime/physical_owners.rs" && file != "crates/workbench-host/src/bootstrap/recovery_disposal/collector.rs") print "VIOLATION: collector readback bypass: "file
  }' || true)
if [[ -n $bypass ]]; then printf '%s\n' "$bypass"; exit 1; fi
echo 'Q2 retention boundaries passed'
