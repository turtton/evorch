//! Successful approval is an ordinary append-only provider turn.
use super::*;
use event_bus::ToolEvent;
use sandbox::approval::{ApprovalDecision, ApprovalProvenance, ApprovalState};
use sandbox::{ApprovalGate, ApprovalMode, ApprovalPolicy, PolicyDecision};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

struct CountedRead(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl Tool for CountedRead {
    fn name(&self) -> &'static str {
        BulkRead.name()
    }
    fn schema(&self) -> Value {
        BulkRead.schema()
    }
    fn permissions(&self) -> Permissions {
        BulkRead.permissions()
    }
    async fn execute(&self, input: Value) -> Result<ToolResult, ToolError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        BulkRead.execute(input).await
    }
}

#[tokio::test]
async fn approved_tool_preserves_next_provider_prefix() {
    for wire in [Wire::ChatCompletions, Wire::Responses, Wire::Codex] {
        let executed = Arc::new(AtomicUsize::new(0));
        let mut registry = None;
        let mut harness = harness_with_catalog_and_executor(
            wire,
            MODEL,
            None,
            vec![
                read_response(0),
                read_response(1),
                text_response("approved and complete"),
            ],
            1_000_000,
            Arc::new(CountedRead(executed.clone())),
            Vec::new(),
            None,
            0.9,
            |_| {},
            |executor, bus| {
                let gate = ApprovalGate::new(bus.clone(), Duration::from_secs(300));
                registry = Some(gate.registry());
                executor.set_approval_gate(gate);
                executor.set_policy(
                    ApprovalPolicy::standard(ApprovalMode::OnRequest)
                        .with_override("read", PolicyDecision::Ask),
                );
            },
        );
        let registry = registry.unwrap();
        let run = harness.runtime.delegate_background(
            Role::Worker,
            "Read the input after human approval and finish".into(),
            RunConfig::default(),
        );
        let mut events = Vec::new();
        let mut last_id = None;
        for index in 0..2 {
            loop {
                let event = harness.receiver.recv().await.unwrap();
                let pending = matches!(
                    &event.kind,
                    EventKind::Tool(ToolEvent::ApprovalRequested { .. })
                );
                events.push(event);
                if pending {
                    break;
                }
            }
            assert_eq!(executed.load(Ordering::SeqCst), index);
            let pending = registry.list_pending(None, 1).pop().unwrap();
            assert_eq!(pending.run_id, run.to_string());
            assert_eq!(
                registry
                    .full_input(&pending.id, pending.revision)
                    .unwrap()
                    .as_deref(),
                Some(&json!({"index": index}))
            );
            registry
                .resolve(
                    &pending.id,
                    pending.revision,
                    ApprovalDecision::Approve,
                    ApprovalProvenance::DESKTOP,
                )
                .unwrap();
            last_id = Some(pending.id);
        }
        events.extend(through_phase(&mut harness.receiver, run, AgentRunPhase::Done).await);
        assert_eq!(
            harness.runtime.wait(run).await.unwrap(),
            AgentRunPhase::Done
        );
        assert_eq!(executed.load(Ordering::SeqCst), 2);
        assert_eq!(
            registry.get(&last_id.unwrap()).unwrap().state,
            ApprovalState::Approved
        );
        assert!(events.iter().any(|e| matches!(
            &e.kind,
            EventKind::Tool(ToolEvent::ToolCompleted { call_id, is_error: false, .. })
                if call_id == "call-0"
        )));
        assert_eq!(
            harness
                .mock
                .recorded_requests()
                .into_iter()
                .filter(|r| r.path == wire.path())
                .count(),
            3
        );
        // Input-derived cache usage AND independent wire prefix/settings/tool
        // schema/affinity comparisons; no fixed cache-hit fixtures.
        verify_trace(&harness, run, &events, 0);
    }
}
