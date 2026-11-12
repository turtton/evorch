//! All fallible preparation precedes the FULL activation transaction.
use super::*;
use crate::restore::{RestoredState, RunRestoreDescriptor};
use std::collections::BTreeMap;
use storage::recovery::{RecoveryActivationRequest, RecoverySnapshotExpectation};

/// Current, caller-granted capabilities, never deserialized from a recipe.
/// Every executable member needs its own config and live ownership permit.
pub struct CurrentRecoveryAuthority {
    pub members: BTreeMap<RunId, RunConfig>,
}

impl AgentRuntime {
    /// Reconnect evidence without publishing endpoints or consuming any notice.
    pub fn restore_relations_and_continuations(
        &self,
        paused: &mut PausedRecoveryComponent,
    ) -> Result<(), RuntimeError> {
        if !Weak::ptr_eq(&paused.runtime, &Arc::downgrade(&self.shared)) {
            return Err(error("foreign paused recovery component"));
        }
        paused.relations = Some(relations::PreparedRelations::prepare(&paused.sources)?);
        Ok(())
    }

    /// No task, model admission, tool, or wake starts until ALL members have
    /// been prepared, the FULL writer ACK received, and ALL endpoints published.
    ///
    /// The caller must load/publish any catalog needed for model compatibility
    /// before calling this API. Missing evidence blocks before ACK without
    /// fetching metadata or modifying the model's pins.
    pub fn activate_component(
        &self,
        paused: PausedRecoveryComponent,
        authority: CurrentRecoveryAuthority,
    ) -> Result<Vec<RunId>, RuntimeError> {
        let models = self.pin_recovery_models(&authority);
        self.activate_component_with_models(paused, authority, models)
    }

    /// Core startup retains these same pins while publishing catalogs. A stale
    /// context is rejected before ACK; after ACK execution keeps the pinned model.
    pub fn activate_component_with_models(
        &self,
        mut paused: PausedRecoveryComponent,
        mut authority: CurrentRecoveryAuthority,
        models: RecoveryModels,
    ) -> Result<Vec<RunId>, RuntimeError> {
        let _dispatch = self.dispatch_registration_guard()?;
        if !Weak::ptr_eq(&paused.runtime, &Arc::downgrade(&self.shared)) {
            return Err(error("foreign paused recovery component"));
        }
        let mut relations = paused
            .relations
            .take()
            .ok_or_else(|| error("relations have not been prepared"))?;
        let ids = paused
            .sources
            .iter()
            .map(|s| s.record.run_id)
            .collect::<BTreeSet<_>>();
        let resume = paused
            .controls
            .iter()
            .map(|(id, _)| *id)
            .collect::<BTreeSet<_>>();
        if authority.members.keys().copied().collect::<BTreeSet<_>>() != resume {
            return Err(error("current authority membership mismatch"));
        }
        models.validate_members(self, &resume)?;
        let gates = ids
            .iter()
            .map(|id| self.stop_restore_gate(*id))
            .collect::<Vec<_>>();
        let _stop = gates
            .iter()
            .map(|g| g.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
            .collect::<Vec<_>>();
        paused.controls.sort_unstable_by_key(|(id, _)| *id);
        let _publication = paused
            .controls
            .iter()
            .map(|(_, e)| e.publication_gate())
            .collect::<Vec<_>>();
        let mut indexes = self
            .shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        {
            let admissions = self
                .shared
                .admissions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let runs = lock_runs(&self.shared.runs);
            for source in &paused.sources {
                let id = source.record.run_id;
                let index = indexes
                    .get(&id)
                    .ok_or_else(|| error("missing paused index"))?;
                if index.cancelled
                    || index.paused_recovery.as_ref().is_none_or(|p| {
                        p.claim_id != paused.claim.claim_id
                            || p.old_incarnation != source.intent.incarnation
                    })
                    || admissions.contains_key(&id)
                    || runs.contains_key(&id)
                {
                    return Err(error("recovery member stopped or replaced"));
                }
            }
        }
        relations.reconcile_current(self)?;
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        // Revalidate the Prepared claim against current rows before loading its
        // retained bundles. The activation transaction repeats this fence.
        store
            .handle
            .validate_recovery_claim(paused.claim.claim_id.clone())
            .map_err(error)?;
        let mut candidates = Vec::new();
        let mut endpoints = Vec::new();
        let mut intents = Vec::new();
        let mut permits = Vec::new();
        let mut snapshots = Vec::new();
        for source in &paused.sources {
            let id = source.record.run_id;
            if let Some(bundle) = &source.bundle {
                snapshots.push(RecoverySnapshotExpectation {
                    run_id: id.to_string(),
                    receipt: source
                        .snapshot
                        .clone()
                        .ok_or_else(|| error("missing snapshot receipt"))?,
                    config_json: bundle.record.config_json.clone(),
                });
            }
            let Some((_, execution)) = paused.controls.iter().find(|(run, _)| *run == id) else {
                endpoints.push(relations.terminal_endpoint(source)?);
                continue;
            };
            if execution.cancelled() {
                return Err(error("recovery member stopped"));
            }
            let mut config = authority.members.remove(&id).expect("membership checked");
            self.validate_recovery_authority(source, &config)?;
            permits.push(config.ownership.clone().expect("validated ownership"));
            // Resolve before candidate construction: its ordinary registration
            // fallback logs errors, which is not sufficient for atomic recovery.
            if let Some(bundle) = &source.bundle {
                let d: RunRestoreDescriptor =
                    serde_json::from_str(&bundle.record.config_json).map_err(error)?;
                self.resolve_pending_reference(d.pending_reference_id.as_deref())?;
                config.spawn_credits = d.spawn_credits.clone();
            }
            let continuation = if let Some(bundle) = &source.bundle {
                if source.intent.checkpoint.as_ref().is_some_and(|c| {
                    c.boundary == crate::checkpoint::CheckpointBoundary::ToolsWaiting
                }) != !source.record.waits.is_empty()
                {
                    return Err(error("intent/continuation ToolsWaiting boundary mismatch"));
                }
                let restored = RestoredState::from_recovery_bundle(bundle, &source.record.waits)?;
                RunContinuation::Resume {
                    restored: Box::new(restored),
                    handoff: None,
                    after_stop: None,
                }
            } else {
                let input = source
                    .intent
                    .initial_input
                    .as_ref()
                    .ok_or_else(|| error("missing admission input"))?;
                config.images.clone_from(&input.images);
                RunContinuation::Fresh
            };
            let prompt = source
                .intent
                .initial_input
                .as_ref()
                .map(|i| i.text.clone())
                .unwrap_or_default();
            let mut candidate = self
                .build_run_candidate(
                    id,
                    source.record.parent,
                    Role::from_name(&source.record.role).map_err(error)?,
                    prompt,
                    config,
                    continuation,
                    None,
                    execution.clone(),
                    &indexes,
                    false,
                )
                .ok_or_else(|| error("recovery candidate was replaced"))?;
            if candidate.entry.config.project_root != source.record.project {
                return Err(error(
                    "project binding changed while building recovery candidate",
                ));
            }
            if candidate.task.startup_failure.is_some()
                || candidate.worker_slot.as_ref().is_some_and(Result::is_err)
            {
                return Err(error(
                    "recovery candidate failed startup or worker admission",
                ));
            }
            let model = models.current(self, id, &candidate.entry.config)?;
            model.validate_checkpoint_compatibility(
                &crate::AgentInvocationContext {
                    run_id: id.to_string(),
                    category: candidate.entry.config.category.clone(),
                    model_preference: candidate.entry.config.model_preference.clone(),
                    ..Default::default()
                },
                candidate.task.role,
                &source.intent.route_pins,
                source.intent.provider_input.as_ref(),
            )?;
            candidate.task.recovery_model = Some(model);
            relations.connect_candidate(&mut candidate)?;
            let mut intent = source.intent.clone();
            intent.incarnation = execution.publication().incarnation();
            intents.push(intent);
            candidates.push(candidate);
        }
        // Hold generation guards across the writer transaction and publication.
        // Never hold the global runs lock while waiting for the writer.
        let _authority = crate::ownership::OwnerPermit::mutation_guards(&permits).map_err(error)?;
        let request = RecoveryActivationRequest {
            claim_id: paused.claim.claim_id.clone(),
            intents: intents
                .iter()
                .map(|i| {
                    Ok(storage::resume_intent::ResumeIntentRecord {
                        run_id: i.run_id.to_string(),
                        incarnation: i.incarnation.clone(),
                        payload: serde_json::to_string(i).map_err(error)?,
                    })
                })
                .collect::<Result<_, RuntimeError>>()?,
            snapshots,
        };
        #[cfg(test)]
        self.execution_hook(
            cancellation::HookPoint::RecoveryBeforeActivation,
            &paused.controls[0].1,
        );
        // Recheck the actual inner model, not only its mutable wrapper, at the
        // last fallible boundary. Later reloads cannot change pinned execution.
        for candidate in &candidates {
            models.current(self, candidate.task.run_id, &candidate.entry.config)?;
        }
        // On an uncertain ACK, retain the durable claim and the closed gate.
        // Observation is diagnostic only: it never permits release or replay.
        if let Err(write_error) = store.handle.activate_recovery_claim(request) {
            let observation =
                store.with_database(|db| db.observe_recovery_activation(&paused.claim));
            return Err(error(format!(
                "activation ACK uncertain: {write_error}; observation={observation:?}"
            )));
        }
        #[cfg(test)]
        if paused.lose_activation_ack {
            return Err(error(
                "activation ACK lost after committed FULL transaction",
            ));
        }
        #[cfg(test)]
        self.execution_hook(
            cancellation::HookPoint::RecoveryActivated,
            &paused.controls[0].1,
        );
        // Pure checks above succeeded for every member. Adopt only after FULL ACK,
        // before any task can reach provider admission through the common gate.
        for source in &paused.sources {
            if paused
                .controls
                .iter()
                .any(|(id, _)| *id == source.record.run_id)
            {
                models
                    .model(source.record.run_id)
                    .expect("validated recovery model")
                    .adopt_recovery_checkpoint(
                        &source.record.run_id.to_string(),
                        &source.intent.route_pins,
                        source.intent.provider_input.as_ref(),
                    );
            }
        }
        self.shared.quiesce.adopt(intents, true);
        let (gate, closed) = watch::channel(false);
        relations.publish(self, &mut endpoints);
        for candidate in candidates {
            self.publish_run_candidate(candidate, Some(closed.clone()));
        }
        for id in &ids {
            indexes
                .get_mut(id)
                .expect("paused index held")
                .paused_recovery = None;
        }
        for reservation in &mut paused._reservations {
            reservation.commit();
        }
        #[cfg(test)]
        self.execution_hook(
            cancellation::HookPoint::RecoveryPublished,
            &paused.controls[0].1,
        );
        // All durable and process-local relationships are now connected.
        gate.send_replace(true);
        #[cfg(test)]
        self.execution_hook(
            cancellation::HookPoint::RecoveryGateOpened,
            &paused.controls[0].1,
        );
        Ok(resume.into_iter().collect())
    }

    pub(super) fn validate_recovery_authority(
        &self,
        source: &RecoverySource,
        config: &RunConfig,
    ) -> Result<(), RuntimeError> {
        let record = &source.record;
        if record.recipe.settings_reference != crate::quiesce::CORE_RECOVERY_SETTINGS_REFERENCE {
            return Err(error("unknown current settings selector"));
        }
        if !matches!(
            record.recipe.policy,
            crate::quiesce::ResumePolicy::CoreOwned
        ) {
            return Err(error(
                "current CLI continuity is not supplied by core authority",
            ));
        }
        let permit = config
            .ownership
            .as_ref()
            .ok_or_else(|| error("current ownership required"))?;
        permit.validate_generation().map_err(error)?;
        if record
            .thread
            .as_ref()
            .is_some_and(|thread| *thread != permit.thread_id)
            || config.project_root != record.project
            || config.lane_scope != record.recipe.lane
            || config.initial_prompt_id.is_some()
            || config.prompt_origin.is_some()
            || config.initial_thread_goal.is_some()
            || !config.skill_mentions.is_empty()
            || !config.images.is_empty()
        {
            return Err(error("current recovery authority identity mismatch"));
        }
        if let Some(saved) = record.recipe.descriptor.as_ref()
            && (config.category != saved.category
                || config.model_preference != saved.model_preference
                || config.load_skills != saved.load_skills
                || config.interactive != saved.interactive
                || config.keep_alive != saved.keep_alive)
        {
            return Err(error(
                "current execution settings differ from the checkpoint",
            ));
        }
        match (&record.recipe.team, &config.team) {
            (None, None)
                if config.team_task.is_none() && config.topology.worker_limit().is_none() => {}
            (Some(saved), Some(current))
                if saved.team_id == current.id
                    && saved.coordinator_run_id == current.coordinator =>
            {
                if current
                    .board
                    .snapshot()
                    .map_err(error)?
                    .iter()
                    .any(|task| matches!(task.state, crate::team::ClaimState::Claimed(_)))
                {
                    return Err(error(
                        "current team claims must be reconciled before activation",
                    ));
                }
            }
            _ => return Err(error("current team identity mismatch")),
        }
        Ok(())
    }
}
