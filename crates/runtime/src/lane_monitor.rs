//! Owned execution observations. Timer progress is deliberately not the durable
//! episode anchor: working prose, retries and wake generations cannot buy nudges.
use crate::{AgentRuntime, RunId, lane_delivery::LaneDeliveryTarget, lanes::LaneError};
use event_bus::lane::{LaneNotificationKind, LaneNudgeState, LaneStallClassification as Class};
use event_bus::{AgentRunPhase, EventKind, LanePolicy, LaneStatus, ProviderEvent, ToolEvent};
use std::collections::{HashMap, HashSet};
use storage::{
    lane_control::ObserveLaneStall,
    lane_delivery::{LaneDeliveryClaim, LaneDeliveryNack, LaneDestinationAction},
};

#[cfg(test)]
thread_local! {
    pub(crate) static BEFORE_FAILURE_COMMIT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}
#[derive(Clone, Copy)]
pub(crate) struct MonitorSettings {
    no_report: i64,
    stalled: i64,
    grace: i64,
}
impl Default for MonitorSettings {
    fn default() -> Self {
        Self {
            no_report: 30 * 60 * 1_000_000_000,
            stalled: 60 * 60 * 1_000_000_000,
            grace: 5 * 60 * 1_000_000_000,
        }
    }
}
#[derive(Default)]
struct Execution {
    activity: Option<event_bus::RunActivity>,
    tools: HashSet<String>,
    requests: HashSet<String>,
    exited_at: Option<i64>,
}
impl Execution {
    fn busy(&self) -> bool {
        !self.tools.is_empty()
            || !self.requests.is_empty()
            || self.activity.is_some_and(|activity| {
                matches!(
                    activity,
                    event_bus::RunActivity::Model
                        | event_bus::RunActivity::Tools
                        | event_bus::RunActivity::Compaction
                        | event_bus::RunActivity::Ownership
                )
            })
    }
}
#[derive(Default)]
pub(crate) struct LaneMonitor {
    events: Option<event_bus::EventReceiver>,
    settings: MonitorSettings,
    execution: HashMap<RunId, Execution>,
    lanes: HashMap<String, Observation>,
}
struct Observation {
    progress_at: i64,
    artifact: Option<String>,
    anchor: event_bus::lane::LaneProgressAnchor,
    exempt: bool,
    grace_until: i64,
    children: HashSet<RunId>,
    episode: Option<(String, u64)>,
}
impl LaneMonitor {
    fn drain(&mut self, now: i64) {
        let (events, lost) = self
            .events
            .as_mut()
            .map_or((vec![], 0), |r| r.drain_pending_snapshot_counting_lag());
        // Missing start/completion events cannot be interpreted as inactivity.
        // Discard observations and re-establish a full observation horizon.
        if lost != 0 {
            self.execution.clear();
            self.lanes.clear();
        }
        for event in events {
            let Some(run) = event.kind.primary_run_id().and_then(|s| s.parse().ok()) else {
                continue;
            };
            if !matches!(
                &event.kind,
                EventKind::Lifecycle(event_bus::LifecycleEvent::RunProgress { .. })
                    | EventKind::Tool(
                        ToolEvent::ToolStarted { .. } | ToolEvent::ToolCompleted { .. }
                    )
                    | EventKind::Provider(
                        ProviderEvent::RequestStarted { .. }
                            | ProviderEvent::RequestCompleted { .. }
                            | ProviderEvent::RequestFailed { .. }
                    )
            ) {
                if matches!(
                    &event.kind,
                    EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
                        to: AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped,
                        ..
                    })
                ) {
                    self.execution.remove(&run);
                }
                continue;
            }
            let state = self.execution.entry(run).or_default();
            let was_busy = state.busy();
            match event.kind {
                EventKind::Lifecycle(event_bus::LifecycleEvent::RunProgress {
                    activity, ..
                }) => {
                    state.activity = Some(activity);
                    if matches!(
                        activity,
                        event_bus::RunActivity::Idle
                            | event_bus::RunActivity::User
                            | event_bus::RunActivity::Children
                    ) {
                        // The loop reached a safe boundary after joining its work.
                        state.tools.clear();
                        state.requests.clear();
                    }
                }
                EventKind::Tool(ToolEvent::ToolStarted { call_id, .. }) => {
                    state.tools.insert(call_id);
                }
                EventKind::Tool(ToolEvent::ToolCompleted { call_id, .. }) => {
                    state.tools.remove(&call_id);
                    state.exited_at = Some(now);
                }
                EventKind::Provider(ProviderEvent::RequestStarted { request_id, .. }) => {
                    state.requests.insert(request_id);
                }
                EventKind::Provider(
                    ProviderEvent::RequestCompleted { request_id, .. }
                    | ProviderEvent::RequestFailed { request_id, .. },
                ) => {
                    state.requests.remove(&request_id);
                    // A bind/lag may hide the start but not this exit boundary.
                    state.exited_at = Some(now);
                }
                _ => {}
            }
            if was_busy && !state.busy() {
                state.exited_at = Some(now);
            }
        }
    }
}
impl AgentRuntime {
    fn lane_healthy_child_wait(
        &self,
        run: RunId,
        phase: Option<AgentRunPhase>,
        monitor: &LaneMonitor,
    ) -> bool {
        phase == Some(AgentRunPhase::Waiting)
            && self.live_descendants(run).into_iter().any(|child| {
                self.has_unanswered_questions(child)
                    || self.lane_compaction_busy(child)
                    || match self.inspect_agent(child).ok().map(|r| r.phase) {
                        None | Some(AgentRunPhase::Pending) => true,
                        // Unknown activity is not a finished wait: after bind/lag only
                        // a whole-run activity boundary re-establishes observability.
                        Some(AgentRunPhase::Running) => monitor
                            .execution
                            .get(&child)
                            .is_none_or(|e| e.activity.is_none() || e.busy()),
                        _ => false,
                    }
            })
    }
    pub(crate) fn start_lane_monitor(&self) {
        let mut monitor = self
            .shared
            .lanes
            .delivery
            .monitor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        monitor
            .events
            .get_or_insert_with(|| self.shared.bus.subscribe());
    }
    /// One owned scan, driven by the delivery worker's startup/Skip timer.
    /// The host clock is injectable and also timestamps the existing lease ledger.
    pub(crate) fn monitor_lanes(&self) -> Result<(), LaneError> {
        let Some(_producer) = self.producer_gate().try_enter() else {
            return Ok(());
        };
        let host = self.lane_monitor_host();
        let Some(host) = host else {
            return Ok(());
        };
        let store = self.delivery_store()?;
        let now = host.now_ns();
        let roots: HashSet<_> = self.goal_lock().roots.values().cloned().collect();
        let mut monitor = self
            .shared
            .lanes
            .delivery
            .monitor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        monitor.drain(now);
        let settings = monitor.settings;
        let mut observed = HashSet::new();
        let mut owned_runs = HashSet::new();
        for thread in roots {
            let Some(lane) = store.handle.lane_by_thread(&thread)? else {
                continue;
            };
            if lane.policy == LanePolicy::Detached || lane.is_operator_stopped() {
                continue;
            }
            let Ok(target) = host.resolve(&thread) else {
                continue;
            };
            // Stored ownership DTOs and absence of local activity are not authority.
            let Some(permit) = target.authority.ownership.as_ref() else {
                continue;
            };
            let Ok(_owner) = permit.mutation_guard() else {
                continue;
            };
            if self.check_delivery_target(&thread, &target).is_err() {
                continue;
            }
            let destination = store.handle.lane_delivery_destination(thread.clone())?;
            if destination.as_ref().is_some_and(|d| d.stopped) {
                continue;
            }
            let owner = target.owner();
            if destination.as_ref().and_then(|d| d.owner.as_ref()) != Some(&owner) {
                store.handle.bind_lane_delivery_destination(
                    thread.clone(),
                    destination.as_ref().map_or(0, |d| d.epoch),
                    owner.clone(),
                    LaneDestinationAction::Bind,
                )?;
            }
            let report = store
                .handle
                .lane_reports(&lane.lane_id, lane.latest_report_seq.saturating_sub(1), 1)?
                .pop();
            let anchor = store.handle.lane_progress_anchor(lane.lane_id.clone())?;
            let phase = self.inspect_agent(target.root_run_id).ok().map(|r| r.phase);
            let phase = phase.or_else(|| {
                store
                    .restore_record(target.root_run_id)
                    .ok()
                    .flatten()
                    .and_then(|r| match r.terminal_phase.as_str() {
                        "Done" => Some(AgentRunPhase::Done),
                        "Error" => Some(AgentRunPhase::Error),
                        _ => None,
                    })
            });
            let question = self.has_unanswered_questions(target.root_run_id);
            let children = self.live_descendants(target.root_run_id);
            let execution = monitor.execution.get(&target.root_run_id);
            owned_runs.insert(target.root_run_id);
            owned_runs.extend(children.iter().copied());
            let healthy_child_wait =
                self.lane_healthy_child_wait(target.root_run_id, phase, &monitor);
            // A completion from a partial event suffix does not prove that all
            // parallel work finished. Only a whole-run activity boundary can
            // re-establish observability after bind/lag.
            let unknown_running = phase == Some(AgentRunPhase::Running)
                && execution.is_none_or(|e| e.activity.is_none());
            let busy = execution.is_some_and(Execution::busy);
            let exited = execution.and_then(|e| e.exited_at);
            let exempt = busy
                || unknown_running
                || self.lane_compaction_busy(target.root_run_id)
                || self.thread_goal(&thread).is_some_and(|g| g.checks_paused)
                || healthy_child_wait;
            let artifact = report
                .as_ref()
                .and_then(|r| r.artifact_revision.as_ref())
                .and_then(|r| r.fingerprint());
            let entry = monitor
                .lanes
                .entry(lane.lane_id.clone())
                .or_insert_with(|| Observation {
                    progress_at: now,
                    artifact: artifact.clone(),
                    anchor: anchor.clone(),
                    exempt: false,
                    grace_until: 0,
                    children: HashSet::new(),
                    episode: None,
                });
            observed.insert(lane.lane_id.clone());
            if entry.anchor != anchor || entry.artifact != artifact {
                entry.progress_at = now;
                entry.anchor = anchor.clone();
                entry.artifact = artifact;
            }
            // A child's first terminal result advances the timer, not the anchor.
            let terminal: Vec<_> = self
                .shared
                .runs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .filter(|(_, r)| r.parent == Some(target.root_run_id))
                .map(|(id, _)| *id)
                .collect();
            for child in terminal {
                if self
                    .inspect_agent(child)
                    .is_ok_and(|r| matches!(r.phase, AgentRunPhase::Done | AgentRunPhase::Error))
                    && entry.children.insert(child)
                {
                    entry.progress_at = now;
                }
            }
            if entry.exempt && !exempt {
                entry.grace_until = now.saturating_add(settings.grace);
            }
            if let Some(at) = exited {
                entry.grace_until = entry.grace_until.max(at.saturating_add(settings.grace));
            }
            entry.exempt = exempt || question;
            let terminal_report = report
                .as_ref()
                .is_some_and(|r| r.status != LaneStatus::Working);
            let unpresented_nudge_error = phase == Some(AgentRunPhase::Error)
                && store
                    .restore_record(target.root_run_id)?
                    .is_some_and(|record| {
                        serde_json::from_str::<crate::restore::RunRestoreDescriptor>(
                            &record.config_json,
                        )
                        .is_ok_and(|descriptor| {
                            descriptor
                                .spawn_credits
                                .lane_presentations
                                .iter()
                                .any(|p| !p.nudge_episode_ids.is_empty())
                        })
                    });
            let classification = if unpresented_nudge_error {
                Some(Class::RestoreUnavailable)
            } else if question {
                Some(Class::WaitingQuestion)
            } else if exempt || terminal_report || now < entry.grace_until {
                None
            } else {
                match phase {
                    Some(AgentRunPhase::Done) => Some(Class::SilentDone),
                    Some(AgentRunPhase::Error) | None => Some(Class::RestoreUnavailable),
                    Some(AgentRunPhase::Stopped) => None,
                    _ if now.saturating_sub(entry.progress_at) >= settings.stalled => {
                        Some(Class::WorkStalled)
                    }
                    _ if lane.policy != LanePolicy::Summary
                        && now.saturating_sub(
                            report
                                .as_ref()
                                .map_or(lane.created_at_ns, |r| r.created_at_ns),
                        ) >= settings.no_report =>
                    {
                        Some(Class::NoReport)
                    }
                    _ => None,
                }
            };
            let Some(classification) = classification else {
                // Resolving progress can make this lane exempt or terminal. Still
                // publish the durable revision change so the host removes stale
                // attention without waiting for focus or an unrelated report.
                if let Some((id, _)) = &entry.episode
                    && let Some(episode) = store.handle.lane_stall_episode(id.clone())?
                {
                    self.publish_stall_change(&thread, target.root_run_id, entry, episode);
                }
                continue;
            };
            let observation = ObserveLaneStall {
                lane_id: lane.lane_id,
                management_revision: lane.management_revision,
                owner,
                progress_anchor: anchor,
                classification,
                at_ns: now,
            };
            let episode = store.handle.observe_lane_stall(observation.clone())?;
            let episode = store.handle.reclassify_lane_stall(
                observation.clone(),
                episode.episode_id,
                episode.revision,
            )?;
            let episode = if matches!(
                classification,
                Class::NoReport | Class::WorkStalled | Class::SilentDone
            ) {
                store.handle.request_lane_nudge(
                    observation,
                    episode.episode_id,
                    episode.revision,
                )?
            } else {
                episode
            };
            self.publish_stall_change(&thread, target.root_run_id, entry, episode);
        }
        monitor.lanes.retain(|lane, _| observed.contains(lane));
        monitor.execution.retain(|run, _| owned_runs.contains(run));
        Ok(())
    }
    fn publish_stall_change(
        &self,
        thread: &str,
        run: RunId,
        entry: &mut Observation,
        episode: event_bus::lane::LaneStallEpisode,
    ) {
        let version = (episode.episode_id, episode.revision);
        if entry.episode.as_ref() != Some(&version) {
            // A re-query hint, not another source of attention state. Stable
            // observations do not emit a heartbeat or reorder the ledger.
            self.shared
                .bus
                .emit(event_bus::Event::new(event_bus::DiagnosticEvent {
                    source: "lane_monitor".into(),
                    severity: event_bus::DiagnosticSeverity::Info,
                    code: event_bus::event::diagnostic_codes::LANE_STALL_CHANGED.into(),
                    detail: format!("episode={} revision={}", version.0, version.1),
                    run_id: Some(run.to_string()),
                    thread_id: Some(thread.to_owned()),
                    call_id: None,
                }));
            entry.episode = Some(version);
        }
    }
    pub(crate) fn fail_lane_nudges(
        &self,
        target: &LaneDeliveryTarget,
        claim: &LaneDeliveryClaim,
        at_ns: i64,
    ) {
        let Some(permit) = target.authority.ownership.as_ref() else {
            return;
        };
        let Ok(_owner) = permit.mutation_guard() else {
            return;
        };
        let Ok(store) = self.delivery_store() else {
            return;
        };
        for notice in &claim.notifications {
            let LaneNotificationKind::Nudge { episode_id } = &notice.kind else {
                continue;
            };
            let Ok(Some(episode)) = store.handle.lane_stall_episode(episode_id.clone()) else {
                continue;
            };
            if episode.resolved_at_ns.is_some()
                || !matches!(
                    episode.nudge_state,
                    LaneNudgeState::Pending | LaneNudgeState::Failed
                )
            {
                continue;
            }
            let observation = ObserveLaneStall {
                lane_id: episode.lane_id.clone(),
                management_revision: episode.management_revision,
                owner: target.owner(),
                progress_anchor: episode.progress_anchor.clone(),
                classification: Class::RestoreUnavailable,
                at_ns,
            };
            let result = (|| -> Result<(), LaneError> {
                let Some(lane) = store.handle.lane(&episode.lane_id)? else {
                    return Ok(());
                };
                if lane.policy == LanePolicy::Detached
                    || lane.management_revision != episode.management_revision
                    || lane.root_run_id.as_deref() != Some(&target.root_run_id.to_string())
                {
                    return Ok(());
                }
                let failed = if episode.nudge_state == LaneNudgeState::Pending {
                    store.handle.reclassify_lane_stall(
                        observation.clone(),
                        episode.episode_id.clone(),
                        episode.revision,
                    )?
                } else {
                    episode
                };
                // The final write must compare episode, latest report AND progress
                // in one transaction. An earlier obsolete check is not a fence.
                #[cfg(test)]
                if let Some(hook) = BEFORE_FAILURE_COMMIT.with(|h| h.borrow_mut().take()) {
                    hook();
                }
                if let Some(appended) = store.handle.append_lane_monitor_failure(
                    storage::lane_management::LaneMonitorFailure {
                        observation,
                        episode_id: failed.episode_id.clone(),
                        expected_episode_revision: failed.revision,
                        expected_report_seq: lane.latest_report_seq,
                        report: event_bus::LaneReportInput {
                            status: LaneStatus::Failed,
                            summary: "Runtime recovery failed; inspect the run diagnostic.".into(),
                            artifact_refs: vec![],
                            artifact_revision: None,
                            failure_episode: Some(failed.episode_id),
                            needs_human: false,
                            question_refs: vec![],
                            reason_code: Some("restore_failed".into()),
                            producer: event_bus::LaneReportProducer::Harness,
                        },
                    },
                )? {
                    self.record_lane_report_progress(&appended.report)?;
                    self.shared.bus.emit(event_bus::Event::new(
                        appended.lane.report_appended_event(
                            &appended.report,
                            Some(target.root_run_id.to_string()),
                        ),
                    ));
                    if let Some(lead) = appended.lane.lead_thread_id {
                        self.schedule_lane_deliveries(&lead);
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                self.lane_delivery_diagnostic(
                    Some(target.root_run_id),
                    "lane_nudge_failure_record_failed",
                    &error.to_string(),
                );
            }
        }
    }
    pub(crate) fn check_nudge_target(
        &self,
        target: &LaneDeliveryTarget,
        claim: &LaneDeliveryClaim,
        at_input_boundary: bool,
    ) -> Result<(), LaneDeliveryNack> {
        if !claim
            .notifications
            .iter()
            .any(|n| matches!(n.kind, LaneNotificationKind::Nudge { .. }))
        {
            return Ok(());
        }
        let permit = target
            .authority
            .ownership
            .as_ref()
            .ok_or(LaneDeliveryNack::OwnershipLost)?;
        permit
            .validate_generation()
            .map_err(|_| LaneDeliveryNack::OwnershipLost)?;
        let store = self
            .delivery_store()
            .map_err(|_| LaneDeliveryNack::StorageFailed)?;
        for notice in &claim.notifications {
            let LaneNotificationKind::Nudge { episode_id } = &notice.kind else {
                continue;
            };
            let episode = store
                .handle
                .lane_stall_episode(episode_id.clone())
                .map_err(|_| LaneDeliveryNack::StorageFailed)?
                .ok_or(LaneDeliveryNack::Stopped)?;
            // Received inputs belong to the saved prefix even after progress resolves
            // the episode. Never invalidate or append them again during ACK repair.
            let received = store
                .handle
                .lane_delivery_received_ids(claim.clone())
                .map_err(|_| LaneDeliveryNack::StorageFailed)?;
            if received.contains(&notice.notification_id) {
                continue;
            }
            let lane = store
                .handle
                .lane(&notice.lane_id)
                .map_err(|_| LaneDeliveryNack::StorageFailed)?
                .ok_or(LaneDeliveryNack::UnknownThread)?;
            let report = store
                .handle
                .lane_reports(&lane.lane_id, lane.latest_report_seq.saturating_sub(1), 1)
                .map_err(|_| LaneDeliveryNack::StorageFailed)?
                .pop();
            let phase = self.inspect_agent(target.root_run_id).ok().map(|r| r.phase);
            let failed = phase == Some(AgentRunPhase::Error)
                || (phase.is_none()
                    && store
                        .restore_record(target.root_run_id)
                        .ok()
                        .flatten()
                        .is_none_or(|r| r.terminal_phase != "Done"));
            // Obsolete means progress won the race, not that restore failed.
            // In particular, never overwrite a successful Ready report with a
            // synthetic harness failure for an unreceived old nudge.
            if episode.resolved_at_ns.is_some()
                || report.is_some_and(|r| r.status != LaneStatus::Working)
            {
                return Err(LaneDeliveryNack::Stopped);
            }
            if failed {
                if episode.nudge_state == LaneNudgeState::Pending {
                    let observation = ObserveLaneStall {
                        lane_id: lane.lane_id,
                        management_revision: lane.management_revision,
                        owner: target.owner(),
                        progress_anchor: episode.progress_anchor.clone(),
                        classification: Class::RestoreUnavailable,
                        at_ns: self
                            .lane_monitor_host()
                            .ok_or(LaneDeliveryNack::UnknownThread)?
                            .now_ns(),
                    };
                    store
                        .handle
                        .reclassify_lane_stall(observation, episode.episode_id, episode.revision)
                        .map_err(|_| LaneDeliveryNack::StorageFailed)?;
                }
                return Err(LaneDeliveryNack::RestoreFailed);
            }
            let (busy, healthy_child_wait) = {
                let mut monitor = self
                    .shared
                    .lanes
                    .delivery
                    .monitor
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                monitor.drain(self.lane_monitor_host().map_or(0, |host| host.now_ns()));
                (
                    monitor
                        .execution
                        .get(&target.root_run_id)
                        .is_some_and(Execution::busy),
                    self.lane_healthy_child_wait(target.root_run_id, phase, &monitor),
                )
            };
            if (!at_input_boundary && busy)
                || healthy_child_wait
                || self.has_unanswered_questions(target.root_run_id)
                || self.lane_compaction_busy(target.root_run_id)
                || self
                    .thread_goal(&lane.thread_id)
                    .is_some_and(|g| g.checks_paused)
            {
                return Err(LaneDeliveryNack::Stopped);
            }
        }
        Ok(())
    }
}
