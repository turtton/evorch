//! Re-adopt the reserved claim; never reserve another revision or incarnation.
use super::*;
use storage::recovery::ValidatedPreparedClaim;

impl AgentRuntime {
    /// Storage evidence selects identities, not authority. The caller supplies
    /// current configuration/permits and publishes catalogs before adoption.
    pub fn adopt_prepared_claim(
        &self,
        proof: ValidatedPreparedClaim,
        startup: storage::RecoveryStartupComplete,
        ownership_root: &std::path::Path,
        authority: &CurrentRecoveryAuthority,
    ) -> Result<PausedRecoveryComponent, RuntimeError> {
        let models = self.pin_recovery_models(authority);
        self.adopt_prepared_claim_with_models(proof, startup, ownership_root, authority, &models)
    }

    /// Use the same models on which the current startup catalogs were published.
    pub fn adopt_prepared_claim_with_models(
        &self,
        proof: ValidatedPreparedClaim,
        startup: storage::RecoveryStartupComplete,
        ownership_root: &std::path::Path,
        authority: &CurrentRecoveryAuthority,
        models: &RecoveryModels,
    ) -> Result<PausedRecoveryComponent, RuntimeError> {
        let _dispatch = self.dispatch_registration_guard()?;
        let startup = Arc::new(startup);
        if !self
            .shared
            .recovery_message_ids_safe
            .load(Ordering::Acquire)
        {
            return Err(error("run store must be attached before recovery"));
        }
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        proof.validate_database(&store.db_path).map_err(error)?;
        // Repeat the writer/startup fence: a proof may have become stale since
        // issuance. This call reserves neither a revision nor a new claim.
        let current = store
            .handle
            .validate_prepared_claim(proof.claim().claim_id.clone(), &startup, ownership_root)
            .map_err(error)?;
        if current.claim() != proof.claim()
            || current.manifest() != proof.manifest()
            || current.snapshots() != proof.snapshots()
            || current.intents() != proof.intents()
        {
            return Err(error("prepared recovery evidence changed"));
        }
        let sources = self.prepared_sources(&current)?;
        let resume = current
            .claim()
            .members
            .iter()
            .map(|m| m.run_id.parse::<RunId>().map_err(error))
            .collect::<Result<BTreeSet<_>, _>>()?;
        if authority.members.keys().copied().collect::<BTreeSet<_>>() != resume {
            return Err(error("current authority membership mismatch"));
        }
        models.validate_members(self, &resume)?;
        for source in &sources {
            if let Some(config) = authority.members.get(&source.record.run_id) {
                self.validate_recovery_authority(source, config)?;
                models
                    .current(self, source.record.run_id, config)?
                    .validate_checkpoint_compatibility(
                        &crate::AgentInvocationContext {
                            run_id: source.record.run_id.to_string(),
                            category: config.category.clone(),
                            model_preference: config.model_preference.clone(),
                            ..Default::default()
                        },
                        Role::from_name(&source.record.role).map_err(error)?,
                        &source.intent.route_pins,
                        source.intent.provider_input.as_ref(),
                    )?;
            }
        }
        let ids = sources
            .iter()
            .map(|s| s.record.run_id)
            .collect::<BTreeSet<_>>();
        let gates = ids
            .iter()
            .map(|id| self.stop_restore_gate(*id))
            .collect::<Vec<_>>();
        let _stop = gates
            .iter()
            .map(|g| g.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
            .collect::<Vec<_>>();
        let mut indexes = self
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
        let mut controls = Vec::new();
        // A local failed activation retains its controls. Reuse only the exact
        // claim/incarnation, never replace an existing execution to recover it.
        for source in &sources {
            let id = source.record.run_id;
            if admissions.contains_key(&id) || runs.contains_key(&id) {
                return Err(error("recovery member already published"));
            }
            let execution = if let Some(index) = indexes.get(&id) {
                let same = index.paused_recovery.as_ref().is_some_and(|p| {
                    p.claim_id == current.claim().claim_id
                        && p.old_incarnation == source.intent.incarnation
                });
                if index.cancelled
                    || (!same && (index.execution.is_some() || index.paused_recovery.is_some()))
                {
                    return Err(error("recovery member already registered or stopped"));
                }
                if same {
                    match (
                        &index.execution,
                        current
                            .claim()
                            .members
                            .iter()
                            .find(|m| m.run_id == id.to_string()),
                    ) {
                        (Some(execution), Some(member))
                            if !execution.cancelled()
                                && execution.publication().incarnation()
                                    == member.new_incarnation =>
                        {
                            Some(execution.clone())
                        }
                        (None, None) => None,
                        _ => return Err(error("paused recovery control changed")),
                    }
                } else if resume.contains(&id) {
                    Some(ExecutionControl::for_prepared(&current, id)?)
                } else {
                    None
                }
            } else if resume.contains(&id) {
                Some(ExecutionControl::for_prepared(&current, id)?)
            } else {
                None
            };
            if let Some(execution) = execution {
                controls.push((id, execution));
            }
        }
        let mut reservations = Vec::new();
        for source in &sources {
            if let Some(receipt) = &source.snapshot {
                let bundle = source
                    .bundle
                    .as_ref()
                    .ok_or_else(|| error("missing recovery snapshot"))?;
                reservations.push(SnapshotReservation::prepare_recovery(
                    store,
                    source.record.run_id,
                    &bundle.record,
                    receipt,
                )?);
            }
        }
        let members = sources.iter().map(|s| s.intent.clone()).collect::<Vec<_>>();
        self.shared.quiesce.adopt(members.clone(), true);
        for source in &sources {
            let index = indexes.entry(source.record.run_id).or_default();
            index.parent = source.record.parent;
            index.execution = controls
                .iter()
                .find(|(id, _)| *id == source.record.run_id)
                .map(|(_, e)| e.clone());
            index.paused_recovery = Some(PausedRecoveryIndex {
                claim_id: current.claim().claim_id.clone(),
                old_incarnation: source.intent.incarnation.clone(),
                stop_config: authority.members.get(&source.record.run_id).cloned(),
                startup: Arc::clone(&startup),
                ownership_root: ownership_root.to_path_buf(),
            });
        }
        Ok(PausedRecoveryComponent {
            runtime: Arc::downgrade(&self.shared),
            claim: current.claim().clone(),
            members,
            controls,
            sources,
            relations: None,
            _reservations: reservations,
            _startup: startup,
            #[cfg(test)]
            lose_activation_ack: false,
        })
    }

    fn prepared_sources(
        &self,
        proof: &ValidatedPreparedClaim,
    ) -> Result<Vec<RecoverySource>, RuntimeError> {
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        let mut sources = Vec::new();
        for member in &proof.manifest().members {
            let record: RunRecoveryRecord =
                serde_json::from_str(&member.record_json).map_err(error)?;
            let row = proof
                .intents()
                .iter()
                .find(|i| i.run_id == member.run_id)
                .ok_or_else(|| error("missing prepared intent"))?;
            let intent = ResumeIntent::decode(row.clone()).map_err(error)?;
            if record.version != 1
                || record.run_id.to_string() != member.run_id
                || record.old_incarnation != member.incarnation
                || record.intent_revision != member.intent_revision
                || intent.incarnation != member.incarnation
                || intent.run_id != record.run_id
                || intent.parent_run_id != record.parent
                || intent.role != record.role
                || record.next_message_id > store.next_recovery_message_id
                || (member.resume && !intent.tool_intent.is_empty())
            {
                return Err(error("invalid prepared domain record"));
            }
            Role::from_name(&record.role).map_err(error)?;
            if member.resume && (!intent.active || intent.stop_reason.is_some()) {
                return Err(error("prepared member has stopped"));
            }
            if !member.resume && intent.active && intent.stop_reason.is_none() {
                return Err(error("prepared terminal endpoint is active"));
            }
            if !matches!(
                record.recipe.policy,
                crate::quiesce::ResumePolicy::CoreOwned
            ) || record.recipe.settings_reference
                != crate::quiesce::CORE_RECOVERY_SETTINGS_REFERENCE
            {
                return Err(error("prepared member requires current core authority"));
            }
            let bundle = store
                .with_database(|db| {
                    db.snapshot_bundle(&member.run_id)
                        .map_err(crate::restore::provenance::storage_error)
                })
                .map_err(error)?;
            if let Some(receipt) = &member.snapshot {
                let bundle = bundle
                    .as_ref()
                    .ok_or_else(|| error("missing prepared snapshot"))?;
                let exact = proof
                    .snapshots()
                    .iter()
                    .find(|s| s.run_id == member.run_id)
                    .ok_or_else(|| error("missing prepared snapshot expectation"))?;
                if !receipt.matches(&bundle.record)
                    || bundle.record.config_json != exact.config_json
                {
                    return Err(error("prepared snapshot changed"));
                }
                if member.resume {
                    crate::restore::validate_recovery_waits(&bundle.record, &record.waits)?;
                }
            } else if intent.initial_input.is_none() || !record.waits.is_empty() {
                return Err(error("missing prepared continuation"));
            }
            if !member.resume && !record.waits.is_empty() {
                return Err(error("terminal endpoint cannot resume a wait"));
            }
            if member.resume
                && (intent.checkpoint.as_ref().is_some_and(|c| {
                    c.boundary == crate::checkpoint::CheckpointBoundary::ToolsWaiting
                }) != !record.waits.is_empty())
            {
                return Err(error("prepared continuation boundary mismatch"));
            }
            sources.push(RecoverySource {
                record,
                intent,
                bundle,
                snapshot: member.snapshot.clone(),
            });
        }
        let records = sources.iter().map(|s| s.record.clone()).collect::<Vec<_>>();
        if super::super::recovery_graph::edges(&records) != proof.manifest().edges {
            return Err(error("prepared graph edges changed"));
        }
        for source in &sources {
            let mut cursor = source.record.run_id;
            let mut seen = BTreeSet::new();
            loop {
                if !seen.insert(cursor) {
                    return Err(error("cyclic prepared graph"));
                }
                let parent = records
                    .iter()
                    .find(|r| r.run_id == cursor)
                    .ok_or_else(|| error("incomplete prepared graph"))?;
                if parent.recipe.policy != source.record.recipe.policy {
                    return Err(error("prepared graph policy mismatch"));
                }
                if let Some(id) = parent.parent {
                    cursor = id;
                } else {
                    break;
                }
            }
            if cursor != source.record.root {
                return Err(error("prepared root mismatch"));
            }
        }
        Ok(sources)
    }
}
