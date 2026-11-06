//! PR producer only: reserve -> bounded logs -> revalidate -> ready/shared outbox.
//! Delivery, durable Stop/guard adoption and receipts belong to the common driver.
use super::policy::eligible_facts;
use super::{Inner, PullRequestError, PullRequestService, Target};
use crate::ownership::OwnerPermit;
use crate::pull_requests::LogRequest;
use std::sync::Arc;
use std::time::Duration;
use storage::pull_request_auto_fix::*;
use storage::pull_requests::{CheckConclusion, CheckStatus, PullRequestFacts, PullRequestSnapshot};
use storage::{StorageError, StorageHandle};
use tokio::time::Instant;

#[cfg(test)]
mod tests;

type Result<T> = std::result::Result<T, PullRequestError>;

pub(super) async fn observe_resumes(
    inner: Arc<Inner>,
    mut resumes: tokio::sync::watch::Receiver<u64>,
) {
    let mut shutdown = inner.shutdown.subscribe();
    if *shutdown.borrow() {
        return;
    }
    loop {
        tokio::select! {
            result = resumes.changed() => {
                if result.is_err() { break; }
                // Full durable-link rescan, not a per-GUI delta or authority token.
                inner.wake.notify_one();
            }
            _ = shutdown.changed() => break,
        }
    }
}

fn current(storage: &StorageHandle, target: &Target) -> Result<PullRequestSnapshot> {
    let thread = storage.pull_request_thread_state(&target.thread_id)?;
    let link = thread.active_link.ok_or(PullRequestError::NotLinked)?;
    if link.link_revision != target.link_revision || link.key != target.key {
        return Err(PullRequestError::StaleRevision);
    }
    storage
        .pull_request_snapshot(&target.key)?
        .ok_or(PullRequestError::Ineligible)
}
fn allowed(storage: &StorageHandle, target: &Target) -> Result<bool> {
    Ok(!storage
        .lane_delivery_destination(target.thread_id.clone())?
        .is_some_and(|d| d.stopped)
        && !storage.pr_wake_guard(target.key.clone())?.suppressed)
}
fn valid_notice(
    inner: &Inner,
    target: &Target,
    notice: &PrNotice,
) -> Result<Option<(PullRequestFacts, Option<i64>)>> {
    let config = inner.config();
    if !config.enabled {
        return Ok(None);
    }
    let snapshot = match current(&inner.storage, target) {
        Ok(snapshot) => snapshot,
        Err(
            PullRequestError::NotLinked
            | PullRequestError::StaleRevision
            | PullRequestError::Deleted,
        ) => return Ok(None),
        Err(error) => return Err(error),
    };
    let Some(facts) = snapshot.facts else {
        return Ok(None);
    };
    let auto = notice.mode == PrNoticeMode::Auto;
    if facts.head_sha != notice.head_sha || !eligible_facts(&facts, auto) {
        return Ok(None);
    }
    if auto {
        let policy = inner.storage.pr_auto_fix_state(target.key.clone())?;
        if !config.auto_fix_enabled
            || !policy.enabled
            || policy.policy_revision != notice.policy_revision
            || policy.budget_epoch != notice.budget_epoch
            || policy.destination_thread_id.as_deref() != Some(&target.thread_id)
            || policy.link_revision != Some(target.link_revision)
            || policy.used >= PR_AUTO_FIX_LIMIT
            || policy
                .cooldown_until_ns
                .is_some_and(|t| inner.clock.now_ns() < t)
        {
            return Ok(None);
        }
    }
    Ok(Some((facts, snapshot.fetched_at_ns)))
}

impl PullRequestService {
    /// Read the linked PR's durable policy, budget and cooldown without granting authority.
    ///
    /// # Errors
    /// No active link, or a storage error.
    pub async fn auto_fix_state(&self, thread: &str) -> Result<PrAutoFixState> {
        let storage = self.inner.storage.clone();
        let thread = thread.to_owned();
        self.inner
            .blocking(move || -> Result<_> {
                let link = storage
                    .pull_request_thread_state(&thread)?
                    .active_link
                    .ok_or(PullRequestError::NotLinked)?;
                Ok(storage.pr_auto_fix_state(link.key)?)
            })
            .await?
    }

    /// Opt in/out without changing another thread's confirmed destination.
    /// Link and policy revisions are independent CAS values.
    ///
    /// # Errors
    /// Stale revisions, missing ownership/link, or destination confirmation is required.
    pub async fn set_auto_fix(
        &self,
        thread: &str,
        link_revision: u64,
        policy_revision: u64,
        enabled: bool,
        permit: &OwnerPermit,
    ) -> Result<PrAutoFixState> {
        self.policy_change(
            thread,
            link_revision,
            policy_revision,
            None,
            PrPolicyAction::SetEnabled(enabled),
            permit,
        )
        .await
    }
    /// Explicitly reset the budget and re-enable. Does not shorten cooldown or resume Stop.
    ///
    /// # Errors
    /// As set_auto_fix.
    pub async fn reset_auto_fix_budget(
        &self,
        thread: &str,
        link_revision: u64,
        policy_revision: u64,
        permit: &OwnerPermit,
    ) -> Result<PrAutoFixState> {
        self.policy_change(
            thread,
            link_revision,
            policy_revision,
            None,
            PrPolicyAction::ResetBudget,
            permit,
        )
        .await
    }
    /// Move the destination after the UI confirms the exact previous destination.
    ///
    /// # Errors
    /// As set_auto_fix. A changed previous destination is never silently overwritten.
    pub async fn change_auto_fix_destination(
        &self,
        thread: &str,
        link_revision: u64,
        policy_revision: u64,
        confirmed_previous: Option<String>,
        permit: &OwnerPermit,
    ) -> Result<PrAutoFixState> {
        self.policy_change(
            thread,
            link_revision,
            policy_revision,
            Some(confirmed_previous),
            PrPolicyAction::SetEnabled(true),
            permit,
        )
        .await
    }
    async fn policy_change(
        &self,
        thread: &str,
        revision: u64,
        expected: u64,
        confirmed: Option<Option<String>>,
        action: PrPolicyAction,
        permit: &OwnerPermit,
    ) -> Result<PrAutoFixState> {
        let inner = &self.inner;
        inner.enabled()?;
        inner.check_permit(thread, permit).await?;
        let storage = inner.storage.clone();
        let thread = thread.to_owned();
        let owned_permit = permit.clone();
        let (result, target) = inner
            .guarded(permit, move || -> Result<_> {
                let link = storage
                    .pull_request_thread_state(&thread)?
                    .active_link
                    .ok_or(PullRequestError::NotLinked)?;
                if link.link_revision != revision {
                    return Err(PullRequestError::StaleRevision);
                }
                let target = Target {
                    thread_id: thread.clone(),
                    key: link.key.clone(),
                    link_id: link.link_id,
                    link_revision: link.link_revision,
                    permit: owned_permit,
                };
                let previous = storage.pr_auto_fix_state(link.key.clone())?;
                let destination = match confirmed {
                    Some(destination) => destination,
                    None => {
                        if previous
                            .destination_thread_id
                            .as_ref()
                            .is_some_and(|d| d != &thread)
                            && !matches!(action, PrPolicyAction::SetEnabled(false))
                        {
                            return Err(StorageError::PrAutoFix(
                                PrAutoFixRejection::DestinationConfirmationRequired,
                            )
                            .into());
                        }
                        previous.destination_thread_id
                    }
                };
                let result = storage.update_pr_auto_fix_policy(PrPolicyUpdate {
                    key: link.key,
                    expected_policy_revision: expected,
                    expected_destination_thread_id: destination,
                    destination_thread_id: thread,
                    link_revision: revision,
                    action,
                })?;
                Ok((result, target))
            })
            .await??;
        // An explicit policy action completes its evaluation even when no GUI delta exists.
        let _ = inner.coordinate(&[target]).await;
        inner.wake.notify_one();
        Ok(result)
    }
    /// Reserve a manual notice now, including while other checks are pending. The returned
    /// ID is durable; preparation is driven independently of GUI diff events. No user authority
    /// or Resume is conferred by this API.
    ///
    /// # Errors
    /// Not owner, stale link/head, no known failure, stopped destination, or SHA conflict.
    pub async fn request_manual_notice(
        &self,
        thread: &str,
        revision: u64,
        expected_head: &str,
        permit: &OwnerPermit,
    ) -> Result<PrNotice> {
        let inner = &self.inner;
        inner.enabled()?;
        inner.check_permit(thread, permit).await?;
        let state = inner.thread_state(thread).await?;
        let link = state.active_link.ok_or(PullRequestError::NotLinked)?;
        if link.link_revision != revision {
            return Err(PullRequestError::StaleRevision);
        }
        let target = Target {
            thread_id: thread.to_owned(),
            key: link.key,
            link_id: link.link_id,
            link_revision: revision,
            permit: permit.clone(),
        };
        let head = expected_head.to_owned();
        let owner = Arc::clone(inner);
        let t = target.clone();
        let notice = inner
            .guarded(permit, move || {
                owner.reserve_notice(&t, PrNoticeMode::Manual, Some(&head))
            })
            .await??;
        // An explicit manual retry is an operator decision, not an automatic retry loop.
        inner.lock().attempted.remove(&(
            notice.notice_id.clone(),
            notice.policy_revision,
            target.permit.lease.generation,
            notice.reservation_revision,
        ));
        inner.spawn_preparation(target, notice.clone());
        inner.wake.notify_one();
        Ok(notice)
    }
}

impl Inner {
    fn reserve_notice(
        &self,
        target: &Target,
        mode: PrNoticeMode,
        expected: Option<&str>,
    ) -> Result<PrNotice> {
        let config = self.enabled()?;
        let snapshot = current(&self.storage, target)?;
        let facts = snapshot.facts.ok_or(PullRequestError::Ineligible)?;
        let auto = mode == PrNoticeMode::Auto;
        if expected.is_some_and(|sha| sha != facts.head_sha)
            || !eligible_facts(&facts, auto)
            || !allowed(&self.storage, target)?
        {
            return Err(PullRequestError::Ineligible);
        }
        if auto
            && !self.lock().attribution.agent(
                &target.thread_id,
                target.permit.lease.generation,
                target.link_revision,
                snapshot.success_seq,
                &facts.head_sha,
            )
        {
            return Err(PullRequestError::Ineligible);
        }
        let policy = self.storage.pr_auto_fix_state(target.key.clone())?;
        Ok(self.storage.reserve_pr_notice(PrNoticeReservation {
            key: target.key.clone(),
            head_sha: facts.head_sha,
            mode,
            destination_thread_id: target.thread_id.clone(),
            link_revision: target.link_revision,
            expected_policy_revision: policy.policy_revision,
            success_seq: snapshot.success_seq,
            auto_fix_enabled: config.auto_fix_enabled,
            now_ns: self.clock.now_ns(),
        })?)
    }
    /// Run on every scheduler round: startup, owner changes, Resume, new seq, policy/manual
    /// requests, and cooldown all reach this path, even when the GUI emitted no transition.
    pub(super) async fn coordinate(self: &Arc<Self>, targets: &[Target]) -> Option<Instant> {
        let mut next = None;
        for target in targets {
            let (storage, key) = (self.storage.clone(), target.key.clone());
            if let Ok(Ok(Some(snapshot))) = self
                .blocking(move || storage.pull_request_snapshot(&key))
                .await
                && let Some(facts) = snapshot.facts
            {
                let mut state = self.lock();
                if !state.flights.contains_key(&target.key) {
                    state.attribution.seed(
                        &target.thread_id,
                        target.permit.lease.generation,
                        target.link_revision,
                        snapshot.success_seq,
                        &facts.head_sha,
                    );
                }
            }
            let inner = Arc::clone(self);
            let t = target.clone();
            let scanned = self
                .guarded(&target.permit, move || -> Result<_> {
                    let policy = inner.storage.pr_auto_fix_state(t.key.clone())?;
                    let mut cursor = String::new();
                    let mut reserved = Vec::new();
                    loop {
                        let page = inner.storage.pr_notices(t.key.clone(), cursor.clone())?;
                        if page.is_empty() {
                            break;
                        }
                        cursor = page.last().unwrap().notice_id.clone();
                        for notice in page {
                            if notice.destination_thread_id.as_deref() != Some(&t.thread_id)
                                || !matches!(
                                    notice.phase,
                                    PrNoticePhase::Reserved | PrNoticePhase::Ready
                                )
                            {
                                continue;
                            }
                            if notice.link_revision != t.link_revision
                                || valid_notice(&inner, &t, &notice)?.is_none()
                            {
                                inner.storage.cancel_pr_notice_if_current(notice)?;
                            } else if notice.phase == PrNoticePhase::Reserved
                                && allowed(&inner.storage, &t)?
                            {
                                reserved.push(notice);
                            }
                        }
                    }
                    if policy.enabled
                        && inner.config().auto_fix_enabled
                        && policy.destination_thread_id.as_deref() == Some(&t.thread_id)
                        && policy.link_revision == Some(t.link_revision)
                        && let Ok(notice) = inner.reserve_notice(&t, PrNoticeMode::Auto, None)
                    {
                        reserved.push(notice);
                    }
                    Ok((policy, reserved))
                })
                .await;
            if let Ok(Ok((policy, reserved))) = scanned {
                if policy.enabled
                    && self.config().auto_fix_enabled
                    && let Some(until) = policy.cooldown_until_ns
                    && until > self.clock.now_ns()
                {
                    let wait = Duration::from_nanos(
                        until.saturating_sub(self.clock.now_ns()).max(0) as u64,
                    );
                    let due = Instant::now() + wait;
                    next = Some(next.map_or(due, |old: Instant| old.min(due)));
                }
                for notice in reserved {
                    self.spawn_preparation(target.clone(), notice);
                }
            }
        }
        next
    }
    fn spawn_preparation(self: &Arc<Self>, target: Target, notice: PrNotice) {
        if notice.phase != PrNoticePhase::Reserved {
            return;
        }
        {
            let mut state = self.lock();
            if state.preparing.contains(&notice.notice_id)
                || state.attempted.contains(&(
                    notice.notice_id.clone(),
                    notice.policy_revision,
                    target.permit.lease.generation,
                    notice.reservation_revision,
                ))
            {
                return;
            }
            state.preparing.insert(notice.notice_id.clone());
        }
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            let mut shutdown = inner.shutdown.subscribe();
            tokio::select! {
                result = inner.prepare_notice(&target, &notice) => {
                    if let Err(error) = result {
                        tracing::warn!(%error, notice_id = %notice.notice_id, "PR notice preparation stopped");
                    }
                }
                _ = shutdown.changed() => {}
            }
            inner.lock().preparing.remove(&notice.notice_id);
            #[cfg(test)]
            inner.run_stage_hook(super::Stage::PrPreparationFinished);
            inner.wake.notify_one();
        });
    }
    async fn prepare_notice(self: &Arc<Self>, target: &Target, notice: &PrNotice) -> Result<()> {
        self.check_permit(&target.thread_id, &target.permit).await?;
        let inner = Arc::clone(self);
        let (t, n) = (target.clone(), notice.clone());
        let facts = self
            .guarded(&target.permit, move || valid_notice(&inner, &t, &n))
            .await??;
        let Some((facts, observed_at_ns)) = facts else {
            return Ok(());
        };
        let (storage, t) = (self.storage.clone(), target.clone());
        if !self.blocking(move || allowed(&storage, &t)).await?? {
            return Ok(());
        }
        // No async wait between the final ownership/global check and starting the source.
        if !self.may_launch([&target.permit])
            || (notice.mode == PrNoticeMode::Auto && !self.config().auto_fix_enabled)
        {
            return Ok(());
        }
        self.lock().attempted.insert((
            notice.notice_id.clone(),
            notice.policy_revision,
            target.permit.lease.generation,
            notice.reservation_revision,
        ));
        let failed_checks: Vec<_> = facts
            .checks
            .into_iter()
            .filter(|check| {
                check.status == CheckStatus::Completed
                    && matches!(
                        check.conclusion,
                        Some(
                            CheckConclusion::Failure
                                | CheckConclusion::TimedOut
                                | CheckConclusion::StartupFailure
                        )
                    )
            })
            .collect();
        let names = failed_checks
            .iter()
            .map(|c| c.name.clone())
            .collect::<Vec<_>>();
        let request = LogRequest::new(notice.key.clone(), &notice.head_sha, failed_checks);
        let bundle = self.logs.fetch_failed_logs(&request).await;
        let payload = crate::lane_delivery::prepare_pr_notice_payload(
            notice,
            &names,
            &observed_at_ns.unwrap_or_default().to_string(),
            Some(&bundle),
        );
        let inner = Arc::clone(self);
        let (t, n) = (target.clone(), notice.clone());
        self.guarded(&target.permit, move || -> Result<()> {
            if valid_notice(&inner, &t, &n)?.is_some() {
                if inner
                    .storage
                    .ready_pr_notice_if_current(n, payload)?
                    .is_some()
                {
                    inner.delivery.schedule(&t.thread_id);
                }
            } else {
                inner.storage.cancel_pr_notice_if_current(n)?;
            }
            Ok(())
        })
        .await??;
        Ok(())
    }
}
