//! ロールのチェーンのピンが run スナップショットを経て再起動後の affinity へ戻る (ADR 0030)。

mod support;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use event_bus::EventBus;
use providers::{ChatResponse, FinishReason, Message, ToolSpec};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, Role, RoutePinRecord, RunConfig, RunStore,
    RuntimeError,
};
use storage::{Storage, StorageConfig};
use support::{ScriptedModel, text_response};
use tools::ToolExecutor;

/// 固定のピンを報告し、復元時に渡されたピンを記録するモデル。
struct PinModel {
    inner: ScriptedModel,
    pins: Vec<RoutePinRecord>,
    seeded: Mutex<Vec<(String, Vec<RoutePinRecord>)>>,
}

#[async_trait]
impl AgentModel for PinModel {
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.inner.complete(invocation, role, messages, tools).await
    }

    fn selected_model(&self, role: Role, category: Option<&str>) -> String {
        self.inner.selected_model(role, category)
    }

    fn route_pins(&self, _run_id: &str) -> Vec<RoutePinRecord> {
        self.pins.clone()
    }

    fn restore_route_pins(&self, run_id: &str, pins: &[RoutePinRecord]) {
        self.seeded
            .lock()
            .unwrap()
            .push((run_id.to_owned(), pins.to_vec()));
    }
}

fn pin_model(pins: Vec<RoutePinRecord>) -> Arc<PinModel> {
    Arc::new(PinModel {
        inner: ScriptedModel::new([Ok(text_response("answer", FinishReason::Stop))]),
        pins,
        seeded: Mutex::new(Vec::new()),
    })
}

fn runtime(model: Arc<PinModel>, config: &StorageConfig, storage: &Storage) -> AgentRuntime {
    let bus = Arc::new(EventBus::new(128));
    AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model)
        .with_run_store(RunStore::open(config, storage.handle()).unwrap())
}

// Given: フォールバック後のピンを持つ run が終了し、スナップショットが保存されている
// When: 別の runtime (再起動) から同じチャットを再開する
// Then: 再開した run の ID でスナップショットのピンがモデルへ渡される
#[tokio::test]
async fn chat_restore_seeds_the_snapshot_pins_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("pins.sqlite3"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let pin = RoutePinRecord {
        chain: "orchestrator".into(),
        position: 1,
        profile: "neuralwatt".into(),
        model: "kimi-k3".into(),
    };
    let before = runtime(pin_model(vec![pin.clone()]), &config, &storage);
    let prior = before.delegate_background(
        Role::Orchestrator,
        "prior request".into(),
        RunConfig {
            name: Some(format!("chat:{}:thread", Role::Orchestrator.name())),
            ..RunConfig::default()
        },
    );
    before.wait(prior).await.unwrap();
    drop(before);

    let restarted_model = pin_model(Vec::new());
    let after = runtime(restarted_model.clone(), &config, &storage);
    let run = after
        .delegate_chat(
            "thread",
            Role::Orchestrator,
            "followup".into(),
            RunConfig::default(),
        )
        .expect("chat restores");
    after.wait(run).await.unwrap();

    assert_eq!(
        *restarted_model.seeded.lock().unwrap(),
        [(run.to_string(), vec![pin])]
    );
}
