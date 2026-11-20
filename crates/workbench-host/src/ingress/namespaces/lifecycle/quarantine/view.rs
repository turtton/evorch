//! Storage rows to bounded wire items. Raw identities never leave the server:
//! the wire carries opaque references that are resolved again on each request.
use crate::admin::{AdminFailure, GraphReferences, conflict, failure};
use sha2::{Digest, Sha256};
use storage::recovery::quarantine as row;
use workbench_protocol::{
    errors::ErrorCode,
    namespaces::lifecycle::{AdminId, quarantine::*},
    wire::Revision,
};

pub(super) struct Rendered {
    pub raw: row::QuarantinedGraph,
    pub item: QuarantineItem,
}

fn alias(domain: &str, raw: &str) -> Result<AdminId, AdminFailure> {
    let mut hash = Sha256::new();
    for part in [domain, raw] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    AdminId::new(format!("{domain}.{:x}", hash.finalize())).map_err(|_| conflict())
}

/// One alias table per snapshot: a collision between two graphs is a conflict,
/// never a silent merge.
pub(super) fn render(graphs: Vec<row::QuarantinedGraph>) -> Result<Vec<Rendered>, AdminFailure> {
    let mut references = GraphReferences::default();
    graphs
        .into_iter()
        .map(|raw| {
            let item = QuarantineItem {
                graph_id: references.graph_reference(
                    &raw.restart_id,
                    &raw.capture_id,
                    &raw.manifest_id,
                )?,
                revision: Revision(raw.revision),
                cold: raw.cold,
                restart_tx_id: alias("tx", &raw.restart_id)?,
                capture_id: alias("capture", &raw.capture_id)?,
                reason: match raw.reason {
                    row::QuarantineReason::ColdBoot => QuarantineReason::ColdBoot,
                    row::QuarantineReason::ReleasedWithoutClaim => {
                        QuarantineReason::ReleasedWithoutClaim
                    }
                    row::QuarantineReason::Other => QuarantineReason::Other,
                },
                eligibility: if raw.revoked {
                    QuarantineEligibility::Revoked
                } else {
                    QuarantineEligibility::Open
                },
                claim: match raw.claim {
                    row::QuarantineClaim::Absent => QuarantineClaim::Absent,
                    row::QuarantineClaim::Prepared => QuarantineClaim::Prepared,
                    row::QuarantineClaim::Activated => QuarantineClaim::Activated,
                    row::QuarantineClaim::Released => QuarantineClaim::Released,
                },
                completion: match raw.completion {
                    row::QuarantineCompletion::NotStarted => QuarantineCompletion::NotStarted,
                    row::QuarantineCompletion::Complete => QuarantineCompletion::Complete,
                    row::QuarantineCompletion::Partial => QuarantineCompletion::Partial,
                    row::QuarantineCompletion::Unknown => QuarantineCompletion::Unknown,
                },
                members: raw.members,
            };
            Ok(Rendered { raw, item })
        })
        .collect()
}

/// A graph outside the inspected window is indistinguishable from an absent one.
pub(super) fn resolve(
    snapshot: row::QuarantineSnapshot,
    graph_id: &AdminId,
) -> Result<Rendered, AdminFailure> {
    render(snapshot.graphs)?
        .into_iter()
        .find(|r| &r.item.graph_id == graph_id)
        .ok_or_else(|| failure(ErrorCode::NotFound, "quarantined graph not found"))
}
