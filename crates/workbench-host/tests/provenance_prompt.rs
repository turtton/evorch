//! Provenance is bookkeeping beside a command: it must not reach the model
//! input, or two clients sending the same words would break each other's
//! prompt cache prefix.

mod common;

use std::sync::{Arc, Mutex};
use workbench_protocol::namespaces::ConversationCommand;

use async_trait::async_trait;
use event_bus::EventBus;
use providers::{ChatResponse, Message, ToolSpec};
use runtime::{AgentInvocationContext, AgentModel, AgentRuntime, Role, RuntimeError};
use tokio::sync::Notify;
use tools::ToolExecutor;
use workbench_host::provenance::local_desktop;
use workbench_protocol::commands::Command;

use common::{cli, host_with_project, send, web};

/// Records the first request it receives and then never answers.
#[derive(Default)]
struct RecordingModel {
    first_request: Mutex<Option<String>>,
    received: Notify,
}

#[async_trait]
impl AgentModel for RecordingModel {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.first_request
            .lock()
            .unwrap()
            .get_or_insert_with(|| format!("{messages:?}\n{tools:?}"));
        self.received.notify_one();
        std::future::pending().await
    }

    fn selected_model(&self, role: Role, _: Option<&str>) -> String {
        format!("test-{}", role.name().to_lowercase())
    }
}

fn first_request_for(provenance: workbench_protocol::envelope::Provenance) -> String {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(RecordingModel::default());
    let bus = Arc::new(EventBus::new(64));
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone());
    let mut host = host_with_project(&rt, runtime, dir.path());
    host.handle_command(
        Command::Conversation(ConversationCommand::SendChat(send(
            "thread-1", "sub-1", "hello",
        ))),
        provenance,
    )
    .unwrap();
    rt.block_on(model.received.notified());
    model.first_request.lock().unwrap().clone().unwrap()
}

#[test]
fn the_model_input_is_the_same_whichever_surface_sent_the_message() {
    let from_web = first_request_for(web());
    let from_cli = first_request_for(cli());
    let from_desktop = first_request_for(local_desktop());

    assert!(from_web.contains("hello"));
    assert_eq!(from_web, from_cli);
    assert_eq!(from_web, from_desktop);
    assert!(!from_web.contains("web-1"));
}
