//! Host service of the `lanes` area: lane listings and details, and the human
//! action that delivers a lead's held notices again.
//!
//! The lane ledger is the source of truth. A lane's state is read from it on
//! every query; an event only names the lane that changed, and the summary it
//! carries is read from the ledger at publication, so an event that arrives
//! late can never move a client back to an older state. Closing or continuing
//! a lane has no gate of its own here, and a Ready is shown against the brief
//! revision the lane had received when it reported.
use std::collections::BTreeMap;
use std::sync::Arc;

use event_bus::RecvError;
use event_bus::lane::{
    LaneAttentionAction as StoredAction, LaneAttentionFilter, LaneAttentionKind as StoredKind,
    LanePolicy, LaneReadyState as StoredReady, LaneStatus,
};
use event_bus::{EventKind, LedgerEvent};
use runtime::AgentRuntime;
use storage::lanes::LaneRecord;
use workbench_protocol::errors::{ErrorCode, RetryAdvice, WorkbenchError};
use workbench_protocol::events::UiEvent;
use workbench_protocol::ids::{RunId, ThreadId};
use workbench_protocol::lane_attention::{LaneAttentionAction, LaneId, LaneReadyState};
use workbench_protocol::lane_management::{LaneManagementPolicy, ManagedLanePolicy};
use workbench_protocol::namespaces::lanes::{
    LaneAttentionBrief, LaneAttentionTag, LaneDeliveriesResumed, LaneDeliveryView, LaneDetail,
    LaneReadyView, LaneReportStatus, LaneReportView, LaneStartState, LaneSummary, ListLanes,
    MAX_LANE_DETAIL_ATTENTION, MAX_LANE_EVENT_THREADS, ResumeLaneDeliveries,
};
use workbench_protocol::namespaces::{LanesEvent, LanesQuery, LanesReply};
use workbench_protocol::queries::{Query, QueryReply};
use workbench_protocol::wire::{Revision, Sequence, UnixMillis};

use super::{AreaHandles, ProjectionSource};
use crate::bootstrap::BootstrapError;
use crate::queries::{Pager, error, revision, storage_error, text};

/// The `lanes` service, reached from `ingress/namespaces/lanes.rs` as
/// `self.areas.lanes`.
#[derive(Default)]
pub struct LanesArea {
    runtime: Option<AgentRuntime>,
    pager: Pager,
}

impl LanesArea {
    pub(super) fn new(handles: &AreaHandles) -> Result<Self, BootstrapError> {
        let area = Self::with_runtime(handles.runtime.clone());
        let source = handles.source("lanes")?;
        let runtime = handles.runtime.clone();
        let mut events = handles.bus.subscribe();
        handles
            .tasks
            .spawn("lanes-events", move |mut stop| async move {
                let mut leads = BTreeMap::new();
                loop {
                    let event = tokio::select! {
                        () = stop.stopped() => break,
                        event = events.recv() => event,
                    };
                    match event {
                        Ok(event) => publish_change(&runtime, &source, &mut leads, &event.kind),
                        Err(RecvError::Lagged(_)) => {}
                        Err(RecvError::Closed) => break,
                    }
                }
                Ok(())
            })
            .map_err(|error| BootstrapError::from(error.to_string()))?;
        Ok(area)
    }

    /// A service over `runtime` with no event task.
    pub fn with_runtime(runtime: AgentRuntime) -> Self {
        Self {
            runtime: Some(runtime),
            pager: Pager::default(),
        }
    }

    /// Answer a query of the lanes namespace that the lane management adapter
    /// does not.
    ///
    /// # Errors
    /// `NotFound` for an unknown lane, `Conflict` for a cursor of another query
    /// or snapshot.
    pub fn query(&self, query: &Query, q: &LanesQuery) -> Result<QueryReply, WorkbenchError> {
        let runtime = self
            .runtime
            .as_ref()
            .ok_or_else(|| error(ErrorCode::NotFound, "the lanes service is not wired"))?;
        let reply = match q {
            LanesQuery::GetLaneManagement { .. } => {
                return Err(error(
                    ErrorCode::InvalidRequest,
                    "answered by lane management",
                ));
            }
            LanesQuery::ListLanes(list) => LanesReply::Lanes(self.list(runtime, query, list)?),
            LanesQuery::GetLane { lane_id } => {
                LanesReply::Lane(Box::new(lane_detail(runtime, lane_id)?))
            }
        };
        Ok(QueryReply::Lanes(reply))
    }

    fn list(
        &self,
        runtime: &AgentRuntime,
        query: &Query,
        list: &ListLanes,
    ) -> Result<workbench_protocol::views::Page<LaneSummary>, WorkbenchError> {
        let policy = list.policy.map(|policy| match policy {
            ManagedLanePolicy::Attached => LanePolicy::Attached,
            ManagedLanePolicy::Summary => LanePolicy::Summary,
        });
        let lead = list.lead_thread.as_str().to_owned();
        let items = runtime
            .with_storage_reader(|db| {
                let mut records = if list.detached {
                    // The `policy` filter does not apply: every detached lane is Detached.
                    db.detached_lanes_for(&lead)?
                } else {
                    let mut managed = db.managed_lanes(&lead)?;
                    managed.retain(|record| policy.is_none_or(|policy| record.policy == policy));
                    managed
                };
                // Newest activity first, ties by lane id: a stable order for paging.
                records.sort_by(|left, right| {
                    right
                        .last_activity_at_ns
                        .cmp(&left.last_activity_at_ns)
                        .then_with(|| left.lane_id.cmp(&right.lane_id))
                });
                let mut out = Vec::with_capacity(records.len());
                for record in &records {
                    let report = latest_report(db, record)?;
                    if let Some(summary) = summary_of(record, report.as_ref()) {
                        out.push(summary);
                    }
                }
                Ok(out)
            })
            .map_err(storage_error)?
            .ok_or_else(|| error(ErrorCode::StorageUnavailable, "storage is unavailable"))?;
        let mut key = query.clone();
        if let Query::Lanes(LanesQuery::ListLanes(list)) = &mut key {
            list.page.cursor = None;
        }
        self.pager.page(
            &serde_json::to_string(&key).expect("serializable query"),
            &items,
            &list.page,
            revision(&items),
        )
    }
}

/// The runtime's source name for a delivery-visible state change (Stop, held notices, resume).
const DELIVERY_SOURCE: &str = "lane_delivery";

/// The lead each lane had when it was last announced, so that the next change tells the lead
/// it left even when the ledger has moved on to another lead by the time it is read.
type AnnouncedLeads = BTreeMap<String, ThreadId>;

fn publish_change(
    runtime: &AgentRuntime,
    source: &ProjectionSource,
    leads: &mut AnnouncedLeads,
    kind: &EventKind,
) {
    let summaries = match kind {
        EventKind::Ledger(event) => {
            let lane_id = match event {
                LedgerEvent::LaneCreated { lane_id, .. }
                | LedgerEvent::LaneReportAppended { lane_id, .. }
                | LedgerEvent::LaneManagementChanged { lane_id, .. }
                | LedgerEvent::LaneBriefRevised { lane_id, .. } => lane_id,
                _ => return,
            };
            read_summaries(runtime, |db| Ok(db.lane(lane_id)?.into_iter().collect()))
        }
        // The delivery state of a lead (and of a lane's own thread) changed. The event carries
        // no state: every lane on either side is read again from the ledger.
        EventKind::Diagnostic(event) if event.source == DELIVERY_SOURCE => {
            let Some(thread) = event.thread_id.as_deref() else {
                return;
            };
            read_summaries(runtime, |db| {
                let mut lanes = db.managed_lanes(thread)?;
                lanes.extend(db.lane_by_thread(thread)?);
                Ok(lanes)
            })
        }
        _ => return,
    };
    for (lane, mut also_for) in summaries {
        let at_event = match kind {
            EventKind::Ledger(LedgerEvent::LaneManagementChanged { lead_thread_id, .. }) => {
                lead_thread_id
                    .as_deref()
                    .and_then(|thread| ThreadId::new(thread).ok())
            }
            _ => lane.lead_thread_id.clone(),
        };
        tell_leads(leads, &lane, at_event, &mut also_for);
        if let Err(error) =
            source.publish(UiEvent::Lanes(LanesEvent::LaneChanged { lane, also_for }))
        {
            tracing::warn!(?error.code, "lanes event was not published");
        }
    }
}

/// Adds to `also_for` the lead `lane` had at the event and the one it had at the previous
/// event, whichever the ledger names now: operations can run ahead of the events, and a lead
/// that was left would otherwise never hear of it.
fn tell_leads(
    leads: &mut AnnouncedLeads,
    lane: &LaneSummary,
    at_event: Option<ThreadId>,
    also_for: &mut Vec<ThreadId>,
) {
    let before = match &at_event {
        Some(lead) => leads.insert(lane.lane_id.as_str().to_owned(), lead.clone()),
        None => leads.remove(lane.lane_id.as_str()),
    };
    for thread in [before, at_event].into_iter().flatten() {
        if lane.lead_thread_id.as_ref() != Some(&thread) && !also_for.contains(&thread) {
            also_for.push(thread);
        }
    }
    also_for.truncate(MAX_LANE_EVENT_THREADS);
}

/// The summaries of the lanes `lanes` reads, each with the threads that show it besides its
/// own thread and current lead: the leads that find it again once detached (its origin and
/// its last lead, so the lead that just detached it hears of the detach).
fn read_summaries(
    runtime: &AgentRuntime,
    lanes: impl FnOnce(&storage::Database) -> Result<Vec<LaneRecord>, storage::StorageError>,
) -> Vec<(LaneSummary, Vec<ThreadId>)> {
    runtime
        .with_storage_reader(|db| {
            let mut out = Vec::new();
            for record in lanes(db)? {
                let report = latest_report(db, &record)?;
                let also_for = db
                    .lane_finders(&record.lane_id)?
                    .iter()
                    .filter_map(|thread| ThreadId::new(thread).ok())
                    .collect();
                out.extend(summary_of(&record, report.as_ref()).map(|summary| (summary, also_for)));
            }
            Ok(out)
        })
        .ok()
        .flatten()
        .unwrap_or_default()
}

fn latest_report(
    db: &storage::Database,
    lane: &LaneRecord,
) -> Result<Option<event_bus::lane::LaneReport>, storage::StorageError> {
    if lane.latest_report_seq == 0 {
        return Ok(None);
    }
    Ok(db
        .lane_reports(&lane.lane_id, lane.latest_report_seq - 1, 1)?
        .into_iter()
        .next())
}

fn management_policy(policy: LanePolicy) -> LaneManagementPolicy {
    match policy {
        LanePolicy::Attached => LaneManagementPolicy::Attached,
        LanePolicy::Summary => LaneManagementPolicy::Summary,
        LanePolicy::Detached => LaneManagementPolicy::Detached,
    }
}

fn report_status(status: LaneStatus) -> LaneReportStatus {
    match status {
        LaneStatus::Working => LaneReportStatus::Working,
        LaneStatus::Waiting => LaneReportStatus::Waiting,
        LaneStatus::Blocked => LaneReportStatus::Blocked,
        LaneStatus::Ready => LaneReportStatus::Ready,
        LaneStatus::Failed => LaneReportStatus::Failed,
    }
}

fn millis(ns: i64) -> UnixMillis {
    UnixMillis((ns.max(0) / 1_000_000) as u64)
}

fn summary_of(
    lane: &LaneRecord,
    report: Option<&event_bus::lane::LaneReport>,
) -> Option<LaneSummary> {
    let start = if lane.is_started() {
        LaneStartState::Started
    } else if lane.is_operator_stopped() {
        LaneStartState::OperatorStopped
    } else if lane.start_blocked_reason.is_some() {
        LaneStartState::StartBlocked
    } else {
        LaneStartState::NotStarted
    };
    let mut summary = LaneSummary {
        lane_id: LaneId::new(&lane.lane_id).ok()?,
        thread_id: ThreadId::new(&lane.thread_id).ok()?,
        lead_thread_id: lane
            .lead_thread_id
            .as_ref()
            .and_then(|lead| ThreadId::new(lead).ok()),
        policy: management_policy(lane.policy),
        management_revision: Revision(lane.management_revision),
        brief_revision: Revision(lane.brief_revision),
        objective_preview: text(&lane.brief.objective, 512),
        root_run: lane
            .root_run_id
            .as_ref()
            .and_then(|run| RunId::new(run).ok()),
        generation: Revision(lane.generation),
        start,
        last_activity_at: millis(lane.last_activity_at_ns),
        latest_report_status: report.map(|report| report_status(report.status)),
        revision: Revision(0),
    };
    summary.revision = revision(&summary);
    Some(summary)
}

fn readiness(value: StoredReady) -> LaneReadyState {
    match value {
        StoredReady::NoEvidence => LaneReadyState::NoEvidence,
        StoredReady::Unreviewed => LaneReadyState::Unreviewed,
        StoredReady::ReviewInProgress => LaneReadyState::ReviewInProgress,
        StoredReady::ReviewRejected => LaneReadyState::ReviewRejected,
        StoredReady::ReviewIncomplete => LaneReadyState::ReviewIncomplete,
        StoredReady::AwaitingClose => LaneReadyState::AwaitingClose,
    }
}

fn action(value: StoredAction) -> LaneAttentionAction {
    match value {
        StoredAction::Answer => LaneAttentionAction::Answer,
        StoredAction::RouteToUser => LaneAttentionAction::RouteToUser,
        StoredAction::Continue => LaneAttentionAction::Continue,
        StoredAction::Inspect => LaneAttentionAction::Inspect,
        StoredAction::Verify => LaneAttentionAction::Verify,
        StoredAction::Close => LaneAttentionAction::Close,
        StoredAction::Resume => LaneAttentionAction::Resume,
    }
}

fn tag(kind: &StoredKind) -> LaneAttentionTag {
    match kind {
        StoredKind::QuestionPending { .. } => LaneAttentionTag::QuestionPending,
        StoredKind::Blocked { .. } => LaneAttentionTag::Blocked,
        StoredKind::Failed { .. } => LaneAttentionTag::Failed,
        StoredKind::RestoreFailed { .. } => LaneAttentionTag::RestoreFailed,
        StoredKind::ReadyUnclosed { .. } => LaneAttentionTag::ReadyUnclosed,
        StoredKind::QuestionCapacityBlocked { .. } => LaneAttentionTag::QuestionCapacityBlocked,
        StoredKind::WakeSuppressed { .. } => LaneAttentionTag::WakeSuppressed,
        StoredKind::StopPending { .. } => LaneAttentionTag::StopPending,
        StoredKind::Stall { .. } => LaneAttentionTag::Stall,
    }
}

fn lane_detail(runtime: &AgentRuntime, lane_id: &LaneId) -> Result<LaneDetail, WorkbenchError> {
    let read = runtime
        .with_storage_reader(|db| {
            let Some(lane) = db.lane(lane_id.as_str())? else {
                return Ok(None);
            };
            let report = latest_report(db, &lane)?;
            let items = match &lane.lead_thread_id {
                Some(lead) => {
                    db.lane_attention(
                        LaneAttentionFilter {
                            lead_thread_id: lead.clone(),
                            project_id: None,
                        },
                        None,
                        usize::MAX,
                    )?
                    .items
                }
                None => Vec::new(),
            }
            .into_iter()
            .filter(|item| item.lane_id == lane.lane_id)
            .collect::<Vec<_>>();
            let destination_stopped = match &lane.lead_thread_id {
                Some(lead) => db
                    .lane_delivery_destination(lead)?
                    .is_some_and(|destination| destination.stopped),
                None => false,
            };
            Ok(Some((lane, report, items, destination_stopped)))
        })
        .map_err(storage_error)?
        .ok_or_else(|| error(ErrorCode::StorageUnavailable, "storage is unavailable"))?;
    let Some((lane, report, items, destination_stopped)) = read else {
        return Err(error(ErrorCode::NotFound, "unknown lane"));
    };
    let summary = summary_of(&lane, report.as_ref())
        .ok_or_else(|| error(ErrorCode::Internal, "the lane has an invalid identity"))?;
    let readiness_of = items.iter().find_map(|item| match &item.kind {
        StoredKind::ReadyUnclosed {
            readiness: state, ..
        } => Some(readiness(*state)),
        _ => None,
    });
    let held_notices = items.iter().find_map(|item| match &item.kind {
        StoredKind::StopPending { pending_count, .. } => Some(*pending_count),
        _ => None,
    });
    let latest = report.as_ref().map(|report| LaneReportView {
        seq: Sequence(report.report_seq),
        status: report_status(report.status),
        summary: text(&report.summary, 1024),
        needs_human: report.needs_human,
        management_revision: Revision(report.management_revision),
        brief_revision: Revision(report.brief_revision),
        created_at: millis(report.created_at_ns),
    });
    let ready = report
        .as_ref()
        .filter(|report| report.status == LaneStatus::Ready)
        .map(|report| LaneReadyView {
            report_seq: Sequence(report.report_seq),
            brief_revision: Revision(report.brief_revision),
            brief_changed_since: lane.brief_revision > report.brief_revision,
            readiness: readiness_of.unwrap_or(LaneReadyState::NoEvidence),
        });
    Ok(LaneDetail {
        detached_since_management_revision: lane.detached_since_management_revision.map(Revision),
        latest_report: latest,
        ready,
        delivery: LaneDeliveryView {
            held: destination_stopped || held_notices.is_some(),
            pending_count: held_notices.map_or(0, |count| u32::try_from(count).unwrap_or(u32::MAX)),
        },
        attention: items
            .iter()
            .take(MAX_LANE_DETAIL_ATTENTION)
            .map(|item| LaneAttentionBrief {
                tag: tag(&item.kind),
                summary: text(&item.summary, 1024),
                revision: Revision(item.revision),
                since: millis(item.since_at_ns),
                available_actions: item.available_actions.iter().copied().map(action).collect(),
            })
            .collect(),
        summary,
    })
}

fn refusal(code: &str) -> WorkbenchError {
    let kind = match code {
        "stale_management_revision" | "not_the_threads_root" => ErrorCode::StaleRevision,
        "ownership_lost" | "not_current_lead" => ErrorCode::PermissionDenied,
        "lane_unavailable" => ErrorCode::NotFound,
        "storage_unavailable" => ErrorCode::StorageUnavailable,
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

/// Deliver the notices held for `request.lead_thread_id` again, if the request
/// still names the lane's current management revision, its current lead and the
/// lead's current root run. A request made against an older one changes
/// nothing. The caller has checked that the actor is a human.
///
/// # Errors
/// `StaleRevision` for a management revision or a root run that is no longer
/// current, `PermissionDenied` for a lead that is not the lane's or an
/// ownership the host lost, `Conflict` when the runtime refuses (for example
/// the lead was stopped and must be continued first), `NotFound`.
pub fn resume_deliveries(
    runtime: &AgentRuntime,
    ownership: Option<&Arc<runtime::ownership::OwnerHost>>,
    request: &ResumeLaneDeliveries,
) -> Result<LaneDeliveriesResumed, WorkbenchError> {
    let lane = runtime
        .with_storage_reader(|db| db.lane(request.lane_id.as_str()))
        .map_err(|_| refusal("storage_unavailable"))?
        .flatten()
        .ok_or_else(|| refusal("lane_unavailable"))?;
    if Revision(lane.management_revision) != request.expected_management_revision {
        return Err(refusal("stale_management_revision"));
    }
    if lane.lead_thread_id.as_deref() != Some(request.lead_thread_id.as_str()) {
        return Err(refusal("not_current_lead"));
    }
    let run = request
        .expected_lead_root_run_id
        .as_str()
        .parse::<runtime::RunId>()
        .map_err(|_| refusal("not_the_threads_root"))?;
    let owner = ownership.ok_or_else(|| refusal("ownership_lost"))?;
    let permit = owner
        .owned_permit(request.lead_thread_id.as_str())
        .map_err(|_| refusal("ownership_lost"))?;
    runtime
        .resume_lane_deliveries(request.lead_thread_id.as_str(), run, Some(&permit))
        .map_err(|error| {
            let reason = error.to_string();
            if reason.contains("not_the_threads_root") {
                refusal("not_the_threads_root")
            } else if reason.contains("lead_stopped") {
                refusal("lead_stopped")
            } else {
                refusal("resume_refused")
            }
        })?;
    Ok(LaneDeliveriesResumed {
        lane_id: request.lane_id.clone(),
        lead_thread_id: request.lead_thread_id.clone(),
        root_run_id: request.expected_lead_root_run_id.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lane(current_lead: Option<&str>) -> LaneSummary {
        serde_json::from_value(serde_json::json!({
            "lane_id": "l1", "thread_id": "thread-l1", "lead_thread_id": current_lead,
            "policy": "attached", "management_revision": "1", "brief_revision": "1",
            "objective_preview": "o", "generation": "1", "start": "started",
            "last_activity_at": "1", "revision": "1"
        }))
        .unwrap()
    }

    fn thread(id: &str) -> ThreadId {
        ThreadId::new(id).unwrap()
    }

    /// Lead A hands the lane to B, which detaches it, and C takes it, all before the first
    /// event is read: each event is read against the ledger as it is by then (led by C).
    #[test]
    fn a_lead_left_by_the_lane_hears_of_the_change_even_when_the_ledger_ran_ahead() {
        let mut leads = AnnouncedLeads::new();
        let told = |leads: &mut AnnouncedLeads, at_event: Option<&str>| {
            let mut also_for = Vec::new();
            tell_leads(
                leads,
                &lane(Some("lead-c")),
                at_event.map(thread),
                &mut also_for,
            );
            also_for
        };
        assert_eq!(told(&mut leads, Some("lead-b")), [thread("lead-b")]);
        // The detach: B was the lead before it, and is named though the ledger says C.
        assert_eq!(told(&mut leads, None), [thread("lead-b")]);
        // C takes it after the detach: the detach already told B, and nothing is left over.
        assert!(told(&mut leads, Some("lead-c")).is_empty());
    }
}
