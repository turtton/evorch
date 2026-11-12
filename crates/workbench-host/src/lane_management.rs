//! Human management adapter shared by desktop and core. No client/GUI crate is
//! needed: SQL owns membership; the sidebar lock fences independent-family edits.
use crate::CommandHost;
use runtime::{AgentRuntime, lane_management as rt};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use storage::lanes::LaneRecord;
use workbench_protocol::{
    commands::{CommandDisposition, CommandReply, CommandResult},
    envelope::{Actor, Provenance},
    errors::{ErrorCode, RetryAdvice, WorkbenchError},
    ids::ThreadId,
    lane_management as wire,
    namespaces::LanesResult,
    wire::Revision,
};

/// Install with the SAME runtime clone used by this host's delivery worker.
/// An ingress without an ownership registry fails closed.
#[derive(Clone)]
pub struct HumanLaneManagement {
    runtime: AgentRuntime,
    ownership: Option<Arc<runtime::ownership::OwnerHost>>,
}
impl HumanLaneManagement {
    pub fn new(
        runtime: AgentRuntime,
        ownership: Option<Arc<runtime::ownership::OwnerHost>>,
    ) -> Self {
        Self { runtime, ownership }
    }
    /// A human ends a lane's ledger entry (ADR 0044): `human_closed`, or `discarded` when
    /// the request says so. Only the ledger changes: no run is stopped, no thread or
    /// worktree deleted, and the lane stays attached. Callers must already have checked
    /// that the actor is a human (see [`CommandHost::close_lane_human`]).
    pub fn close_lane(
        &self,
        request: runtime::lane_close::HumanLaneCloseRequest,
    ) -> Result<event_bus::lane::LaneCloseResult, WorkbenchError> {
        let lane = read_lane(&self.runtime, &request.lane_id)?;
        let mut threads = vec![lane.thread_id];
        threads.extend(lane.lead_thread_id);
        threads.sort();
        threads.dedup();
        let owner = self
            .ownership
            .as_ref()
            .ok_or_else(|| failure("ownership_lost"))?;
        let permits = threads
            .iter()
            .map(|thread| {
                owner
                    .owned_permit(thread)
                    .map_err(|_| failure("ownership_lost"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.runtime
            .close_lane_from_host(rt::LaneHumanContext, request, &permits)
            .map_err(|e| failure(e.code))
    }
    /// Caller holds the authoritative family lock through this call and any save.
    pub fn apply(
        &self,
        request: rt::LaneManagementRequest,
        guard: Option<rt::LaneAttachGuard>,
    ) -> Result<rt::LaneManagementOutcome, WorkbenchError> {
        let lane = read_lane(&self.runtime, &request.lane_id)?;
        let mut threads = vec![lane.thread_id];
        threads.extend(lane.lead_thread_id);
        if let rt::LaneManagementAction::Reattach { lead_thread_id, .. } = &request.action {
            threads.push(lead_thread_id.clone());
        }
        threads.sort();
        threads.dedup();
        let owner = self
            .ownership
            .as_ref()
            .ok_or_else(|| failure("ownership_lost"))?;
        let permits = threads
            .iter()
            .map(|thread| {
                owner
                    .owned_permit(thread)
                    .map_err(|_| failure("ownership_lost"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.runtime
            .manage_lane_from_host(rt::LaneHumanContext, request, guard, &permits)
            .map_err(|e| failure(e.code))
    }
}
fn failure(code: &str) -> WorkbenchError {
    let kind = match code {
        "stale_management_revision"
        | "stale_family_revision"
        | "stale_management"
        | "stale_request"
        | "stale_ready"
        | "stale_brief" => ErrorCode::StaleRevision,
        "ownership_lost" | "not_current_lead" | "human_action_required" => {
            ErrorCode::PermissionDenied
        }
        "storage_unavailable" => ErrorCode::StorageUnavailable,
        "lane_unavailable" => ErrorCode::NotFound,
        "invalid_input" => ErrorCode::InvalidRequest,
        _ => ErrorCode::Conflict,
    };
    WorkbenchError::new(
        kind,
        code,
        if kind == ErrorCode::StaleRevision {
            RetryAdvice::AfterResync
        } else {
            RetryAdvice::Never
        },
    )
}
fn read_lane(runtime: &AgentRuntime, id: &str) -> Result<LaneRecord, WorkbenchError> {
    runtime
        .with_storage_reader(|db| db.lane(id))
        .map_err(|_| failure("storage_unavailable"))?
        .flatten()
        .ok_or_else(|| failure("lane_unavailable"))
}
fn policy(policy: event_bus::LanePolicy) -> wire::LaneManagementPolicy {
    match policy {
        event_bus::LanePolicy::Attached => wire::LaneManagementPolicy::Attached,
        event_bus::LanePolicy::Summary => wire::LaneManagementPolicy::Summary,
        event_bus::LanePolicy::Detached => wire::LaneManagementPolicy::Detached,
    }
}
fn request(value: wire::LaneManagementRequest) -> rt::LaneManagementRequest {
    rt::LaneManagementRequest {
        operation_id: value.operation_id.as_str().to_owned(),
        lane_id: value.lane_id.as_str().to_owned(),
        expected_management_revision: value.expected_management_revision.0,
        action: match value.action {
            wire::LaneManagementAction::SetPolicy { policy } => {
                rt::LaneManagementAction::SetPolicy {
                    policy: match policy {
                        wire::ManagedLanePolicy::Attached => rt::ManagedLanePolicy::Attached,
                        wire::ManagedLanePolicy::Summary => rt::ManagedLanePolicy::Summary,
                    },
                }
            }
            wire::LaneManagementAction::Detach => rt::LaneManagementAction::Detach,
            wire::LaneManagementAction::Reattach {
                lead_thread_id,
                expected_lead_root_run_id,
            } => rt::LaneManagementAction::Reattach {
                lead_thread_id: lead_thread_id.as_str().into(),
                expected_lead_root_run_id: expected_lead_root_run_id.as_str().into(),
            },
        },
    }
}

// The core reads only the independent family's state, never GUI drafts or
// derived tree caches. Missing/malformed files fail closed. It never saves or
// ACKs sidebar projection: a desktop repairs that durable pending revision.
#[derive(serde::Deserialize)]
struct SidebarFamilies {
    version: u32,
    families: Vec<Family>,
}
#[derive(serde::Deserialize)]
struct Family {
    key: String,
    #[serde(default)]
    membership_epoch: u64,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    archive_revision: u64,
    #[serde(default)]
    pinned: bool,
    #[serde(default)]
    pin_revision: u64,
}
struct FamilyLock(std::fs::File);
impl FamilyLock {
    fn acquire(path: &Path) -> Result<Self, WorkbenchError> {
        let mut name = path
            .file_name()
            .ok_or_else(|| failure("storage_unavailable"))?
            .to_os_string();
        name.push(".lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path.with_file_name(name))
            .map_err(|_| failure("storage_unavailable"))?;
        file.try_lock().map_err(|_| {
            WorkbenchError::new(
                ErrorCode::Busy,
                "sidebar is locked",
                RetryAdvice::SameRequest,
            )
        })?;
        Ok(Self(file))
    }
}
impl Drop for FamilyLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}
fn family(
    runtime: &AgentRuntime,
    path: &Path,
    lane: &LaneRecord,
) -> Result<(wire::LaneFamilyRevision, rt::LaneAttachGuard), WorkbenchError> {
    let epoch = lane
        .detached_since_management_revision
        .ok_or_else(|| failure("invalid_policy_transition"))?;
    let sidebar: SidebarFamilies =
        serde_json::from_slice(&std::fs::read(path).map_err(|_| failure("storage_unavailable"))?)
            .map_err(|_| failure("projection_pending"))?;
    if sidebar.version != 2 {
        return Err(failure("projection_pending"));
    }
    let row = runtime
        .with_storage_reader(|db| db.thread(&lane.thread_id))
        .map_err(|_| failure("storage_unavailable"))?
        .flatten()
        .ok_or_else(|| failure("lane_unavailable"))?;
    let mut revision = wire::LaneFamilyRevision {
        key: ThreadId::new(&lane.thread_id)?,
        membership_epoch: Revision(epoch),
        archive_revision: Revision(row.archive_revision.unwrap_or(0)),
        pin_revision: Revision(row.pin_revision.unwrap_or(0)),
    };
    let mut guard = rt::LaneAttachGuard {
        pinned: row.pinned,
        archived: row.archived,
    };
    let mut matching = sidebar
        .families
        .iter()
        .filter(|f| f.key == lane.thread_id && f.membership_epoch == epoch);
    if let Some(f) = matching.next() {
        if matching.next().is_some() {
            return Err(failure("projection_pending"));
        }
        revision.archive_revision.0 = revision.archive_revision.0.max(f.archive_revision);
        revision.pin_revision.0 = revision.pin_revision.0.max(f.pin_revision);
        // Conservative when a file edit has not yet reached its SQL mirror.
        guard.pinned |= f.pinned;
        guard.archived |= f.archived;
    }
    Ok((revision, guard))
}
impl CommandHost {
    /// Human close or discard of a lane's ledger entry, for the desktop and core host
    /// to call from a verified human action. This is deliberately not a wire
    /// `Command` variant, and no agent tool, MCP method or agent dispatch reaches it: an
    /// agent provenance is refused, so a claimed human actor in a message cannot close.
    pub fn close_lane_human(
        &mut self,
        request: runtime::lane_close::HumanLaneCloseRequest,
        provenance: &Provenance,
    ) -> Result<event_bus::lane::LaneCloseResult, WorkbenchError> {
        if provenance.actor != Actor::Human {
            return Err(failure("human_action_required"));
        }
        let mut request = request;
        // The record names the surface and client the host authenticated, not what the
        // caller put in the request.
        request.surface = serde_json::to_value(provenance.surface)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| "unknown".into());
        request.client_id = provenance.client_id.as_ref().map(|c| c.as_str().to_owned());
        let adapter = HumanLaneManagement::new(
            self.service().runtime().clone(),
            self.service().ownership().cloned(),
        );
        let result = adapter.close_lane(request)?;
        if let Err(error) = self.projection().refresh() {
            tracing::warn!(?error.code, "lane projection refresh failed");
        }
        Ok(result)
    }
    pub fn with_lane_sidebar_path(mut self, path: PathBuf) -> Self {
        self.lane_sidebar_path = Some(path);
        self
    }
    pub(crate) fn manage_lane_command(
        &mut self,
        input: wire::ManageLane,
        provenance: &Provenance,
    ) -> Result<CommandReply, WorkbenchError> {
        if provenance.actor != Actor::Human {
            return Err(failure("human_action_required"));
        }
        let adapter = HumanLaneManagement::new(
            self.service().runtime().clone(),
            self.service().ownership().cloned(),
        );
        let request = request(input.request);
        let outcome = if matches!(request.action, rt::LaneManagementAction::Reattach { .. }) {
            if !matches!(
                provenance.surface,
                workbench_protocol::envelope::Surface::Desktop
                    | workbench_protocol::envelope::Surface::Web
            ) {
                return Err(failure("human_action_required"));
            }
            // A replay is answered before validating a now-obsolete drag state.
            // A new transition cannot commit without its authoritative family guard.
            match adapter.apply(request.clone(), None) {
                Ok(replayed) => replayed,
                Err(error) if error.message == "stale_family_revision" => {
                    let path = self
                        .lane_sidebar_path
                        .as_ref()
                        .ok_or_else(|| failure("storage_unavailable"))?;
                    let _lock = FamilyLock::acquire(path)?;
                    let lane = read_lane(self.service().runtime(), &request.lane_id)?;
                    let (revision, guard) = family(self.service().runtime(), path, &lane)?;
                    if input.expected_family_revision.as_ref() != Some(&revision) {
                        return Err(failure("stale_family_revision"));
                    }
                    adapter.apply(request, Some(guard))?
                }
                Err(error) => return Err(error),
            }
        } else {
            if input.expected_family_revision.is_some() {
                return Err(failure("invalid_input"));
            }
            adapter.apply(request, None)?
        };
        let op = outcome.operation;
        let lane = op.lane;
        let result = wire::LaneManagementResult {
            operation_id: wire::LaneManagementOperationId::new(op.operation_id)?,
            lane_id: workbench_protocol::lane_attention::LaneId::new(lane.lane_id)?,
            thread_id: ThreadId::new(lane.thread_id)?,
            lead_thread_id: lane.lead_thread_id.map(ThreadId::new).transpose()?,
            policy: policy(lane.policy),
            management_revision: Revision(lane.management_revision),
            detached_since_management_revision: lane
                .detached_since_management_revision
                .map(Revision),
            family_topology_revision: Revision(op.family_topology_revision),
            changed: op.changed,
            replayed: op.replayed,
            projection_pending: op.projection_pending || outcome.refresh_pending,
        };
        if let Err(error) = self.projection().refresh() {
            tracing::warn!(?error.code, "lane projection refresh failed");
        }
        Ok(CommandReply {
            disposition: if result.replayed {
                CommandDisposition::AlreadyApplied
            } else {
                CommandDisposition::Accepted
            },
            ..CommandReply::with_result(CommandResult::Lanes(LanesResult::Managed(result)))
        })
    }
    pub(crate) fn lane_management_state(
        &self,
        id: &str,
    ) -> Result<wire::LaneManagementState, WorkbenchError> {
        let runtime = self.service().runtime();
        let lane = read_lane(runtime, id)?;
        let family_revision = if lane.detached_since_management_revision.is_some() {
            self.lane_sidebar_path.as_ref().and_then(|path| {
                let _lock = FamilyLock::acquire(path).ok()?;
                family(runtime, path, &lane).ok().map(|(rev, _)| rev)
            })
        } else {
            None
        };
        Ok(wire::LaneManagementState {
            lane_id: workbench_protocol::lane_attention::LaneId::new(lane.lane_id)?,
            thread_id: ThreadId::new(lane.thread_id)?,
            lead_thread_id: lane.lead_thread_id.map(ThreadId::new).transpose()?,
            policy: policy(lane.policy),
            management_revision: Revision(lane.management_revision),
            detached_since_management_revision: lane
                .detached_since_management_revision
                .map(Revision),
            family_revision,
        })
    }
}
