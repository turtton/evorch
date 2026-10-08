//! A panic inside an agent run's task ends the run in Error with a live diagnostic,
//! instead of leaving it Running with nothing reported.

mod support;

use std::sync::Arc;

use async_trait::async_trait;
use event_bus::{AgentRunPhase, EventKind, LifecycleEvent};
use providers::{ChatResponse, Message, ToolSpec};
use runtime::{
    AgentInvocationContext, AgentModel, ModelSource, Role, RunConfig, RuntimeComposition,
    RuntimeError, compose_runtime,
};
use support::drain_events;

struct PanickingModel;

#[async_trait]
impl AgentModel for PanickingModel {
    async fn complete(
        &self,
        _invocation: &AgentInvocationContext,
        _role: Role,
        _messages: &[Message],
        _tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        tokio::task::yield_now().await;
        panic!("model fixture exploded");
    }

    fn selected_model(&self, _role: Role, _category: Option<&str>) -> String {
        "panicking".into()
    }
}

#[tokio::test]
async fn a_panicking_run_ends_in_error_with_its_panic_site() {
    // Given: a recording hook and a model that panics mid-run.
    runtime::panic_capture::install();
    let bus = Arc::new(event_bus::EventBus::new(1024));
    let mut receiver = bus.subscribe();
    let dir = tempfile::tempdir().unwrap();
    let config = config::Config::default();
    let composed = compose_runtime(RuntimeComposition {
        user_config_dir: Some(dir.path().join("empty-user-config")),
        config: &config,
        executor: Arc::new(tools::ToolExecutor::with_standard_tools(
            bus.clone(),
            Arc::new(sandbox::DirectSandbox::new_unchecked()),
        )),
        bus,
        credential_store: Arc::new(
            sandbox::credential::FileCredentialStore::open(dir.path()).unwrap(),
        ),
        env: Arc::new(routing::MapEnv::default()),
        model_source: ModelSource::Fixed(Arc::new(PanickingModel)),
        workspace: None,
    })
    .unwrap();
    // When: the run's task unwinds.
    let run =
        composed
            .runtime
            .delegate_background(Role::Worker, "panic".into(), RunConfig::default());
    let phase = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        composed.runtime.wait(run),
    )
    .await
    .expect("a panicked run still reaches a terminal phase")
    .unwrap();
    // Then: Error, with one correlated diagnostic naming the panic site.
    assert_eq!(phase, AgentRunPhase::Error);
    let events = drain_events(&mut receiver).await;
    let diagnostics: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Diagnostic(d) if d.code == "AgentRunPanicked" => Some(d),
            _ => None,
        })
        .collect();
    assert_eq!(diagnostics.len(), 1);
    let diagnostic = diagnostics[0];
    assert_eq!(diagnostic.run_id.as_deref(), Some(run.to_string().as_str()));
    assert_eq!(diagnostic.severity, event_bus::DiagnosticSeverity::Error);
    let mut lines = diagnostic.detail.lines();
    assert_eq!(lines.next(), Some("model fixture exploded"));
    assert!(
        diagnostic
            .detail
            .contains("site=crates/runtime/tests/run_panic.rs:"),
        "{}",
        diagnostic.detail
    );
    assert!(
        diagnostic
            .detail
            .contains("shell jobs: stopped; workspace retained"),
        "{}",
        diagnostic.detail
    );
    assert!(
        diagnostic.detail.contains("run_panic::PanickingModel"),
        "backtrace names the panicking frame: {}",
        diagnostic.detail
    );
    let reasons: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                to: AgentRunPhase::Error,
                reason,
                ..
            }) => reason.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(reasons, ["panicked: model fixture exploded"]);
}
