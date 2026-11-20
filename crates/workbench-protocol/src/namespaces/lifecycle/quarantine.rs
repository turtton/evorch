//! Quarantine leaf contract. Display never implies resume.
//!
//! The eligibility of a quarantined graph and the cleanup that follows its
//! revocation are separate facts: a discard that revoked the eligibility is not
//! complete until `completion` says so.
use super::contracts::*;
use crate::{
    validation::ValidationError,
    wire::{InstanceEpoch, Revision},
};
use serde::{Deserialize, Serialize};

pub const MAX_QUARANTINE_ITEMS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum QuarantineCommand {
    Discard(ConfirmedGraph),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum QuarantineQuery {
    List,
    Detail { graph_id: AdminId },
}
/// Why the evidence was set aside. Never free text from storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuarantineReason {
    ColdBoot,
    ReleasedWithoutClaim,
    Other,
}
/// Whether the graph can ever be resumed. `Revoked` is permanent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuarantineEligibility {
    Open,
    Revoked,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuarantineClaim {
    Absent,
    Prepared,
    Activated,
    Released,
}
/// What is left after the revocation. Only `Complete` means nothing is left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuarantineCompletion {
    NotStarted,
    Complete,
    Partial,
    Unknown,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuarantineItem {
    pub graph_id: AdminId,
    pub revision: Revision,
    /// No live successor owns the graph. Cold items are never counted as
    /// undisposed owned graphs.
    pub cold: bool,
    pub restart_tx_id: AdminId,
    pub capture_id: AdminId,
    pub reason: QuarantineReason,
    pub eligibility: QuarantineEligibility,
    pub claim: QuarantineClaim,
    pub completion: QuarantineCompletion,
    pub members: u32,
}
impl QuarantineItem {
    /// True only when the eligibility is revoked and nothing is left to clean up.
    pub fn discard_complete(&self) -> bool {
        self.eligibility == QuarantineEligibility::Revoked
            && self.completion == QuarantineCompletion::Complete
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuarantineDetail {
    pub item: QuarantineItem,
    /// The epoch a discard binds to. A cold graph has no owner to bind.
    pub core_epoch: InstanceEpoch,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum QuarantineReply {
    List {
        items: Vec<QuarantineItem>,
        revision: Revision,
        truncated: bool,
    },
    Detail(QuarantineDetail),
}
impl QuarantineReply {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Self::List { items: rows, .. } = self {
            items(rows)?;
            if rows.len() > MAX_QUARANTINE_ITEMS {
                return Err(ValidationError::TooLong {
                    field: "quarantine.items",
                    max: MAX_QUARANTINE_ITEMS,
                    actual: rows.len(),
                });
            }
        }
        bounded(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(revision: u64) -> QuarantineItem {
        QuarantineItem {
            graph_id: AdminId::new("graph.a").unwrap(),
            revision: Revision(revision),
            cold: true,
            restart_tx_id: AdminId::new("tx.a").unwrap(),
            capture_id: AdminId::new("capture.a").unwrap(),
            reason: QuarantineReason::ColdBoot,
            eligibility: QuarantineEligibility::Revoked,
            claim: QuarantineClaim::Released,
            completion: QuarantineCompletion::Complete,
            members: 2,
        }
    }

    #[test]
    fn a_revocation_is_not_a_complete_discard_until_nothing_is_left() {
        for (completion, complete) in [
            (QuarantineCompletion::Complete, true),
            (QuarantineCompletion::Partial, false),
            (QuarantineCompletion::Unknown, false),
            (QuarantineCompletion::NotStarted, false),
        ] {
            let revoked = QuarantineItem {
                completion,
                ..item(1)
            };
            assert_eq!(revoked.discard_complete(), complete, "{completion:?}");
        }
        let open = QuarantineItem {
            eligibility: QuarantineEligibility::Open,
            ..item(1)
        };
        assert!(!open.discard_complete());
    }

    #[test]
    fn the_list_is_bounded_in_count() {
        let list = |n: usize| QuarantineReply::List {
            items: (0..n as u64).map(item).collect(),
            revision: Revision(1),
            truncated: false,
        };
        assert!(list(MAX_QUARANTINE_ITEMS).validate().is_ok());
        assert!(list(MAX_QUARANTINE_ITEMS + 1).validate().is_err());
    }

    #[test]
    fn the_wire_form_is_closed_and_carries_no_free_text() {
        let value = serde_json::to_value(item(3)).unwrap();
        assert_eq!(
            value,
            json!({
                "graph_id":"graph.a","revision":"3","cold":true,"restart_tx_id":"tx.a",
                "capture_id":"capture.a","reason":"cold_boot","eligibility":"revoked",
                "claim":"released","completion":"complete","members":2
            })
        );
        let mut extra = value;
        extra["path"] = json!("/etc");
        assert!(serde_json::from_value::<QuarantineItem>(extra).is_err());
    }
}
