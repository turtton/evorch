//! Resume the unchanged writer's SQLite bundles, not reconstructed old-shaped waits.
use crate::{
    AgentRuntime,
    quiesce::DurableWait,
    restore::{RestoredState, RunRestoreDescriptor},
    run_store::RunStore,
};
use event_bus::EventBus;
use providers::{ChatRequest, ChatResponse, ContentBlock, Message, ToolSpec};
use std::{path::PathBuf, sync::Arc};

const NAMES: [&str; 5] = [
    "runs-legacy-true",
    "runs-legacy-false",
    "delegates",
    "reply",
    "mixed",
];

fn source() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("fixtures/waits-main-2eec1163")
}

struct Fixture {
    _dir: test_socket_dir::SplitDir,
    config: storage::StorageConfig,
    storage: storage::Storage,
    store: RunStore,
}

impl Fixture {
    fn new() -> Self {
        let dir = test_socket_dir::SplitDir::new();
        let mut config = storage::StorageConfig {
            db_path: dir.path().join("legacy-resume.db"),
            ..Default::default()
        };
        config.thread_index.enabled = false;
        std::fs::copy(source().join("mixed-manifest.sqlite"), &config.db_path).unwrap();
        let storage = storage::Storage::open(config.clone()).unwrap();
        let store = RunStore::open(&config, storage.handle()).unwrap();
        Self {
            _dir: dir,
            config,
            storage,
            store,
        }
    }

    fn waits(&self, name: &str) -> Vec<DurableWait> {
        serde_json::from_slice(&std::fs::read(source().join(format!("{name}.waits.json"))).unwrap())
            .unwrap()
    }

    fn bundle(&self, name: &str) -> storage::thread_index::SnapshotBundle {
        let bundle = self
            .store
            .restore_bundle(self.waits(name)[0].requester)
            .unwrap()
            .unwrap();
        // These comparisons are against raw writer bytes, before any current writer runs.
        assert_eq!(
            bundle.record.config_json.as_bytes(),
            std::fs::read(source().join(format!("{name}.config.json"))).unwrap()
        );
        assert_eq!(
            bundle.record.messages_json.as_bytes(),
            std::fs::read(source().join(format!("{name}.messages.json"))).unwrap()
        );
        bundle
    }
}

fn outputs(messages: &[Message]) -> Vec<(&str, bool, String)> {
    messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_call_id,
                is_error,
                content,
            } => Some((
                tool_call_id.as_str(),
                *is_error,
                content
                    .iter()
                    .filter_map(|part| match part {
                        providers::ToolResultContent::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
            )),
            _ => None,
        })
        .collect()
}

fn request(messages: Vec<Message>, tools: Vec<ToolSpec>) -> ChatRequest {
    ChatRequest {
        model: "fixture-model".into(),
        messages,
        tools,
        temperature: None,
        max_tokens: None,
        reasoning_effort: None,
        service_tier: None,
        output_schema: None,
        tool_choice: None,
        observation: None,
        responses_replay: None,
    }
}

#[tokio::test]
async fn human_continue_of_old_bundles_appends_one_error_per_call_and_preserves_wire_prefix() {
    let f = Fixture::new();
    for name in NAMES {
        let bundle = f.bundle(name);
        let prefix: Vec<Message> = serde_json::from_str(&bundle.record.messages_json).unwrap();
        let waits = f.waits(name);
        let restored = RestoredState::for_human_recovery_bundle(&bundle).unwrap();
        assert_eq!(&restored.messages[..prefix.len()], prefix);
        assert!(restored.recovery_waits.is_empty());
        let results = outputs(&restored.messages);
        assert_eq!(
            results.iter().map(|(id, _, _)| *id).collect::<Vec<_>>(),
            waits
                .iter()
                .map(|w| w.tool_call_id.as_str())
                .collect::<Vec<_>>()
        );
        assert!(results.iter().all(|(_, error, _)| *error));
        if name == "reply" {
            assert_eq!(results[0].2, crate::meta::RETIRED_REPLY_WAIT);
        }
        // Model a second load of the saved continuation, not a second invocation
        // against the original, intentionally immutable unfinished snapshot.
        let mut continued = bundle.clone();
        let mut descriptor: RunRestoreDescriptor =
            serde_json::from_str(&continued.record.config_json).unwrap();
        descriptor.recovery_waits.clear();
        descriptor.recovery_boundary = None;
        continued.record.config_json = serde_json::to_string(&descriptor).unwrap();
        continued.record.messages_json = serde_json::to_string(&restored.messages).unwrap();
        let second = RestoredState::for_human_recovery_bundle(&continued).unwrap();
        assert_eq!(second.messages, restored.messages);
        assert!(second.recovery_waits.is_empty());

        let intent: crate::quiesce::ResumeIntent = serde_json::from_slice(
            &std::fs::read(source().join(format!("{name}.intent.json"))).unwrap(),
        )
        .unwrap();
        let before = intent.provider_input.unwrap().request;
        let after = request(restored.messages, before.tools.clone());
        let ids: Vec<_> = waits.iter().map(|w| w.tool_call_id.as_str()).collect();
        crate::recovery_cache_test_support::assert_wait_cache(&before, &after, &ids).await;
        assert_eq!(f.bundle(name).record, bundle.record);
    }
}

struct UnexpectedExecutionModel;
#[async_trait::async_trait]
impl crate::AgentModel for UnexpectedExecutionModel {
    fn selected_model(&self, _: crate::Role, _: Option<&str>) -> String {
        "unexpected-execution".into()
    }
    async fn complete(
        &self,
        _: &crate::AgentInvocationContext,
        _: crate::Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, crate::RuntimeError> {
        panic!("old recovery recipes must never reach a provider");
    }
}

#[tokio::test]
async fn old_wait_manifests_are_listed_but_never_automatically_activated() {
    let fixture = Fixture::new();
    let bus = Arc::new(EventBus::new(32));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        Arc::new(UnexpectedExecutionModel),
    )
    .with_run_store(RunStore::open(&fixture.config, fixture.storage.handle()).unwrap());
    runtime.install_planned_recovery_fixture().unwrap();
    let plan = runtime.recovery_plan().unwrap();
    assert!(plan.components.is_empty());
    assert_eq!(plan.report.blocked.len(), 4);
    assert!(
        plan.report
            .blocked
            .iter()
            .all(|entry| entry.reason == crate::quiesce::RecoveryReason::UnknownSchema)
    );
    assert!(
        runtime.list_agents().is_empty(),
        "old recovery recipes must not manufacture executions"
    );
    let database = storage::Database::open(&fixture.config).unwrap();
    let headers = database.recovery_graph_headers().unwrap();
    assert_eq!(
        headers.len(),
        4,
        "unsupported records remain visible to management"
    );
    assert!(headers.iter().all(|header| !header.is_current()));
}
