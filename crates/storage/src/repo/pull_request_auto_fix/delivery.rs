//! PR adapter to the shared outbox engine. No lane progress/spawn APIs here.
use super::*;
use crate::repo::lane_delivery::OutboxNotice;
impl OutboxNotice for PrNoticeRef {
    const LIVE_NOTICE: &'static str = "n.source_kind='pull_request_checks_failed' AND n.destination_thread_id NOT IN (SELECT thread_id FROM thread_tombstones)";
    const RECEIPT_PREFIX: &'static str = "harness-notice:";
    const PROVENANCE_KIND: &'static str = "harness_notice";
    const ELIGIBLE_QUERY: &'static str="WITH eligible AS (
 SELECT n.payload,n.notification_id,ROW_NUMBER() OVER (PARTITION BY n.pr_key ORDER BY n.notification_id) AS rank
 FROM lane_notifications n JOIN pr_notices p ON p.notice_id=n.notification_id JOIN pr_auto_fix_state s ON s.pr_key=p.pr_key
 WHERE n.source_kind='pull_request_checks_failed' AND n.destination_thread_id=?1 AND n.state='pending'
 AND n.destination_thread_id NOT IN (SELECT thread_id FROM thread_tombstones)
 AND (n.receipt_prompt_id IS NOT NULL OR (p.phase='ready' AND (p.mode='manual' OR (s.enabled=1 AND p.policy_revision=s.policy_revision AND p.budget_epoch=s.budget_epoch))))
 AND (?2 OR COALESCE(json_extract(s.wake_guard_json,'$.suppressed'),0)=0)
 ) SELECT payload FROM eligible WHERE rank=1 ORDER BY notification_id LIMIT 20";
    fn notification_id(&self) -> &str {
        &self.notification_id
    }
    fn provenance(&self) -> crate::HostInputProvenance {
        crate::HostInputProvenance::HarnessNotice {
            notification_id: self.notification_id.clone(),
            destination_thread_id: self.destination_thread_id.clone(),
        }
    }
    fn validate(
        &self,
        c: &Connection,
        claim: &PrDeliveryClaim,
        received: bool,
    ) -> Result<(), StorageError> {
        if self.destination_thread_id != claim.destination_thread_id {
            return Err(StorageError::PrAutoFix(
                PrAutoFixRejection::DestinationMismatch,
            ));
        }
        if received {
            return Ok(());
        }
        let n = required(c, &self.notification_id)?;
        if n.phase != PrNoticePhase::Ready
            || n.destination_thread_id.as_deref() != Some(&self.destination_thread_id)
        {
            return Err(StorageError::PrAutoFix(PrAutoFixRejection::NotReady));
        }
        check_link(c, &n.key, &self.destination_thread_id, n.link_revision)?;
        if claim.completed_root_wake && guard(c, &n.key)?.suppressed {
            return Err(StorageError::PrAutoFix(PrAutoFixRejection::WakeSuppressed));
        }
        if n.mode == PrNoticeMode::Auto {
            let s = state(c, &n.key)?;
            policy_revision(s.policy_revision, n.policy_revision)?;
            if n.budget_epoch != s.budget_epoch {
                return Err(StorageError::PrAutoFix(PrAutoFixRejection::StaleBudget));
            }
        }
        Ok(())
    }
    fn adopt(
        &self,
        c: &Connection,
        claim: &PrDeliveryClaim,
        now: i64,
        global: bool,
        wakes: &mut std::collections::HashSet<String>,
    ) -> Result<(), StorageError> {
        let n = required(c, &self.notification_id)?;
        let key = key_id(&n.key)?;
        if n.mode == PrNoticeMode::Auto {
            let s = state(c, &n.key)?;
            auto_allowed(
                &s,
                &self.destination_thread_id,
                n.link_revision,
                global,
                now,
            )?;
            let until = now
                .checked_add(PR_AUTO_FIX_COOLDOWN_NS)
                .ok_or(StorageError::OutOfRange("PR cooldown"))?;
            let used = s.used + 1;
            let limit = used == PR_AUTO_FIX_LIMIT;
            c.execute("UPDATE pr_auto_fix_state SET used=?2,cooldown_until_ns=?3,enabled=CASE WHEN ?4 THEN 0 ELSE enabled END,stop_reason=CASE WHEN ?4 THEN 'limit_reached' ELSE stop_reason END,limit_reached_epoch=CASE WHEN ?4 THEN budget_epoch ELSE limit_reached_epoch END,policy_revision=policy_revision+?4 WHERE pr_key=?1",params![key,used,until,limit])?;
            if limit {
                c.execute("INSERT INTO pr_auto_fix_limit_events(pr_key,budget_epoch,notice_id,at_ns) VALUES (?1,?2,?3,?4)",params![key,s.budget_epoch,n.notice_id,now])?;
            }
        }
        c.execute(
            "UPDATE pr_notices SET phase='adopted' WHERE notice_id=?1",
            [&self.notification_id],
        )?;
        if claim.completed_root_wake && wakes.insert(key) {
            let rev = guard(c, &n.key)?.revision;
            update_guard_in_transaction(
                c,
                &n.key,
                rev,
                PrGuardSignal::WakeBatch {
                    batch_id: format!("pr-batch:{}", self.notification_id),
                },
            )?;
        }
        Ok(())
    }
}
