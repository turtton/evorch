mod support;

use config::{
    Config, MetadataSource, ModelEntryConfig, ModelPresetConfig, ProviderProfileConfig,
    SummarizerKind,
};
use event_bus::{CompactionEvent, EventBus, EventKind};
use runtime::{ModelSource, Role, RunConfig, RuntimeComposition, compose_runtime};
use sandbox::{DirectSandbox, credential::FileCredentialStore};
use std::collections::BTreeMap;
use std::sync::Arc;
use support::{ScriptedModel, drain_events, text_response};

#[tokio::test]
async fn composed_runtime_compacts_with_preset_window() {
    let mut entry = ModelEntryConfig::enabled("deepseek-v4-flash-0731");
    entry.metadata_source = Some(MetadataSource::Manual);
    entry.preset = Some("small".into());
    let mut config = Config {
        providers: BTreeMap::from([(
            "local".into(),
            ProviderProfileConfig {
                models: vec![entry],
                ..ProviderProfileConfig::default()
            },
        )]),
        model_presets: BTreeMap::from([(
            "small".into(),
            ModelPresetConfig {
                context_window: Some(64000),
                ..ModelPresetConfig::default()
            },
        )]),
        ..Config::default()
    };
    config.compaction.summarizer = SummarizerKind::Structural;
    config.compaction.keep_recent_tokens = 1;
    let bus = Arc::new(EventBus::new(128));
    let mut receiver = bus.subscribe();
    let dir = tempfile::tempdir().unwrap();
    let model = ScriptedModel::new([
        Ok(text_response(
            &"data ".repeat(50000),
            providers::FinishReason::Stop,
        )),
        Ok(text_response("done", providers::FinishReason::Stop)),
    ])
    .with_selected_model("local/deepseek-v4-flash-0731");
    let composed = compose_runtime(RuntimeComposition {
        config: &config,
        executor: Arc::new(tools::ToolExecutor::with_standard_tools(
            bus.clone(),
            Arc::new(DirectSandbox::new_unchecked()),
        )),
        bus,
        credential_store: Arc::new(FileCredentialStore::open(dir.path()).unwrap()),
        env: Arc::new(routing::MapEnv::default()),
        model_source: ModelSource::Fixed(Arc::new(model)),
        workspace: None,
    })
    .unwrap();

    let run = composed.runtime.delegate_background(
        Role::Worker,
        "compact accumulated history".into(),
        RunConfig {
            interactive: true,
            ..RunConfig::default()
        },
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while composed.runtime.inspect_agent(run).unwrap().phase != runtime::AgentRunPhase::Waiting
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    composed
        .runtime
        .send_message(run, "continue".into())
        .unwrap();
    let phase = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        composed.runtime.wait(run),
    )
    .await
    .unwrap()
    .unwrap();
    let events = drain_events(&mut receiver).await;

    assert_eq!(phase, runtime::AgentRunPhase::Done);
    assert!(events.iter().any(|event| matches!(
        event.kind,
        EventKind::Compaction(CompactionEvent::Compacted { .. })
    )));
}
