//! Capture and planning only. Activation deliberately belongs to PR 06b/06c.
use super::*;
use crate::quiesce::*;
use std::collections::{BTreeMap, BTreeSet};
use storage::recovery::{RecoveryManifestRecord, RecoveryMemberRecord};

fn graph_error(reason: impl ToString) -> RuntimeError {
    crate::quiesce::runtime::error(reason)
}
fn snapshot_receipt(intent: &ResumeIntent) -> Option<storage::RunContextReceipt> {
    intent
        .checkpoint
        .as_ref()
        .map(|c| &c.snapshot)
        .or(intent.snapshot.as_ref())
        .and_then(|s| match s {
            crate::checkpoint::SnapshotReference::RunContext { receipt } => Some(receipt.clone()),
            _ => None,
        })
}
fn edges(records: &[RunRecoveryRecord]) -> Vec<RecoveryEdge> {
    let mut edges = BTreeSet::new();
    let captured: BTreeSet<_> = records.iter().map(|record| record.run_id).collect();
    for r in records {
        if let Some(parent) = r.parent {
            edges.insert((
                parent.to_string(),
                r.run_id.to_string(),
                "parent".to_string(),
            ));
        }
        for wait in &r.waits {
            for target in &wait.targets {
                edges.insert((
                    wait.requester.to_string(),
                    target.to_string(),
                    "wait".into(),
                ));
            }
        }
        for d in &r.deliveries {
            // Consumed receipts are historical evidence, not a live dependency.
            if d.state == DurableDeliveryState::Consumed {
                continue;
            }
            if !captured.contains(&d.sender) || !captured.contains(&d.recipient) {
                tracing::warn!(
                    run_id=%r.run_id, sender=%d.sender, recipient=%d.recipient,
                    state=?d.state, code="RecoveryDeliveryEndpointMissing",
                    "delivery retained without a graph edge: endpoint was not captured"
                );
                continue;
            }
            edges.insert((
                d.sender.to_string(),
                d.recipient.to_string(),
                "delivery".into(),
            ));
        }
    }
    edges
        .into_iter()
        .map(|(from, to, kind)| RecoveryEdge { from, to, kind })
        .collect()
}
fn root_of(run: RunId, intents: &BTreeMap<RunId, ResumeIntent>) -> Result<RunId, RuntimeError> {
    let mut current = run;
    let mut seen = BTreeSet::new();
    loop {
        if !seen.insert(current) {
            return Err(graph_error("cyclic recovery parent graph"));
        }
        match intents
            .get(&current)
            .ok_or_else(|| graph_error("missing recovery parent"))?
            .parent_run_id
        {
            Some(parent) => current = parent,
            None => return Ok(current),
        }
    }
}
fn delivery(
    q: &crate::mailbox::QueuedMessage,
    state: DurableDeliveryState,
    sequence: u64,
) -> Result<DurableDelivery, RuntimeError> {
    use crate::mailbox::completion::MessageOrigin;
    let notice = match &q.origin {
        MessageOrigin::Explicit => DurableNotice::Explicit,
        MessageOrigin::CompletionNotice(n) => DurableNotice::Completion {
            child: n.child,
            recipient: n.recipient,
            generation: n.generation,
            observed: n.observed(),
        },
        MessageOrigin::QuestionNotice(n) => DurableNotice::Question {
            id: n.id.clone(),
            parent: n.parent,
            child: n.child,
            blocking: n.blocking,
            observed: n.observed(),
            admitted: n.admitted.load(Ordering::Acquire),
            paused: n.paused.load(Ordering::Acquire),
        },
    };
    Ok(DurableDelivery {
        message_id: q.message.message_id.clone(),
        sender: q.message.sender_run_id.parse().map_err(graph_error)?,
        recipient: q.message.recipient_run_id.parse().map_err(graph_error)?,
        sequence,
        state,
        consumed_receipt: None,
        notice,
        message: Some(q.message.clone()),
    })
}
impl AgentRuntime {
    /// Non-destructive capture after restart quiescence and dispatch drain.
    /// The adapter MUST explicitly supply ownership/configuration references.
    /// Missing recipes remain Unsupported rather than guessing CoreOwned.
    pub fn capture_recovery_records(
        &self,
        recipes: &BTreeMap<RunId, RecoveryRecipe>,
    ) -> Result<Vec<RunRecoveryRecord>, RuntimeError> {
        if self.quiesce_state() != QuiesceState::Checkpointed || !self.dispatch_drained() {
            return Err(graph_error(
                "recovery capture requires Checkpointed and dispatch drain",
            ));
        }
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| graph_error("no run store"))?;
        let (intents, revisions) = store
            .with_database(|db| {
                let mut intents = BTreeMap::new();
                let mut revisions = BTreeMap::new();
                for (_, row) in db.recovery_intent_loads()? {
                    let Ok(row) = row else { continue };
                    let id = row.run_id.clone();
                    let Ok(intent) = ResumeIntent::decode(row) else {
                        continue;
                    };
                    let Some(revision) = db.resume_intent_revision(&id).ok().flatten() else {
                        continue;
                    };
                    revisions.insert(intent.run_id, revision);
                    intents.insert(intent.run_id, intent);
                }
                Ok((intents, revisions))
            })
            .map_err(graph_error)?;
        let runs = lock_runs(&self.shared.runs);
        let held = self
            .shared
            .held_messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let wakes = self
            .shared
            .auto_wakes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let resets = self
            .shared
            .wake_reset_generations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let sent = self
            .shared
            .sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut records = Vec::new();
        for intent in intents.values() {
            let Ok(root) = root_of(intent.run_id, &intents) else {
                tracing::warn!(run_id=%intent.run_id,"incomplete parent component excluded from recovery capture");
                continue;
            };
            let mut recipe = recipes
                .get(&intent.run_id)
                .or_else(|| recipes.get(&root))
                .cloned()
                .unwrap_or(RecoveryRecipe {
                    settings_reference: String::new(),
                    policy: ResumePolicy::Unsupported,
                    goal_id: None,
                    team: None,
                    lane: None,
                    descriptor: None,
                });
            recipe.policy = recipes
                .get(&root)
                .map(|r| r.policy.clone())
                .unwrap_or_default();
            let snapshot = match store
                .with_database(|db| db.run_context(&intent.run_id.to_string()))
            {
                Ok(snapshot) => snapshot,
                Err(_) => {
                    tracing::warn!(run_id=%intent.run_id, "unreadable snapshot excluded from recovery capture");
                    continue;
                }
            };
            if let Some(snapshot) = snapshot {
                let Ok(descriptor) = serde_json::from_str(&snapshot.config_json) else {
                    tracing::warn!(run_id=%intent.run_id, "corrupt snapshot descriptor excluded from recovery capture");
                    continue;
                };
                recipe.descriptor = Some(descriptor);
            }
            let descriptor = recipe.descriptor.as_ref();
            let mut deliveries = Vec::new();
            if let Some(entry) = runs.get(&intent.run_id) {
                for (index, q) in entry.mailbox.recovery_messages().iter().enumerate() {
                    deliveries.push(delivery(q, DurableDeliveryState::Mailbox, index as u64)?);
                }
            }
            if let Some(messages) = held.get(&intent.run_id) {
                for (index, q) in messages.iter().enumerate() {
                    deliveries.push(delivery(q, DurableDeliveryState::Held, index as u64)?);
                }
            }
            let pending_ids: BTreeSet<_> =
                deliveries.iter().map(|d| d.message_id.clone()).collect();
            let mut consumed: Vec<_> = sent
                .iter()
                .filter(|(id, record)| {
                    record.recipient == intent.run_id && !pending_ids.contains(*id)
                })
                .collect();
            consumed.sort_by(|a, b| a.0.cmp(b.0));
            for (sequence, (id, record)) in consumed.into_iter().enumerate() {
                deliveries.push(DurableDelivery {
                    message_id: id.clone(),
                    sender: record.sender,
                    recipient: record.recipient,
                    sequence: sequence as u64,
                    state: DurableDeliveryState::Consumed,
                    consumed_receipt: Some(id.clone()),
                    notice: DurableNotice::Explicit,
                    message: None,
                });
            }
            let completion = runs
                .get(&intent.run_id)
                .filter(|e| e.completion_notice.observed())
                .map(|e| ObservedGeneration {
                    id: e.completion_notice.child.to_string(),
                    generation: e.completion_notice.generation,
                });
            records.push(RunRecoveryRecord {
                version: 1,
                run_id: intent.run_id,
                old_incarnation: intent.incarnation.clone(),
                intent_revision: revisions[&intent.run_id],
                parent: intent.parent_run_id,
                root,
                thread: descriptor
                    .and_then(|d| d.thread_id.clone())
                    .or_else(|| self.goal_thread(intent.run_id)),
                project: descriptor.and_then(|d| d.project_root.clone()),
                role: intent.role.clone(),
                recipe,
                waits: Vec::new(),
                deliveries,
                completion,
                auto_wakes: wakes.get(&root).copied().unwrap_or_default(),
                user_reset_generation: resets.get(&root).copied().unwrap_or_default(),
                input_ids: intent
                    .initial_input
                    .iter()
                    .chain(intent.inputs.iter())
                    .map(|i| i.id.clone())
                    .collect(),
            });
        }
        Ok(records)
    }
    /// Persist weakly connected components. A component's partial capture never
    /// becomes executable; a failed commit leaves explicit Preparing evidence.
    ///
    /// `capture_id` MUST be unique for every attempt, including retries (PR 07).
    /// Components commit independently: Err does not roll back earlier successes,
    /// and later healthy components are still attempted. On Err, inspect durable
    /// manifests / recovery_plan to discover committed IDs before retrying with a
    /// new capture_id; the error itself does not return those IDs.
    pub fn save_recovery_graph(
        &self,
        capture_id: &str,
        records: &[RunRecoveryRecord],
    ) -> Result<Vec<String>, RuntimeError> {
        if self.quiesce_state() != QuiesceState::Checkpointed || !self.dispatch_drained() {
            return Err(graph_error(
                "recovery commit requires Checkpointed and dispatch drain",
            ));
        }
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| graph_error("no run store"))?;
        let mut pending: BTreeMap<RunId, &RunRecoveryRecord> =
            records.iter().map(|r| (r.run_id, r)).collect();
        if pending.len() != records.len() {
            return Err(graph_error("duplicate recovery run"));
        }
        let all_edges = edges(records);
        let mut manifests = Vec::new();
        let mut failure = None;
        while let Some((&first, _)) = pending.first_key_value() {
            let mut ids = BTreeSet::from([first]);
            loop {
                let old = ids.len();
                for e in &all_edges {
                    let a: RunId = e.from.parse().map_err(graph_error)?;
                    let b: RunId = e.to.parse().map_err(graph_error)?;
                    if ids.contains(&a) || ids.contains(&b) {
                        ids.insert(a);
                        ids.insert(b);
                    }
                }
                if old == ids.len() {
                    break;
                }
            }
            let saved = (|| -> Result<String, RuntimeError> {
                // Always remove this whole component, including when a dangling
                // endpoint sorts before its first real member. Otherwise a bad
                // edge would leave the cursor on the same component forever.
                let component: Vec<_> = ids
                    .iter()
                    .filter_map(|id| pending.remove(id).cloned())
                    .collect();
                if component.len() != ids.len() {
                    return Err(graph_error("recovery graph edge has no member"));
                }
                let manifest_id = format!("{capture_id}/{first}");
                let members = component
                    .iter()
                    .map(|r| {
                        let row = store
                            .with_database(|db| db.resume_intent(&r.run_id.to_string()))
                            .map_err(graph_error)?
                            .ok_or_else(|| graph_error("missing recovery intent"))?;
                        let intent = ResumeIntent::decode(row).map_err(graph_error)?;
                        if r.version != 1
                            || intent.run_id != r.run_id
                            || intent.incarnation != r.old_incarnation
                            || intent.parent_run_id != r.parent
                            || intent.role != r.role
                        {
                            return Err(graph_error("recovery member identity mismatch"));
                        }
                        Ok(RecoveryMemberRecord {
                            run_id: r.run_id.to_string(),
                            incarnation: r.old_incarnation.clone(),
                            intent_revision: r.intent_revision,
                            snapshot: snapshot_receipt(&intent),
                            resume: intent.active && intent.stop_reason.is_none(),
                            record_json: serde_json::to_string(r).map_err(graph_error)?,
                        })
                    })
                    .collect::<Result<Vec<_>, RuntimeError>>()?;
                let record = RecoveryManifestRecord {
                    manifest_id: manifest_id.clone(),
                    schema_version: 1,
                    committed: false,
                    superseded: false,
                    edges: edges(&component),
                    members,
                };
                store
                    .handle
                    .prepare_recovery_manifest(record)
                    .map_err(graph_error)?;
                store
                    .handle
                    .commit_recovery_manifest(manifest_id.clone())
                    .map_err(graph_error)?;
                Ok(manifest_id)
            })();
            match saved {
                Ok(id) => manifests.push(id),
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(manifests)
    }
    /// Read-only: candidates still require CURRENT authority. No automatic
    /// registration, provider call, claim consumption or activation occurs.
    pub fn recovery_plan(&self) -> Result<RecoveryPlan, RuntimeError> {
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| graph_error("no run store"))?;
        let mut plan = RecoveryPlan::default();
        let mut covered = BTreeSet::new();
        let loads = store
            .with_database(|db| db.recovery_manifests())
            .map_err(graph_error)?;
        let claims = store
            .with_database(|db| db.recovery_claim_loads())
            .map_err(graph_error)?;
        for load in loads {
            let id = load.manifest_id;
            let component_runs: Vec<_> = store
                .with_database(|db| db.recovery_manifest_run_ids(&id))
                .map_err(graph_error)?
                .into_iter()
                .filter_map(|id| id.parse::<RunId>().ok())
                .collect();
            covered.extend(component_runs.iter().copied());
            let mut candidates = Vec::new();
            let mut required = vec![
                RequiredRecoveryAuthority::CurrentOwnership,
                RequiredRecoveryAuthority::CurrentConfiguration,
            ];
            let check = (|| -> Result<(), RecoveryReason> {
                let manifest = load.record.map_err(|_| RecoveryReason::CorruptComponent)?;
                if manifest.superseded {
                    return Err(RecoveryReason::SupersededManifest);
                }
                if !manifest.committed {
                    return Err(RecoveryReason::PartialManifest);
                }
                if manifest.schema_version != 1 {
                    return Err(RecoveryReason::UnknownSchema);
                }
                let records = manifest
                    .members
                    .iter()
                    .map(|m| {
                        serde_json::from_str::<RunRecoveryRecord>(&m.record_json)
                            .map_err(|_| RecoveryReason::CorruptComponent)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if edges(&records) != manifest.edges {
                    return Err(RecoveryReason::MissingEdge);
                }
                for (_, claim) in claims.iter().filter(|(manifest, _)| manifest == &id) {
                    let claim = claim
                        .as_ref()
                        .map_err(|_| RecoveryReason::CorruptComponent)?;
                    if !matches!(claim.state, RecoveryClaimState::Released { .. }) {
                        return Err(RecoveryReason::PendingClaim);
                    }
                }
                for (member, r) in manifest.members.iter().zip(&records) {
                    if r.version != 1
                        || member.run_id != r.run_id.to_string()
                        || member.incarnation != r.old_incarnation
                        || member.intent_revision != r.intent_revision
                    {
                        return Err(RecoveryReason::CorruptComponent);
                    }
                    let (row, revision, snapshot) = store
                        .with_database(|db| {
                            Ok((
                                db.resume_intent(&member.run_id)?,
                                db.resume_intent_revision(&member.run_id)?,
                                db.run_context(&member.run_id)?,
                            ))
                        })
                        .map_err(|_| RecoveryReason::CorruptComponent)?;
                    let intent = ResumeIntent::decode(row.ok_or(RecoveryReason::CorruptComponent)?)
                        .map_err(|_| RecoveryReason::CorruptComponent)?;
                    if revision != Some(member.intent_revision)
                        || intent.incarnation != member.incarnation
                    {
                        return Err(RecoveryReason::IntentRevisionConflict);
                    }
                    if !intent.active || intent.stop_reason.is_some() {
                        let reason = match intent.stop_reason {
                            Some(StopReason::OperatorStopped) => RecoveryReason::OperatorStopped,
                            Some(StopReason::Cancelled) => RecoveryReason::Cancelled,
                            Some(StopReason::CliDisconnected) => RecoveryReason::CliDisconnected,
                            _ => RecoveryReason::Terminal,
                        };
                        plan.report.skipped.push(RecoveryOutcome {
                            manifest_id: Some(id.clone()),
                            runs: vec![r.run_id],
                            reason,
                        });
                        continue;
                    }
                    if !member.resume {
                        return Err(RecoveryReason::CorruptComponent);
                    }
                    if intent.parent_run_id != r.parent || intent.role != r.role {
                        return Err(RecoveryReason::CorruptComponent);
                    }
                    if !intent.tool_intent.is_empty() {
                        return Err(RecoveryReason::UnconfirmedEffects);
                    }
                    if !r.waits.is_empty() {
                        return Err(RecoveryReason::UnfinishedWaitUnsupportedIn06a);
                    }
                    if let Some(receipt) = &member.snapshot {
                        let snapshot = snapshot.ok_or(RecoveryReason::MissingSnapshot)?;
                        if !receipt.matches(&snapshot) {
                            return Err(RecoveryReason::MissingSnapshot);
                        }
                        let d: crate::restore::RunRestoreDescriptor =
                            serde_json::from_str(&snapshot.config_json)
                                .map_err(|_| RecoveryReason::CorruptComponent)?;
                        if d.non_restorable_reason.as_deref() == Some("snapshot_consumed") {
                            return Err(RecoveryReason::SnapshotConsumed);
                        }
                    } else if intent.initial_input.is_none() {
                        return Err(RecoveryReason::MissingSnapshot);
                    }
                    let root = records
                        .iter()
                        .find(|root| root.run_id == r.root && root.parent.is_none())
                        .ok_or(RecoveryReason::MissingEdge)?;
                    if r.recipe.policy != root.recipe.policy {
                        return Err(RecoveryReason::CorruptComponent);
                    }
                    match &root.recipe.policy {
                        ResumePolicy::CoreOwned => {}
                        ResumePolicy::CliInvocation { .. } => {
                            required.push(RequiredRecoveryAuthority::CliContinuity);
                            return Err(RecoveryReason::CliContinuityRequired);
                        }
                        ResumePolicy::Unsupported => return Err(RecoveryReason::UnknownOwnership),
                    }
                    if r.recipe.settings_reference.trim().is_empty() {
                        return Err(RecoveryReason::CurrentSettingsRequired);
                    }
                    if r.recipe.team.is_some() {
                        required.push(RequiredRecoveryAuthority::CurrentTeamAndClaimFence);
                    }
                    candidates.push(r.run_id);
                }
                Ok(())
            })();
            match check {
                Ok(()) => {
                    if !candidates.is_empty() {
                        plan.components.push(RecoveryComponent {
                            manifest_id: id,
                            runs: candidates,
                            required_authority: required,
                        });
                    }
                }
                Err(reason) => plan.report.blocked.push(RecoveryOutcome {
                    manifest_id: Some(id),
                    runs: component_runs,
                    reason,
                }),
            }
        }
        // Legacy/crash intents without a committed manifest are never candidates.
        let rows = store
            .with_database(|db| db.recovery_intent_loads())
            .map_err(graph_error)?;
        for (id, row) in rows {
            let run = id.and_then(|id| id.parse::<RunId>().ok());
            match row.ok().and_then(|row| ResumeIntent::decode(row).ok()) {
                Some(intent) if intent.active && !covered.contains(&intent.run_id) => {
                    plan.report.skipped.push(RecoveryOutcome {
                        manifest_id: None,
                        runs: vec![intent.run_id],
                        reason: RecoveryReason::NoCommittedManifest,
                    })
                }
                None => plan.report.blocked.push(RecoveryOutcome {
                    manifest_id: None,
                    runs: run.into_iter().collect(),
                    reason: RecoveryReason::CorruptComponent,
                }),
                _ => {}
            }
        }
        Ok(plan)
    }
}
