use super::*;
use crate::lane_instructions::{LaneInstructionAction, LaneInstructionRequest};
use tools::{Permissions, Tool, ToolError, ToolResult};

struct InterruptedEffect {
    calls: Arc<AtomicUsize>,
    entered: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl Tool for InterruptedEffect {
    fn name(&self) -> &str {
        "read"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn permissions(&self) -> Permissions {
        // Exercise conservative effect metadata even for a role-visible tool name.
        Permissions::read_write()
    }
    async fn execute(&self, _: serde_json::Value) -> Result<ToolResult, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        std::future::pending().await
    }
}

fn restart(f: &mut Fixture, effect: Arc<InterruptedEffect>) -> Arc<Model> {
    let model = Arc::new(Model::default());
    let bus = Arc::new(EventBus::new(16384));
    let mut executor = tools::ToolExecutor::new(bus.clone());
    executor.register(effect).unwrap();
    f.events = bus.subscribe();
    f.model = model.clone();
    f.runtime = AgentRuntime::new(bus, Arc::new(executor), model.clone())
        .with_run_store(crate::RunStore::open(&f.config, f.storage.handle()).unwrap());
    model
}

#[tokio::test]
async fn lane_send_recovers_cold_legacy_history_without_replaying_effects_or_rewriting_history() {
    MANUAL_SOURCES
        .scope((), async {
            for phase in ["Done", "Error", "Checkpoint"] {
                scenario(false, phase).await;
            }
        })
        .await;
}

#[tokio::test]
async fn operator_stopped_checkpoint_and_other_nonterminal_phases_still_refuse_lane_send() {
    MANUAL_SOURCES
        .scope((), async {
            scenario(true, "Checkpoint").await;
            scenario(false, "Running").await;
            scenario(false, "Stopped").await;
        })
        .await;
}

async fn scenario(operator_stopped: bool, phase: &str) {
    let mut f = Fixture::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let effect = Arc::new(InterruptedEffect {
        calls: calls.clone(),
        entered: Arc::new(tokio::sync::Notify::new()),
    });
    restart(&mut f, effect.clone());
    let (lead, authority) = f.lead().await;
    let lane = f.lane(lead, "legacy-checkpoint").await;
    // Source completion must not start an independent driver during the crash fixture.
    *f.runtime.shared.lanes.delivery.host.lock().unwrap() = None;
    f.model.next_tools.lock().unwrap().insert(
        lane.root_run_id.to_string(),
        ContentBlock::ToolUse {
            id: "write-before-crash".into(),
            name: "read".into(),
            input: serde_json::json!({}),
        },
    );
    f.runtime
        .send_message(lane.root_run_id, "perform write".into())
        .unwrap();
    effect.entered.notified().await;
    let mut pending = f
        .runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_record(lane.root_run_id)
        .unwrap()
        .unwrap();
    if operator_stopped {
        f.runtime
            .stop(lane.root_run_id, crate::StopScope::SelfOnly)
            .unwrap();
    } else {
        f.runtime.cancel(lane.root_run_id).unwrap();
    }
    f.runtime.wait(lane.root_run_id).await.unwrap();
    f.finish(lead).await;
    let mut descriptor: crate::restore::RunRestoreDescriptor =
        serde_json::from_str(&pending.config_json).unwrap();
    assert!(descriptor.has_uncertain_effects());
    descriptor.restorable = false;
    descriptor.non_restorable_reason =
        Some(crate::restore::OWNERSHIP_ONLY_UNRESTORABLE_REASON.into());
    pending.restorable = false;
    pending.terminal_phase = phase.into();
    pending.config_json = serde_json::to_string(&descriptor).unwrap();
    f.storage.handle().upsert_run_context(&pending).unwrap();
    let saved: Vec<Message> = serde_json::from_str(&pending.messages_json).unwrap();
    let model = restart(&mut f, effect);
    assert!(f.runtime.inspect_agent(lane.root_run_id).is_err());
    f.runtime
        .resume_chat(lead, authority.target.authority.clone())
        .unwrap();
    f.phase(lead, AgentRunPhase::Waiting).await;
    model.requests.lock().unwrap().clear();
    let host = Arc::new(Host {
        targets: Mutex::new(HashMap::from([(
            lane.thread_id.clone(),
            LaneDeliveryTarget {
                root_run_id: lane.root_run_id,
                generation: 1,
                authority: authority.target.authority.clone(),
            },
        )])),
        deny: Mutex::new(None),
        fail: AtomicBool::new(false),
        restores: AtomicUsize::new(0),
        now: AtomicI64::new(crate::run_store::review_time_ns()),
    });
    *f.runtime.shared.lanes.delivery.host.lock().unwrap() = Some(host.clone());
    let sent = f
        .runtime
        .send_lane_instruction(
            lead,
            LaneInstructionRequest {
                operation_id: "lane-send-after-crash".into(),
                lane_id: lane.lane_id.clone(),
                expected_management_revision: 1,
                expected_brief_revision: 1,
                action: LaneInstructionAction::Supplement {
                    message: "inspect the write outcome before retrying".into(),
                },
            },
        )
        .unwrap();
    let outcome = drive(&f, &lane.thread_id).await;
    if operator_stopped || !matches!(phase, "Done" | "Error" | "Checkpoint") {
        assert_eq!(
            outcome,
            if operator_stopped {
                LaneDeliveryOutcome::Stopped
            } else {
                LaneDeliveryOutcome::Nacked(LaneDeliveryNack::Stopped)
            }
        );
        assert_eq!(host.restores.load(Ordering::SeqCst), 0);
        assert!(model.requests.lock().unwrap().is_empty());
    } else {
        assert_eq!(outcome, LaneDeliveryOutcome::Delivered { notifications: 1 });
        f.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
        assert_eq!(host.restores.load(Ordering::SeqCst), 1);
        assert_eq!(
            f.storage
                .handle()
                .lane_instruction_delivery(&lane.lane_id, sent.operation.instruction_seq.unwrap(),)
                .unwrap(),
            Some(crate::lane_instructions::LaneInstructionDelivery::Delivered)
        );
        {
            let observed = model.requests.lock().unwrap();
            assert_eq!(observed.len(), 1);
            assert!(observed[0].starts_with(&saved));
            let text = serde_json::to_string(&observed[0][saved.len()..]).unwrap();
            assert!(text.contains("ToolExecutionOutcomeUnknown"));
            assert!(text.contains("write-before-crash"));
            assert!(text.contains("inspect the write outcome before retrying"));
        }
        f.runtime
            .stop(lane.root_run_id, crate::StopScope::SelfOnly)
            .unwrap();
        f.runtime.wait(lane.root_run_id).await.unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    f.runtime.stop(lead, crate::StopScope::SelfOnly).unwrap();
    f.runtime.wait(lead).await.unwrap();
}
