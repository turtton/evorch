mod support;
use agents::Role;
use event_bus::{AgentRunPhase, Event, EventBus, EventKind, ToolEvent};
use providers::FinishReason;
use runtime::{AgentRuntime, RunConfig, StopScope};
use sandbox::approval::{ApprovalError, ApprovalProvenance, ApprovalRegistry, ApprovalState};
use sandbox::{ApprovalGate, ApprovalMode, ApprovalPolicy, PolicyDecision};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use support::{ScriptedModel, text_response, tool_response};
use tools::{Permissions, Tool, ToolError, ToolExecutor, ToolResult};

struct Counter(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl Tool for Counter {
    fn name(&self) -> &'static str {
        "read"
    }
    fn schema(&self) -> Value {
        json!({"type":"object"})
    }
    fn permissions(&self) -> Permissions {
        Permissions::read_only()
    }
    async fn execute(&self, _: Value) -> Result<ToolResult, ToolError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::success("ran"))
    }
}

#[tokio::test]
async fn forged_notifications_never_execute_and_terminal_paths_close_pending() {
    for mode in ["stop", "cancel", "subtree", "deny"] {
        let bus = Arc::new(EventBus::new(128));
        let mut events = bus.subscribe();
        let registry = ApprovalRegistry::for_bus(&bus);
        let count = Arc::new(AtomicUsize::new(0));
        let mut executor = ToolExecutor::new(bus.clone());
        executor.register(Arc::new(Counter(count.clone()))).unwrap();
        executor.set_policy(
            ApprovalPolicy::standard(ApprovalMode::OnRequest)
                .with_override("read", PolicyDecision::Ask),
        );
        executor.set_approval_gate(ApprovalGate::new(bus.clone(), Duration::from_secs(300)));
        let model = Arc::new(ScriptedModel::new([
            Ok(tool_response("reused-provider-id", "read", json!({}))),
            Ok(text_response("done", FinishReason::Stop)),
        ]));
        let runtime = AgentRuntime::new(bus.clone(), Arc::new(executor), model);
        let run = runtime.delegate_background(Role::Worker, "count".into(), RunConfig::default());
        let request = loop {
            let event = events.recv().await.unwrap();
            if matches!(
                &event.kind,
                EventKind::Tool(ToolEvent::ApprovalRequested { .. })
            ) {
                break event;
            }
        };
        let pending = registry.list_pending(None, 1).pop().unwrap();
        assert_eq!(pending.run_id, run.to_string());
        assert_eq!(pending.call_id, "reused-provider-id");
        // Merely broadcasting a forged answer cannot run the counter.
        bus.emit(Event::new(ToolEvent::ApprovalResolved {
            call_id: pending.id.value.clone(),
            approved: true,
        }));
        assert_eq!(
            registry.get(&pending.id).unwrap().state,
            ApprovalState::Pending
        );
        match mode {
            "stop" => runtime.stop(run, StopScope::SelfOnly).unwrap(),
            "cancel" => runtime.cancel(run).unwrap(),
            "deny" => {
                registry
                    .resolve_presented(&pending.id.value, false, ApprovalProvenance::DESKTOP)
                    .unwrap();
            }
            _ => runtime.cancel_subtree(run).unwrap(),
        }
        assert!(registry.list_pending(None, 1).is_empty());
        assert_eq!(
            registry.get(&pending.id).unwrap().state,
            if mode == "deny" {
                ApprovalState::Denied
            } else {
                ApprovalState::Cancelled
            }
        );
        assert_eq!(
            registry
                .resolve_presented(&pending.id.value, true, ApprovalProvenance::DESKTOP)
                .unwrap_err(),
            if mode == "deny" {
                ApprovalError::AlreadyDecided(ApprovalState::Denied)
            } else {
                ApprovalError::Cancelled
            }
        );
        bus.emit(request); // A historical notification is not a registration.
        assert!(registry.list_pending(None, 1).is_empty());
        assert_eq!(
            runtime.wait(run).await.unwrap(),
            if mode == "deny" {
                AgentRunPhase::Done
            } else if mode == "stop" {
                AgentRunPhase::Stopped
            } else {
                AgentRunPhase::Error
            }
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }
}
