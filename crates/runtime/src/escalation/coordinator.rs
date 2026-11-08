//! Two-phase responsibility transfer. No provider request is made by prepare.
use crate::{AgentRuntime, EscalationMemo, Role, RunConfig, RunId};
use event_bus::{Event, LanePolicy, LaneScope};
use storage::handoffs::{HandoffReceipt, HandoffState};

impl AgentRuntime {
    /// 3E should keep calling this from the root Worker tool handler, before
    /// returning its terminal directive. It must not be deferred to finalization.
    pub(crate) fn prepare_handoff_to_lane(
        &self,
        memo: &EscalationMemo,
        source_config: &RunConfig,
        continuity: crate::continuity::Continuity,
        publication: &crate::runtime::PublicationToken,
    ) -> Result<RunId, String> {
        let store = self
            .shared
            .run_store
            .get()
            .ok_or("handoff requires durable run storage")?;
        let source = memo.source_run_id;
        let fresh = self.shared.run_ids.next();
        let receipt = store
            .handle
            .reserve_handoff(HandoffReceipt {
                source_run_id: source.to_string(),
                source_incarnation: publication.incarnation(),
                target_run_id: fresh.to_string(),
                lane_id: format!("lane-{fresh}"),
                request_json: serde_json::to_string(memo).map_err(|e| e.to_string())?,
                state: HandoffState::Preparing,
                goal_json: None,
                todo_json: None,
            })
            .map_err(|e| e.to_string())?;
        let target: RunId = receipt
            .target_run_id
            .parse()
            .map_err(|e: crate::run::ParseRunIdError| e.to_string())?;
        if receipt.state.is_committed() {
            return Ok(target);
        }
        let origin = crate::thread_lineage::source_thread(self, source)
            .unwrap_or_else(|| format!("handoff-source-{source}"));
        let thread = event_bus::escalation_thread_id(&target.to_string());
        let config = RunConfig {
            lane_scope: Some(LaneScope::Lane),
            conversation: true,
            interactive: source_config.interactive,
            keep_alive: source_config.keep_alive,
            budget: source_config.budget.clone(),
            project_root: source_config.project_root.clone(),
            ..RunConfig::default()
        };
        // The receipt exists first, so neither a partial seed nor a Prepared lane
        // can be mistaken for a normal recoverable lane_open intent.
        let lane = store.handle.create_lane(storage::lanes::NewLane {
            lane_id: receipt.lane_id.clone(), thread_id: thread.clone(),
            origin_thread_id: origin.clone(), origin_run_id: source.to_string(),
            creation_key: format!("handoff:{source}:{target}"),
            brief: event_bus::LaneBrief {
                verification_requirements: Default::default(), objective: memo.original_request.clone(),
                inputs: vec![], deliverables: vec![], acceptance_criteria: vec!["Complete the original request and provide evidence".into()], constraints: vec![],
                workspace: Default::default(), report_policy: LanePolicy::Detached,
            },
            start_seed: serde_json::json!({"run_id":target,"project_root":source_config.project_root,"requires_ownership":source_config.ownership.is_some()}).to_string(),
            project_id: None, project_root: source_config.project_root.as_ref().map(|p|p.to_string_lossy().into_owned()),
            title: Some("Handoff · Orchestrator".into()), at_ns: crate::run_store::review_time_ns(),
        }).map_err(|e|e.to_string())?.lane;
        store
            .handle
            .upsert_thread_runtime(&storage::threads::ThreadUpsert {
                thread_id: thread.clone(),
                root_run_id: Some(target.to_string()),
                chat_role: Some(Role::Orchestrator.name().into()),
                lineage_kind: Some(storage::threads::ThreadLineageKind::Lane),
                escalation_source_run_id: Some(source.to_string()),
                created_at_ns: crate::run_store::review_time_ns(),
                updated_at_ns: crate::run_store::review_time_ns(),
                ..Default::default()
            })
            .map_err(|e| e.to_string())?;
        store
            .handle
            .prepare_handoff_questions(source.to_string(), receipt.source_incarnation.clone())
            .map_err(|e| e.to_string())?;
        let questions = store.user_questions(target).map_err(|e| e.to_string())?;
        let prompt = super::prompt::render_escalation_prompt(memo, &questions);
        let reference = self
            .persist_pending_reference(source)
            .map_err(|e| e.to_string())?
            .map(|r| r.reference_id);
        // Preparation retries reuse the target but freeze the source's latest
        // continuity and questions. Only uncommitted seeds may be replaced.
        crate::restore::persist_escalation_seed(
            self,
            target,
            source,
            &prompt,
            &config,
            source_config.ownership.is_some(),
            &continuity,
            reference,
        )
        .map_err(|e| e.to_string())?;
        // Reserve outside Q. Goal transfer prepares ownership before taking Q,
        // then commits the receipt with the exact goal/todo snapshots under Q.
        let execution = self.reserve_handoff_admission(source, target);
        let committed = publication.commit(|| {
            self.handoff_thread_goal_prepared(
                source,
                target,
                source_config.ownership.as_ref(),
                Some(&publication.incarnation()),
                |owner| {
                    // Stop/Cancel must be durable before the first target goal
                    // event; C is released before acquiring A (never I under Q).
                    self.shared
                        .lanes
                        .creation_intents
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(target, receipt.lane_id.clone());
                    self.update_admission_owner(target, &execution, owner.clone());
                    self.shared.bus.emit(Event::new(lane.created_event()));
                },
            )
            .map(|_| ())
        });
        if let Err(reason) = committed {
            self.fail_reserved_admission(target, &execution, reason.clone());
            return Err(reason);
        }
        crate::thread_lineage::record_escalation(self, target, source, None, &config);
        self.inherit_pull_request(Some(origin), &thread);
        for question in questions {
            self.shared
                .bus
                .emit(Event::new(event_bus::ToolEvent::UserQuestionUpdated {
                    question,
                }));
        }
        Ok(target)
    }

    pub(crate) fn start_committed_handoff(
        &self,
        source: RunId,
        incarnation: &str,
        source_config: &RunConfig,
    ) -> Result<RunId, String> {
        let store = self
            .shared
            .run_store
            .get()
            .ok_or("handoff storage is unavailable")?;
        let receipt = store
            .handle
            .handoff_operation(source.to_string(), incarnation.into())
            .map_err(|e| e.to_string())?
            .ok_or("handoff receipt is missing")?;
        if !receipt.state.is_committed() {
            return Err("handoff has not committed".into());
        }
        let target: RunId = receipt
            .target_run_id
            .parse()
            .map_err(|e: crate::run::ParseRunIdError| e.to_string())?;
        // Ownership was prepared before commit. The source may have released
        // its lease as soon as its terminal became public; never consult it
        // again to authorize this independent target.
        let ownership = self.admission_snapshot(target).and_then(|a| a.ownership);
        let resumed = if source_config.ownership.is_some() && ownership.is_none() {
            Err("prepared handoff ownership is missing".into())
        } else {
            self.spawn_prepared_handoff(target, ownership)
        };
        if let Err(reason) = resumed {
            let execution = self.reserve_handoff_admission(source, target);
            self.fail_reserved_admission(target, &execution, reason.clone());
            return Err(reason);
        }
        Ok(target)
    }
}
