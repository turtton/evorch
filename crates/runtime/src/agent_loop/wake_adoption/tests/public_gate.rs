use crate::agent_loop::idle_adoption_tests::wait_for_phase;
use crate::escalation_review::support::{ScriptedModel, text_response};
use crate::{AgentRuntime, RunConfig};
use agents::Role;
use event_bus::{AgentMessageKind, AgentRunPhase, EventBus};
use providers::FinishReason;
use std::sync::Arc;
use tools::ToolExecutor;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mailbox_is_empty_but_adoption_survives_until_publication_or_cancel() {
    for cancel in [false, true] {
        let bus = Arc::new(EventBus::new(128));
        let mut events = bus.subscribe();
        let model = Arc::new(ScriptedModel::new([
            Ok(text_response("parent first", FinishReason::Stop)),
            Ok(text_response("child first", FinishReason::Stop)),
            Ok(text_response("parent second", FinishReason::Stop)),
        ]));
        let runtime =
            AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone());
        let config = RunConfig {
            interactive: true,
            keep_alive: true,
            ..Default::default()
        };
        let parent = runtime
            .delegate_chat(
                "wake-adoption",
                Role::Orchestrator,
                "parent".into(),
                config.clone(),
            )
            .unwrap();
        wait_for_phase(&mut events, parent, AgentRunPhase::Waiting).await;
        let child = runtime
            .delegate_background_as_child(parent, Role::Worker, "child", config)
            .unwrap();
        wait_for_phase(&mut events, child, AgentRunPhase::Waiting).await;
        let model_resume = Arc::new(tokio::sync::Notify::new());
        model.gate_key("parent", model_resume.clone()).await;
        let (reached, held) = tokio::sync::oneshot::channel();
        let (resume, resumed) = std::sync::mpsc::channel();
        runtime
            .gate_next_input_wake_adoption(parent, reached, resumed)
            .unwrap();
        runtime
            .send_agent_message(child, parent, AgentMessageKind::Send, "wake parent", None)
            .unwrap();
        held.await.unwrap();
        let info = runtime
            .list_agents()
            .into_iter()
            .find(|run| run.run_id == parent)
            .unwrap();
        assert_eq!(info.phase, AgentRunPhase::Waiting);
        assert!(
            runtime.run_mailbox(parent).unwrap().is_empty(),
            "the handler has already removed the wake-up input"
        );
        assert_eq!(runtime.follow_up_status(parent).unwrap().pending, 0);
        assert!(runtime.is_adopting_user_input(parent));
        assert!(
            runtime.is_adopting_user_input(parent),
            "observation preserves the guard"
        );
        // The gate has no runtime-map lock and cannot stall a separate runtime.
        let other_bus = Arc::new(EventBus::new(32));
        let other = AgentRuntime::new(
            other_bus.clone(),
            Arc::new(ToolExecutor::new(other_bus)),
            Arc::new(ScriptedModel::new([Ok(text_response(
                "other",
                FinishReason::Stop,
            ))])),
        );
        let other_run =
            other.delegate_background(Role::Orchestrator, "other".into(), RunConfig::default());
        assert_eq!(other.wait(other_run).await.unwrap(), AgentRunPhase::Done);
        assert!(!other.is_adopting_user_input(other_run));
        assert!(runtime.is_adopting_user_input(parent));
        if cancel {
            runtime.cancel(parent).unwrap();
        }
        resume.send(()).unwrap();
        if cancel {
            assert_eq!(runtime.wait(parent).await.unwrap(), AgentRunPhase::Error);
        } else {
            model.wait_for_request(2).await;
        }
        assert!(!runtime.is_adopting_user_input(parent));
        if !cancel {
            model_resume.notify_one();
            wait_for_phase(&mut events, parent, AgentRunPhase::Waiting).await;
            runtime.cancel(parent).unwrap();
            runtime.wait(parent).await.unwrap();
        }
        let _ = runtime.cancel(child);
        runtime.wait(child).await.unwrap();
    }
}
