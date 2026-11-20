#!/usr/bin/env bash
# Q2: the physical bootstrap pass is the ONLY retention entrance.
set -euo pipefail
root=${1:-$(cd "$(dirname "$0")/.." && pwd)}
cd "$root"
violations=$(
  grep -rn --include='*.rs' -E 'claim_physical_bootstrap_owners|retained_physical_owners|finish_recovery_graph_control|write_recovery_graph_progress' crates 2>/dev/null | awk '
    {
      file=$0; sub(/:.*/,"",file); text=$0; sub(/^[^:]*:[0-9]+:/,"",text); sub(/^[[:space:]]+/,"",text)
      if (file == "crates/runtime/shared-test-support/physical_ownership.rs") next
      if (file ~ /(^|\/)tests\// || file ~ /(^|\/)tests\.rs$/ || file ~ /_tests\.rs$/ || text ~ /^\/\//) next
      if (index(text,"claim_physical_bootstrap_owners")) {
        if (file=="crates/runtime/src/runtime/physical_owners.rs" && text ~ /pub fn /) definitions++
        else if(file=="crates/workbench-host/src/bootstrap/planned.rs") calls++
        else print "VIOLATION: extra physical retention entrance: "file ": "text
      }
      if (text ~ /finish_recovery_graph_control|write_recovery_graph_progress/) {
        if (file=="crates/storage/src/writer/recovery.rs" && text ~ /pub fn /) next
        if (file ~ /^crates\/runtime\/src\/runtime\/recovery_disposal(\.rs|\/)/ || file ~ /^crates\/storage\/src\/recovery\/owned_graphs(\.rs|\/)/ || file ~ /^crates\/evorch-core\/src\/recovery_admin(\.rs|\/)/ || file ~ /^crates\/workbench-host\/src\/bootstrap\/recovery_disposal(\.rs|\/)/) next
        print "VIOLATION: manual-only writer seam used outside leaf supervisor: "file ": "text
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
echo 'Q2 retention boundaries passed'
