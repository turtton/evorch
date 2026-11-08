//! Host sidebar projection fences. The file is saved before mirror/ACK; a failed
//! CAS leaves the durable management record pending for repair, never deletion.
use crate::{
    StorageError,
    lanes::LaneRecord,
    threads::{ThreadUpsert, ThreadWriter},
};
use rusqlite::{Transaction, TransactionBehavior};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SidebarMembershipFence {
    pub topology_revision: u64,
    pub management_revisions: Vec<(String, u64)>,
}

#[derive(Debug, Clone)]
pub struct SidebarProjectionSnapshot {
    pub fence: SidebarMembershipFence,
    pub lanes: Vec<LaneRecord>,
    pub projection_pending: bool,
    pub revision_floors: BTreeMap<String, (u64, u64)>,
}

impl crate::StorageHandle {
    /// Snapshot read under the host's shared-sidebar lock. No state is ACKed here.
    pub fn sidebar_projection_snapshot(
        &self,
        thread_ids: Vec<String>,
    ) -> Result<SidebarProjectionSnapshot, StorageError> {
        self.lane_job(false, move |conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Deferred)?;
            let mut lanes = Vec::new();
            let mut projection_pending = false;
            let mut revision_floors = BTreeMap::new();
            for id in thread_ids {
                if let Some(lane) = crate::repo::lanes::get_by_thread(&tx, &id)? {
                    projection_pending |= tx.query_row(
                        "SELECT management_projection_revision < management_revision FROM lanes WHERE lane_id=?1",
                        [&lane.lane_id], |row| row.get::<_, bool>(0),
                    )?;
                    lanes.push(lane);
                }
                if let Some(thread) = crate::repo::threads::get(&tx, &id)? {
                    revision_floors.insert(
                        id,
                        (
                            thread.archive_revision.unwrap_or(0),
                            thread.pin_revision.unwrap_or(0),
                        ),
                    );
                }
            }
            let fence = SidebarMembershipFence {
                topology_revision: crate::repo::lane_management::topology_revision(&tx)?,
                management_revisions: lanes
                    .iter()
                    .map(|lane| (lane.lane_id.clone(), lane.management_revision))
                    .collect(),
            };
            tx.commit()?;
            Ok(SidebarProjectionSnapshot {
                fence,
                lanes,
                projection_pending,
                revision_floors,
            })
        })
    }

    /// Mirror only a file checked against this exact membership, then ACK the
    /// revisions it durably contains. Checks, mirror and ACK share one transaction.
    pub fn project_sidebar_threads(
        &self,
        threads: Vec<ThreadUpsert>,
        fence: SidebarMembershipFence,
        acknowledge: bool,
    ) -> Result<usize, StorageError> {
        self.lane_job(true, move |conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            if crate::repo::lane_management::topology_revision(&tx)? != fence.topology_revision {
                return Err(StorageError::LaneManagement(
                    crate::lane_management::LaneManagementError::StaleManagementRevision,
                ));
            }
            for (id, revision) in &fence.management_revisions {
                let lane =
                    crate::repo::lanes::get(&tx, id)?.ok_or(StorageError::LaneManagement(
                        crate::lane_management::LaneManagementError::StaleManagementRevision,
                    ))?;
                if lane.management_revision != *revision {
                    return Err(StorageError::LaneManagement(
                        crate::lane_management::LaneManagementError::StaleManagementRevision,
                    ));
                }
            }
            let skipped = crate::repo::threads::upsert_in(&tx, ThreadWriter::Host, &threads)?;
            if acknowledge {
                for (id, revision) in &fence.management_revisions {
                    crate::repo::lane_management::ack_projection(&tx, id, *revision)?;
                }
            }
            tx.commit()?;
            Ok(skipped)
        })
    }
}
