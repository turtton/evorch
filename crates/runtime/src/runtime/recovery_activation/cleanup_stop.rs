//! Explicit Stop is distinct from local cleanup and requires live authority.
use super::*;

impl AgentRuntime {
    /// Stop only this claim's unpublished new incarnations under CURRENT owner
    /// guards. Unknown/third incarnations and any CAS conflict remain errors.
    /// The claim stays quarantined: Stop is not proof that it can be released.
    pub fn stop_quarantined_recovery(
        &self,
        handle: &RecoveryCleanupHandle,
        authority: &CurrentRecoveryAuthority,
        reason: crate::quiesce::StopReason,
    ) -> Result<(), RuntimeError> {
        if !Weak::ptr_eq(&handle.runtime, &Arc::downgrade(&self.shared)) {
            return Err(error("foreign recovery cleanup handle"));
        }
        if !matches!(
            reason,
            crate::quiesce::StopReason::OperatorStopped | crate::quiesce::StopReason::Cancelled
        ) {
            return Err(error("quarantine requires explicit Stop/Cancel"));
        }
        let observed = handle
            .observation
            .as_ref()
            .ok_or_else(|| error("quarantine identity unavailable"))?;
        let ids = observed
            .claim
            .members
            .iter()
            .map(|m| m.run_id.parse::<RunId>().map_err(error))
            .collect::<Result<BTreeSet<_>, _>>()?;
        if authority.members.keys().copied().collect::<BTreeSet<_>>() != ids {
            return Err(error("quarantine Stop authority membership mismatch"));
        }
        let gates = handle
            .members
            .iter()
            .map(|(id, _, _)| self.stop_restore_gate(*id))
            .collect::<Vec<_>>();
        let _stop = gates
            .iter()
            .map(|g| g.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
            .collect::<Vec<_>>();
        let _publication = handle
            .members
            .iter()
            .filter_map(|(_, _, e)| e.as_ref().map(|e| e.publication_gate()))
            .collect::<Vec<_>>();
        let indexes = self
            .shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let admissions = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let runs = lock_runs(&self.shared.runs);
        for (id, old, execution) in &handle.members {
            let index = indexes
                .get(id)
                .ok_or_else(|| error("quarantine index missing"))?;
            if index
                .paused_recovery
                .as_ref()
                .is_none_or(|p| p.claim_id != handle.claim_id || p.old_incarnation != *old)
                || match (&index.execution, execution) {
                    (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
                    (None, None) => false,
                    _ => true,
                }
                || admissions.contains_key(id)
                || runs.contains_key(id)
            {
                return Err(error("quarantine Stop member replaced or published"));
            }
        }
        drop(runs);
        drop(admissions);
        let permits = authority
            .members
            .values()
            .map(|c| {
                c.ownership
                    .clone()
                    .ok_or_else(|| error("quarantine Stop current ownership required"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        // Hold these guards through readback AND the FULL CAS. A handoff cannot
        // manufacture rights from either the saved recipe or the current row.
        let _authority = crate::ownership::OwnerPermit::mutation_guards(&permits).map_err(error)?;
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        let current = store
            .with_database(|db| db.recovery_claim_snapshot(&handle.claim_id))
            .map_err(error)?;
        if current.claim.members != observed.claim.members || current.manifest != observed.manifest
        {
            return Err(error("quarantine Stop claim identity changed"));
        }
        let mut expected = Vec::new();
        let mut stopped = Vec::new();
        let mut intents = Vec::new();
        for member in &current.claim.members {
            let id = member.run_id.parse::<RunId>().map_err(error)?;
            let record: RunRecoveryRecord = serde_json::from_str(
                &current
                    .manifest
                    .members
                    .iter()
                    .find(|m| m.run_id == member.run_id)
                    .ok_or_else(|| error("quarantine Stop record missing"))?
                    .record_json,
            )
            .map_err(error)?;
            let row = current
                .intents
                .iter()
                .find(|o| o.run_id == member.run_id)
                .ok_or_else(|| error("quarantine Stop intent missing"))?;
            let before = row
                .intent
                .clone()
                .ok_or_else(|| error("quarantine Stop intent unavailable"))?;
            let intent = ResumeIntent::decode(before.clone()).map_err(error)?;
            let local = handle
                .members
                .iter()
                .find(|(run, _, _)| *run == id)
                .and_then(|(_, _, e)| e.as_ref());
            if before.incarnation != member.new_incarnation
                || local.is_none_or(|e| e.publication().incarnation() != member.new_incarnation)
            {
                return Err(error("quarantine Stop refuses successor incarnation"));
            }
            self.validate_recovery_authority(
                &RecoverySource {
                    record,
                    intent: intent.clone(),
                    bundle: None,
                    snapshot: None,
                },
                &authority.members[&id],
            )?;
            let mut intent = intent;
            intent.active = false;
            intent.resume_requested = false;
            intent.stop_reason = Some(reason);
            expected.push((
                before,
                row.revision
                    .ok_or_else(|| error("quarantine Stop revision missing"))?,
            ));
            stopped.push(storage::resume_intent::ResumeIntentRecord {
                run_id: member.run_id.clone(),
                incarnation: intent.incarnation.clone(),
                payload: serde_json::to_string(&intent).map_err(error)?,
            });
            intents.push(intent);
        }
        store
            .handle
            .stop_quarantined_recovery_intents(handle.claim_id.clone(), expected, stopped)
            .map_err(error)?;
        self.shared.quiesce.adopt(intents, true);
        for (_, _, execution) in &handle.members {
            if let Some(execution) = execution {
                execution.stop_admission();
            }
        }
        // Keep the index: terminality/release remains unproven for this claim.
        Ok(())
    }
}
