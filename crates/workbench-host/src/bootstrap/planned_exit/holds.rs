//! Two databases, deliberately no cross-DB transaction assumption. This owner
//! survives uncertain ACKs and is also reachable by operator PlannedExit cleanup.
use runtime::ownership::{
    CapturedThreadRuns, ExpectedCapturedRuns, OwnerHost, PlannedHold, ThreadOwner,
};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};
use storage::recovery::restart::{
    RestartMetadata, RestartMutation, RestartOwnerProgress, RestartPhase, RestartTransaction,
};

pub struct RestartHolds {
    owner: Arc<OwnerHost>,
    storage: storage::StorageConfig,
    writer: storage::StorageHandle,
    pending: Mutex<Option<Pending>>,
}
struct Pending {
    metadata: RestartMetadata,
    // Remember BEFORE mutation, including when the commit ACK is lost.
    holds: Vec<PlannedHold>,
}
impl RestartHolds {
    pub(super) fn new(
        owner: Arc<OwnerHost>,
        storage: storage::StorageConfig,
        writer: storage::StorageHandle,
    ) -> Self {
        Self {
            owner,
            storage,
            writer,
            pending: Mutex::new(None),
        }
    }
    pub fn read_transaction(
        &self,
        metadata: &RestartMetadata,
    ) -> Result<RestartTransaction, String> {
        let tx = storage::Database::open(&self.storage)
            .map_err(text)?
            .restart_transaction(&metadata.restart_id)
            .map_err(text)?
            .ok_or("restart transaction missing")?;
        if tx.metadata != *metadata {
            return Err("restart metadata changed".into());
        }
        Ok(tx)
    }
    fn mutate(
        &self,
        old: &RestartTransaction,
        mutation: RestartMutation,
        mut expected: RestartTransaction,
    ) -> Result<RestartTransaction, String> {
        expected.revision = old
            .revision
            .checked_add(1)
            .ok_or("restart revision overflow")?;
        let ack = self.writer.mutate_restart(
            old.metadata.restart_id.clone(),
            Some(old.revision),
            mutation,
        );
        // Read the SAME transaction/revision/nonce even on Err. Never reissue
        // Create/BeginCapture or pretend an unknown ACK is a rejection.
        let observed = self.read_transaction(&old.metadata)?;
        if observed != expected {
            return Err(ack
                .err()
                .map(text)
                .unwrap_or_else(|| "restart mutation readback mismatch".into()));
        }
        Ok(observed)
    }
    pub fn save_and_read_back(
        &self,
        committed: &RestartTransaction,
    ) -> Result<RestartTransaction, String> {
        if committed.phase != RestartPhase::Committed || !committed.owner_progress.is_empty() {
            return Err("owner hold requires an unmodified committed inventory".into());
        }
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending.is_some() {
            return Err("hold installation already attempted".into());
        }
        let expected = expected_runs(committed, self.owner.owner_id())?;
        let inventory = self.owner.restart_inventory().map_err(text)?;
        if expected.iter().any(|(thread, runs)| {
            !inventory.iter().any(|o| {
                o.thread_id == *thread
                    && o.lease.generation == runs.generation
                    && o.active_runs.is_subset(&runs.captured_runs)
            })
        }) {
            return Err("captured owner missing from full inventory".into());
        }
        let holds = inventory
            .iter()
            .map(|o| PlannedHold {
                transaction_id: committed.metadata.restart_id.clone(),
                capture_id: committed.metadata.capture_id.clone(),
                thread_id: o.thread_id.clone(),
                owner_id: o.lease.owner_id.clone(),
                generation: o.lease.generation,
                active_runs: o.active_runs.clone(),
                parked_runs: expected
                    .get(&o.thread_id)
                    .map(|e| {
                        e.captured_runs
                            .difference(&o.active_runs)
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default(),
                orphaned: false,
            })
            .collect::<Vec<_>>();
        *pending = Some(Pending {
            metadata: committed.metadata.clone(),
            holds: holds.clone(),
        });
        let ack = self.owner.hold_captured_for_restart(
            &committed.metadata.restart_id,
            &committed.metadata.capture_id,
            &expected,
        );
        let readback = self.owner.restart_inventory().map_err(text)?;
        if !inventory_matches(&readback, &holds) {
            return Err(ack
                .err()
                .map(text)
                .unwrap_or_else(|| "complete owner hold inventory unconfirmed".into()));
        }
        let mut tx = committed.clone();
        for hold in &holds {
            let progress = RestartOwnerProgress {
                thread_id: hold.thread_id.clone(),
                old_owner_id: hold.owner_id.clone(),
                old_generation: hold.generation,
                active_runs: hold.active_runs.clone(),
                parked_runs: hold.parked_runs.clone(),
                hold_saved: true,
                hold_read_back: true,
                worker_joined: false,
                socket_unlinked: false,
                hold_removed: false,
            };
            let mut next = tx.clone();
            next.owner_progress.push(progress.clone());
            next.owner_progress
                .sort_by(|a, b| a.thread_id.cmp(&b.thread_id));
            tx = self.mutate(&tx, RestartMutation::OwnerProgress(progress), next)?;
        }
        // Full main DB membership/progress readback, not just the last row ACK.
        if tx != self.read_transaction(&tx.metadata)?
            || !inventory_matches(&self.owner.restart_inventory().map_err(text)?, &holds)
        {
            return Err("owner inventory changed during main DB readback".into());
        }
        Ok(tx)
    }
    pub fn arm_and_read_back(
        &self,
        held: &RestartTransaction,
        nonce_hash: String,
    ) -> Result<RestartTransaction, String> {
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let pending = pending.as_ref().ok_or("no owned holds")?;
        if pending.metadata != held.metadata
            || held.phase != RestartPhase::Committed
            || held.owner_progress.len() != pending.holds.len()
            || held.owner_progress.iter().any(|p| {
                !pending.holds.iter().any(|h| {
                    p.thread_id == h.thread_id
                        && p.old_owner_id == h.owner_id
                        && p.old_generation == h.generation
                        && p.active_runs == h.active_runs
                        && p.parked_runs == h.parked_runs
                })
            })
            || held.owner_progress.iter().any(|p| {
                !p.hold_saved
                    || !p.hold_read_back
                    || p.hold_removed
                    || p.worker_joined
                    || p.socket_unlinked
            })
            || !inventory_matches(
                &self.owner.restart_inventory().map_err(text)?,
                &pending.holds,
            )
        {
            return Err("Arm requires the complete live owner hold readback".into());
        }
        let mut next = held.clone();
        next.phase = RestartPhase::Armed;
        next.ticket_nonce_hash = Some(nonce_hash.clone());
        self.mutate(held, RestartMutation::Arm { nonce_hash }, next)
    }
    /// Invalidate before releasing a hold, then generation/tx CAS each row.
    /// Failed work stays in this shared owner and is retried by host cleanup.
    pub fn cleanup(&self) -> Result<(), String> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(work) = pending.as_ref() else {
            return Ok(());
        };
        let mut tx = self.read_transaction(&work.metadata)?;
        if tx.phase != RestartPhase::Invalidated {
            let mut next = tx.clone();
            next.phase = RestartPhase::Invalidated;
            next.failure_reason = Some("planned finalization ended without exec".into());
            tx = self.mutate(
                &tx,
                RestartMutation::Invalidate {
                    reason: next.failure_reason.clone().unwrap(),
                },
                next,
            )?;
        }
        let mut failure = None;
        for hold in &work.holds {
            let result = (|| {
                self.owner.clear_planned_hold(hold).map_err(text)?;
                let row = self.owner.attach(&hold.thread_id).map_err(text)?;
                if !hold.matches_generation(&row) || row.handoff_hold.is_some() {
                    return Err("hold removal readback mismatch".into());
                }
                if let Some(index) = tx
                    .owner_progress
                    .iter()
                    .position(|p| p.thread_id == hold.thread_id)
                    && !tx.owner_progress[index].hold_removed
                {
                    let progress = &tx.owner_progress[index];
                    if progress.old_owner_id != hold.owner_id
                        || progress.old_generation != hold.generation
                        || progress.active_runs != hold.active_runs
                        || progress.parked_runs != hold.parked_runs
                    {
                        return Err("hold cleanup progress identity mismatch".into());
                    }
                    let mut next = tx.clone();
                    next.owner_progress[index].hold_removed = true;
                    let p = next.owner_progress[index].clone();
                    tx = self.mutate(&tx, RestartMutation::OwnerProgress(p), next)?;
                }
                Ok::<_, String>(())
            })();
            if let Err(error) = result {
                failure.get_or_insert(error);
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        *pending = None;
        Ok(())
    }
}
fn expected_runs(tx: &RestartTransaction, owner: &str) -> Result<ExpectedCapturedRuns, String> {
    let mut expected = ExpectedCapturedRuns::new();
    for m in &tx.expected {
        match (&m.thread_id, &m.owner_id, m.owner_generation) {
            (Some(thread), Some(id), Some(generation)) if id == owner => {
                let e = expected
                    .entry(thread.clone())
                    .or_insert_with(|| CapturedThreadRuns {
                        generation,
                        captured_runs: BTreeSet::new(),
                    });
                if e.generation != generation {
                    return Err("mixed capture owner generations".into());
                }
                e.captured_runs.insert(m.run_id.clone());
            }
            (_, None, None) => (),
            _ => return Err("capture belongs to another or incomplete owner".into()),
        }
    }
    Ok(expected)
}
fn inventory_matches(owners: &[ThreadOwner], holds: &[PlannedHold]) -> bool {
    owners.len() == holds.len()
        && owners.iter().all(|owner| {
            holds.iter().any(|hold| {
                hold.matches_generation(owner)
                    && owner.handoff_hold.as_deref() == Some(hold)
                    && owner.active_runs == hold.active_runs
                    && hold.active_runs.is_disjoint(&hold.parked_runs)
                    && owner.state == runtime::ownership::OwnerState::Quiescing
            })
        })
}
fn text(e: impl std::fmt::Display) -> String {
    e.to_string()
}
