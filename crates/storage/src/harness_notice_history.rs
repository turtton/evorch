//! Read-only projection of durably adopted harness inputs for conversation displays.
use crate::{
    Database, HostInputProvenance, StorageError,
    pull_request_auto_fix::{PrNotice, PrNoticePhase},
    thread_index::{self, BlockOrigin, SnapshotApiError},
};
use std::collections::{BTreeMap, BTreeSet};

/// A native harness input at its saved non-system message position. Copies in forks
/// are inherited by the transcript's existing branch replay, not announced anew.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptedHarnessNotice {
    pub run_id: String,
    pub ordinal: usize,
    pub adopted_at_ns: i64,
    pub notice: PrNotice,
}

/// Adoption evidence and the event-log horizon observed in the same read transaction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HarnessNoticeSnapshot {
    pub through_event: i64,
    pub notices: Vec<AdoptedHarnessNotice>,
}

impl Database {
    /// Read adoption evidence with its log horizon so a bounded display replay can
    /// defer its tail until following has caught up, including skipped local rows.
    ///
    /// # Errors
    /// Returns storage read/decoding errors.
    pub fn adopted_harness_notice_snapshot(
        &self,
        run_ids: Option<&[String]>,
    ) -> Result<HarnessNoticeSnapshot, StorageError> {
        crate::repo::run_messages::read_snapshot(&self.conn, || {
            let through_event = self.last_event_id()?;
            let notices = self.adopted_harness_notices(run_ids)?;
            Ok(HarnessNoticeSnapshot {
                through_event,
                notices,
            })
        })
    }

    /// Committed harness inputs, ordered as host input replay is ordered. This reads
    /// snapshots only for runs with harness receipts; ordinary histories pay no
    /// snapshot/body reads. `None` selects all receipt runs; a supplied list restricts
    /// worker-side conversation rereads. Unknown/mismatched provenance is never inferred
    /// from text.
    ///
    /// # Errors
    /// Returns storage read/decoding errors. An unverifiable snapshot supplies no cards.
    pub fn adopted_harness_notices(
        &self,
        run_ids: Option<&[String]>,
    ) -> Result<Vec<AdoptedHarnessNotice>, StorageError> {
        crate::repo::run_messages::read_snapshot(&self.conn, || {
            let runs = run_ids
                .map(serde_json::to_string)
                .transpose()
                .map_err(|error| StorageError::Serialization(error.to_string()))?;
            let mut query = self.conn.prepare(
                "SELECT prompt_id FROM host_input_receipts
                 WHERE status='delivered'
                   AND json_extract(provenance_json,'$.kind')='harness_notice'
                   AND (?1 IS NULL OR run_id IN (SELECT value FROM json_each(?1)))
                 ORDER BY acceptance_seq",
            )?;
            let ids = query
                .query_map([runs], |row| row.get(0))?
                .collect::<Result<Vec<String>, _>>()?;
            let mut by_run = BTreeMap::<String, Vec<crate::HostInputReceipt>>::new();
            for receipt in self.host_input_receipts(&ids)? {
                by_run
                    .entry(receipt.run_id.clone())
                    .or_default()
                    .push(receipt);
            }
            let mut deliveries = Vec::new();
            for (run_id, receipts) in by_run {
                let bundle = match self.snapshot_bundle(&run_id) {
                    Ok(Some(bundle)) => bundle,
                    Ok(None) => continue,
                    Err(SnapshotApiError::Storage(error)) => return Err(error),
                    Err(_) => continue,
                };
                let Some(provenance) = bundle.provenance.as_ref() else {
                    continue;
                };
                if provenance.run_id != run_id
                    || provenance.snapshot_revision != bundle.snapshot_revision
                    || provenance.snapshot_sha256
                        != thread_index::snapshot_sha256(
                            &bundle.record.messages_json,
                            &bundle.record.checkpoints_json,
                        )
                {
                    continue;
                }
                let (Ok(messages), Ok(checkpoints)) = (
                    thread_index::parse_index_snapshot(&bundle.record.messages_json),
                    thread_index::parse_index_snapshot(&bundle.record.checkpoints_json),
                ) else {
                    continue;
                };
                let Some(checkpoints) = checkpoints.as_array() else {
                    continue;
                };
                if thread_index::validate_saved_provenance(provenance, &messages, checkpoints.len())
                    .is_err()
                {
                    continue;
                }
                let mut seen = BTreeSet::new();
                for message in &provenance.map.messages {
                    for block in &message.blocks {
                        let BlockOrigin::HarnessNotice { notification_id } = &block.origin else {
                            continue;
                        };
                        // Harness inputs are excluded from native/human search indexing,
                        // not from the transcript. Their typed origin is the display evidence.
                        if !seen.insert(notification_id) {
                            continue;
                        }
                        let Some(receipt) = receipts.iter().find(|receipt| matches!(
                            &receipt.provenance, HostInputProvenance::HarnessNotice { notification_id: id, .. } if id == notification_id
                        )) else { continue };
                        let Some(notice) = self.pr_notice(notification_id)? else {
                            continue;
                        };
                        let HostInputProvenance::HarnessNotice {
                            destination_thread_id,
                            ..
                        } = &receipt.provenance
                        else {
                            continue;
                        };
                        if notice.phase != PrNoticePhase::Adopted
                            || notice.payload.is_none()
                            || notice.destination_thread_id.as_ref() != Some(destination_thread_id)
                        {
                            continue;
                        }
                        deliveries.push(AdoptedHarnessNotice {
                            run_id: run_id.clone(),
                            ordinal: message.ordinal,
                            adopted_at_ns: receipt.accepted_at_ns,
                            notice,
                        });
                    }
                }
            }
            deliveries.sort_by(|a, b| {
                (a.adopted_at_ns, &a.run_id, a.ordinal).cmp(&(
                    b.adopted_at_ns,
                    &b.run_id,
                    b.ordinal,
                ))
            });
            Ok(deliveries)
        })
    }
}
