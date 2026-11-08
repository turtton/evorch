//! Durable management writes through the single writer.
use crate::{StorageError, lane_management::*, lanes::LaneRecord};
impl crate::StorageHandle {
    pub fn append_lane_monitor_failure(
        &self,
        request: LaneMonitorFailure,
    ) -> Result<Option<crate::lanes::LaneReportAppended>, StorageError> {
        self.lane_job(true, move |c| {
            crate::repo::lane_management::monitor_failure::append(c, request)
        })
    }

    /// Applies all management effects in one IMMEDIATE transaction. Its commit is
    /// the linearization point. A retry returns the original result (replayed=true).
    /// No lead notification/wake, run start/stop or history mutation is performed.
    pub fn apply_lane_management_transition(
        &self,
        request: LaneManagementRequest,
        actor: LaneManagementActor,
        attach_guard: Option<LaneAttachGuard>,
        at_ns: i64,
    ) -> Result<LaneManagementResult, StorageError> {
        self.lane_job(true, move |c| {
            crate::repo::lane_management::apply(c, &request, &actor, attach_guard, at_ns)
        })
    }
    pub fn lane_family_topology_revision(&self) -> Result<u64, StorageError> {
        self.lane_job(false, crate::repo::lane_management::topology_revision)
    }
    /// Rehydrate or repair these projections after a crash. Bounded, stable order.
    pub fn pending_lane_management_projections(
        &self,
        limit: usize,
    ) -> Result<Vec<LaneRecord>, StorageError> {
        self.lane_job(false, move |c| {
            crate::repo::lane_management::pending_projections(c, limit)
        })
    }
    /// Called only after the host has durably saved the projection of this revision.
    pub fn acknowledge_lane_management_projection(
        &self,
        lane_id: String,
        expected: u64,
    ) -> Result<(), StorageError> {
        self.lane_job(true, move |c| {
            crate::repo::lane_management::ack_projection(c, &lane_id, expected)
        })
    }
}
