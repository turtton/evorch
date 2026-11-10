//! PR source adapter. Only already-processed bodies cross this boundary.
use super::*;
use crate::pull_requests::log_source::{CiLogBundle, render_ci_log_data};
use storage::lane_delivery::LANE_DELIVERY_PAGE_BYTES;
use storage::pull_request_auto_fix::{PrDeliveryClaim, PrGuardSignal, PrNotice, PrNoticePhase};

fn header(notice: &PrNotice) -> String {
    format!(
        "[harness-notice kind=pr_checks_failed pr=#{} head={}]\nrepo: {}/{}\n",
        notice.key.number, notice.head_sha, notice.key.host, notice.key.repository
    )
}
/// D's preparation boundary: pass this processed body to StorageHandle::ready_pr_notice,
/// then schedule_lane_deliveries(destination). Logs come only from the bounded log source.
/// The wrapper is added from the durable identity at flush, not parsed from message text.
/// Facts are redacted before bounding; logs use B's escaping and optional rendering.
#[must_use]
pub fn prepare_pr_notice_payload(
    notice: &PrNotice,
    checks: &[String],
    observed_at: &str,
    logs: Option<&CiLogBundle>,
) -> String {
    let budget = LANE_DELIVERY_PAGE_BYTES.saturating_sub(header(notice).len());
    let facts = format!(
        "checks: {}\nobserved_at: {}\n",
        serde_json::to_string(checks).expect("string list"),
        serde_json::to_string(observed_at).expect("string")
    );
    let mut body = secret_guard::SecretRedactor::from_env().redact(&facts).text;
    if body.len() > budget {
        let mut end = budget;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        body.truncate(end);
    }
    if let Some(logs) = logs.filter(|logs| logs.head_sha == notice.head_sha)
        && let Some(rendered) = render_ci_log_data(logs, budget.saturating_sub(body.len() + 1))
        && body.len() + 1 + rendered.len() <= budget
    {
        body.push('\n');
        body.push_str(&rendered);
    }
    body
}
/// What a person is shown of a stored notice payload: the facts and the log excerpts, read
/// back from the structure [`prepare_pr_notice_payload`] wrote. The agent-facing wrapper
/// (`[harness-notice …]`, `<ci-log-data>`, the data-not-instructions marker) is never part of
/// it. A payload that does not have the expected shape gives what could be read, never an
/// error: the card then shows less, not the raw text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrNoticeView {
    /// The names of the failed checks the notice lists.
    pub checks: Vec<String>,
    /// When the failure was observed, as written (RFC 3339).
    pub observed_at: Option<String>,
    /// One excerpt per run the log source could read.
    pub logs: Vec<PrNoticeLog>,
    /// Whether anything was left out of the logs.
    pub logs_truncated: bool,
}

/// The excerpt of one run's failed-step log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrNoticeLog {
    /// The failed checks this log answers.
    pub checks: Vec<String>,
    pub excerpt: String,
}

#[must_use]
pub fn view_pr_notice_payload(payload: &str) -> PrNoticeView {
    use crate::pull_requests::log_source::{CI_LOG_DATA_CLOSE, CI_LOG_DATA_OPEN};
    let mut view = PrNoticeView::default();
    let (facts, logs) = match payload.find(CI_LOG_DATA_OPEN) {
        Some(at) => (
            &payload[..at],
            Some(&payload[at + CI_LOG_DATA_OPEN.len()..]),
        ),
        None => (payload, None),
    };
    for line in facts.lines() {
        if let Some(rest) = line.strip_prefix("checks: ") {
            view.checks = serde_json::from_str(rest).unwrap_or_default();
        } else if let Some(rest) = line.strip_prefix("observed_at: ") {
            view.observed_at = serde_json::from_str(rest).ok();
        }
    }
    if let Some(logs) = logs {
        let json = logs.split(CI_LOG_DATA_CLOSE).next().unwrap_or("").trim();
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(json) {
            view.logs_truncated = value["truncated"].as_bool().unwrap_or(false);
            for run in value["runs"].as_array().into_iter().flatten() {
                let excerpt = run["excerpt"].as_str().unwrap_or_default();
                if excerpt.is_empty() {
                    continue;
                }
                view.logs.push(PrNoticeLog {
                    checks: run["checks"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|name| name.as_str().map(str::to_owned))
                        .collect(),
                    excerpt: excerpt.to_owned(),
                });
            }
        }
    }
    view
}
impl AgentRuntime {
    /// Tells displays that what limits the thread's pull request notices changed (a durable
    /// Stop, an explicit Resume, a reset wake guard). A thread with no pull request has
    /// nothing to refresh; the event carries identities only.
    pub(crate) fn announce_pr_delivery_state(&self, thread: &str) {
        use event_bus::{Event, PullRequestAutoFixChange, PullRequestEvent, PullRequestRef};
        let Some(store) = self.shared.run_store.get() else {
            return;
        };
        let Ok(state) = store.handle.pull_request_thread_state(thread) else {
            return;
        };
        let Some(link) = state.active_link else {
            return;
        };
        self.shared
            .bus
            .emit(Event::new(PullRequestEvent::AutoFixChanged {
                thread_id: thread.to_owned(),
                link_revision: link.link_revision,
                transition_seq: state.transition_seq,
                pull_request: PullRequestRef {
                    provider: link.key.provider.as_str().to_owned(),
                    host: link.key.host.clone(),
                    repository: link.key.repository.clone(),
                    number: link.key.number,
                },
                reason: PullRequestAutoFixChange::Delivery,
            }));
    }
    pub(super) fn pr_user_progress(
        &self,
        thread: &str,
        signal: &LaneGuardSignal,
    ) -> Result<(), LaneError> {
        let signal = match signal {
            LaneGuardSignal::RealUserInput => PrGuardSignal::RealUserInput,
            LaneGuardSignal::ExplicitUserResume => PrGuardSignal::ExplicitUserResume,
            _ => return Ok(()),
        };
        let store = self.delivery_store()?;
        let Some(link) = store.handle.pull_request_thread_state(thread)?.active_link else {
            return Ok(());
        };
        for attempt in 0..4 {
            let guard = store.handle.pr_wake_guard(link.key.clone())?;
            match store.handle.update_pr_wake_guard(
                link.key.clone(),
                guard.revision,
                signal.clone(),
            ) {
                Ok(_) => {
                    self.announce_pr_delivery_state(thread);
                    return Ok(());
                }
                Err(storage::StorageError::StaleRevision {
                    kind: "wake_guard", ..
                }) if attempt < 3 => continue,
                Err(error) => return Err(error.into()),
            }
        }
        unreachable!("the final CAS error is returned")
    }
}
impl crate::agent_loop::LoopState {
    pub(super) fn accept_pr_batch(
        &mut self,
        runtime: &AgentRuntime,
        claim: &PrDeliveryClaim,
    ) -> Result<usize, LaneDeliveryNack> {
        let thread = self
            .thread_id
            .as_deref()
            .ok_or(LaneDeliveryNack::UnknownThread)?;
        let host = runtime
            .lane_monitor_host()
            .ok_or(LaneDeliveryNack::UnknownThread)?;
        let current = host.resolve(thread)?;
        if current.owner() != claim.owner || current.root_run_id != self.task.run_id {
            return Err(LaneDeliveryNack::OwnershipLost);
        }
        let gate = runtime.stop_restore_gate(self.task.run_id);
        let _stop = gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        runtime.check_delivery_target(thread, &current)?;
        let _owner = current
            .authority
            .ownership
            .as_ref()
            .map(|p| p.mutation_guard())
            .transpose()
            .map_err(|_| LaneDeliveryNack::OwnershipLost)?;
        let store = runtime
            .delivery_store()
            .map_err(|_| LaneDeliveryNack::StorageFailed)?;
        let received = store
            .handle
            .pr_delivery_receipt_ids(claim.clone())
            .map_err(|_| LaneDeliveryNack::StorageFailed)?;
        let mut bodies = Vec::new();
        let mut adopted = Vec::new();
        for reference in &claim.notifications {
            if received.contains(&reference.notification_id) {
                continue;
            }
            let notice = store
                .handle
                .pr_notice(reference.notification_id.clone())
                .map_err(|_| LaneDeliveryNack::PayloadUnavailable)?
                .ok_or(LaneDeliveryNack::PayloadUnavailable)?;
            if notice.phase != PrNoticePhase::Ready
                || notice.destination_thread_id.as_deref() != Some(thread)
            {
                return Err(LaneDeliveryNack::PayloadUnavailable);
            }
            let mut body = header(&notice);
            body.push_str(
                notice
                    .payload
                    .as_deref()
                    .ok_or(LaneDeliveryNack::PayloadUnavailable)?,
            );
            bodies.push((body, notice.notice_id.clone()));
            adopted.push(notice);
        }
        if bodies.is_empty() {
            return Ok(0);
        }
        if bodies.iter().map(|(body, _)| body.len()).sum::<usize>() > LANE_DELIVERY_PAGE_BYTES {
            return Err(LaneDeliveryNack::PayloadUnavailable);
        }
        crate::restore::persist_checkpoint(self).map_err(|_| LaneDeliveryNack::StorageFailed)?;
        let mut candidate = self.context.clone();
        let start = candidate.messages.len();
        for (i, (body, id)) in bodies.iter().enumerate() {
            candidate.push_user(body);
            candidate
                .provenance
                .harness_notice(&candidate.messages, start + i, id);
        }
        candidate.harness_notice_pending = true;
        let previous_turn = self.turn.clone();
        self.begin_internal_input();
        if crate::restore::persist_pr_delivery(self, &candidate, claim, host.now_ns()).is_err() {
            self.turn = previous_turn;
            return Err(LaneDeliveryNack::StorageFailed);
        }
        self.context = candidate;
        self.publish_message_count();
        self.announce_pr_adoption(&store.handle, &adopted);
        Ok(bodies.len())
    }
}

impl crate::agent_loop::LoopState {
    /// After the snapshot and receipt committed: tells displays which durable notices are now
    /// part of the conversation (once per notice ID) and that the PR budget moved. The events
    /// carry identities only; the stored notice and policy are what a display reads.
    fn announce_pr_adoption(&self, handle: &storage::StorageHandle, adopted: &[PrNotice]) {
        let Some(thread) = self.thread_id.as_deref() else {
            return;
        };
        use event_bus::{
            Event, LifecycleEvent, PullRequestAutoFixChange, PullRequestEvent, PullRequestRef,
        };
        use storage::pull_request_auto_fix::PrNoticeMode;
        let run_id = self.task.run_id.to_string();
        let bus = &self.shared.bus;
        let transition_seq = handle
            .pull_request_thread_state(thread)
            .map_or(0, |state| state.transition_seq);
        for notice in adopted {
            bus.emit(Event::new(LifecycleEvent::HarnessNoticeDelivered {
                run_id: run_id.clone(),
                notification_id: notice.notice_id.clone(),
            }));
            let automatic = notice.mode == PrNoticeMode::Auto;
            bus.emit(Event::new(PullRequestEvent::AutoFixChanged {
                thread_id: thread.to_owned(),
                link_revision: notice.link_revision,
                transition_seq,
                pull_request: PullRequestRef {
                    provider: notice.key.provider.as_str().to_owned(),
                    host: notice.key.host.clone(),
                    repository: notice.key.repository.clone(),
                    number: notice.key.number,
                },
                reason: PullRequestAutoFixChange::Adopted { automatic },
            }));
        }
    }
}
