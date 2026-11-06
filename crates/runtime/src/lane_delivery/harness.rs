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
impl AgentRuntime {
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
                Ok(_) => return Ok(()),
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
            bodies.push((body, notice.notice_id));
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
        Ok(bodies.len())
    }
}
