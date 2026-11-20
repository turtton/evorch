//! Resume the unchanged writer's SQLite bundles, not reconstructed old-shaped waits.
use crate::{
    AgentInvocationContext, AgentModel, AgentRuntime, Role, RunConfig, RunId, RuntimeError,
    quiesce::{
        DurableWait, DurableWaitMode, ProviderInput, RunRecoveryRecord, WaitContinuation, WaitKind,
        WaitPayload,
    },
    restore::{RestoredState, RunRestoreDescriptor},
    run_store::RunStore,
};
use event_bus::EventBus;
use providers::{ChatRequest, ChatResponse, ContentBlock, Message, ToolSpec};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use tokio::sync::{mpsc, oneshot};

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
    dir: test_socket_dir::SplitDir,
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
            dir,
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

    /// Extend only the unsent batch, retaining every old call and wait verbatim.
    /// The old writer cannot produce Events; the extra entry uses the current
    /// variant and the storage writer's real receipt/revision/manifest APIs.
    fn add_events_to_mixed_manifest(
        &self,
    ) -> (storage::thread_index::SnapshotBundle, Vec<DurableWait>) {
        let bundle = self.bundle("mixed");
        let mut waits = self.waits("mixed");
        let old_waits = waits.clone();
        let mut messages: Vec<Message> =
            serde_json::from_str(&bundle.record.messages_json).unwrap();
        let old_messages = messages.clone();
        let position = messages
            .iter()
            .flat_map(|m| &m.content)
            .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
            .count();
        waits.push(DurableWait {
            requester: waits[0].requester,
            targets: vec![],
            mode: DurableWaitMode::Any,
            tool_call_id: "current-events".into(),
            completions: vec![],
            questions: vec![],
            answers: vec![],
            continuation: WaitContinuation {
                tool_name: "wait".into(),
                legacy_result: false,
                consume_mailbox: true,
                payload: Some(WaitPayload {
                    position,
                    input: serde_json::json!({}),
                    kind: WaitKind::Events,
                }),
            },
        });
        let batch = messages.last_mut().unwrap();
        assert_eq!(batch.role, providers::Role::Assistant);
        batch.content.push(ContentBlock::ToolUse {
            id: "current-events".into(),
            name: "wait".into(),
            input: serde_json::json!({}),
        });
        assert_eq!(&waits[..old_waits.len()], old_waits);
        assert_eq!(
            &messages.last().unwrap().content[..old_messages.last().unwrap().content.len()],
            old_messages.last().unwrap().content
        );
        let mut record = bundle.record.clone();
        let mut descriptor: RunRestoreDescriptor =
            serde_json::from_str(&record.config_json).unwrap();
        descriptor.recovery_waits = waits.clone();
        record.config_json = serde_json::to_string(&descriptor).unwrap();
        record.messages_json = serde_json::to_string(&messages).unwrap();
        record.updated_at_ns += 1;
        crate::restore::validate_recovery_waits(&record, &waits).unwrap();
        let db = storage::Database::open(&self.config).unwrap();
        let mut manifest = db
            .recovery_manifests()
            .unwrap()
            .into_iter()
            .filter_map(|load| load.record.ok())
            .find(|manifest| manifest.members.iter().any(|m| m.run_id == record.run_id))
            .unwrap();
        let write = crate::restore::provenance::ConversationProvenance::from_bundle(&bundle)
            .write(record.clone(), bundle.snapshot_revision)
            .unwrap();
        let receipt = self.storage.handle().save_snapshot_bundle(&write).unwrap();
        let row = db.resume_intent(&record.run_id).unwrap().unwrap();
        let mut intent = crate::quiesce::ResumeIntent::decode(row).unwrap();
        let reference = crate::checkpoint::SnapshotReference::RunContext {
            receipt: receipt.run_context_receipt.clone(),
        };
        intent.snapshot = Some(reference.clone());
        intent.checkpoint.as_mut().unwrap().snapshot = reference;
        self.storage
            .handle()
            .save_resume_intent(
                storage::resume_intent::ResumeIntentRecord {
                    run_id: record.run_id.clone(),
                    incarnation: intent.incarnation.clone(),
                    payload: serde_json::to_string(&intent).unwrap(),
                },
                Some(intent.incarnation.clone()),
            )
            .unwrap();
        let member = manifest
            .members
            .iter_mut()
            .find(|m| m.run_id == record.run_id)
            .unwrap();
        member.snapshot = Some(receipt.run_context_receipt);
        member.intent_revision = db.resume_intent_revision(&record.run_id).unwrap().unwrap();
        let mut recovery: RunRecoveryRecord = serde_json::from_str(&member.record_json).unwrap();
        recovery.intent_revision = member.intent_revision;
        recovery.waits = waits.clone();
        recovery.recipe.descriptor = Some(descriptor);
        member.record_json = serde_json::to_string(&recovery).unwrap();
        manifest.manifest_id = "legacy-with-current-events".into();
        manifest.committed = false;
        self.storage
            .handle()
            .prepare_recovery_manifest(manifest.clone())
            .unwrap();
        self.storage
            .handle()
            .commit_recovery_manifest(manifest.manifest_id)
            .unwrap();
        (
            self.store
                .restore_bundle(waits[0].requester)
                .unwrap()
                .unwrap(),
            waits,
        )
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

type ModelEntrance = (RunId, ChatRequest, oneshot::Sender<ChatResponse>);

struct ResumeModel {
    entered: mpsc::UnboundedSender<ModelEntrance>,
}

async fn assert_reply_retirement_is_pure(f: &Fixture, runtime: &AgentRuntime) {
    let original = f.waits("reply").remove(0);
    let caller = original.requester;
    let parent = original.targets[0];
    let crate::quiesce::WaitKind::Reply {
        message_id,
        deadline_unix_ms,
        ..
    } = &original.continuation.payload.as_ref().unwrap().kind
    else {
        panic!("actual old Reply fixture")
    };
    assert_eq!(
        runtime.sent_message_recipient(caller, message_id).unwrap(),
        parent
    );
    let ordinary = runtime
        .send_agent_message(
            parent,
            caller,
            event_bus::AgentMessageKind::Send,
            "ordinary message remains queued",
            None,
        )
        .unwrap();
    let reply = runtime
        .send_agent_message(
            parent,
            caller,
            event_bus::AgentMessageKind::Reply,
            "correlated reply remains queued",
            Some(message_id.clone()),
        )
        .unwrap();
    let mailbox = runtime.run_mailbox(caller).unwrap();
    let queued = || {
        mailbox
            .recovery_messages()
            .iter()
            .map(|message| message.message.clone())
            .collect::<Vec<_>>()
    };
    let before = queued();
    assert!(before.iter().any(|m| m.message_id == ordinary));
    assert!(before.iter().any(|m| m.message_id == reply));
    let (_dir, _storage, state_runtime, mut state) =
        crate::agent_loop::continuity_tests::stored_fixture_in("retired-old-reply.db", false).await;
    let state_run = state.task.run_id;
    state.task.run_id = caller;
    // The first case uses the unchanged old bytes. The extrema make a deadline
    // comparison observable without making wall-clock time part of correctness.
    for deadline in [*deadline_unix_ms, 1, u64::MAX] {
        let mut wait = original.clone();
        if let crate::quiesce::WaitKind::Reply {
            deadline_unix_ms, ..
        } = &mut wait.continuation.payload.as_mut().unwrap().kind
        {
            *deadline_unix_ms = deadline;
        }
        let result = crate::meta::resume_wait(&mut state, runtime, &wait).await;
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, original.tool_call_id);
        assert!(result[0].1.result.is_error);
        assert_eq!(result[0].1.result.content, crate::meta::RETIRED_REPLY_WAIT);
        assert_eq!(queued(), before);
        assert_eq!(
            runtime.sent_message_recipient(caller, message_id).unwrap(),
            parent
        );
    }
    // Correlation remains usable even after repeated retirement attempts.
    runtime
        .send_agent_message(
            parent,
            caller,
            event_bus::AgentMessageKind::Reply,
            "second correlated reply",
            Some(message_id.clone()),
        )
        .unwrap();
    state_runtime.cancel(state_run).unwrap();
    state_runtime.wait(state_run).await.unwrap();
}

#[async_trait::async_trait]
impl AgentModel for ResumeModel {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "fixture-model".into()
    }

    fn validate_checkpoint_compatibility(
        &self,
        invocation: &AgentInvocationContext,
        _: Role,
        pins: &[crate::RoutePinRecord],
        input: Option<&ProviderInput>,
    ) -> Result<(), RuntimeError> {
        assert!(pins.is_empty());
        assert!(input.is_none_or(|i| {
            i.profile == "fixture"
                && i.request.model == "fixture-model"
                && i.affinity_key == invocation.run_id
        }));
        Ok(())
    }

    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let (release, resume) = oneshot::channel();
        self.entered
            .send((
                invocation.run_id.parse().unwrap(),
                request(messages.to_vec(), tools.to_vec()),
                release,
            ))
            .unwrap();
        Ok(resume.await.unwrap())
    }
}

#[tokio::test]
async fn old_runs_delegates_and_reply_activate_and_resume_without_replay_or_duplicate_results() {
    resume_legacy_manifests(false).await;
}

#[tokio::test]
async fn old_waits_and_current_events_in_one_manifest_resume_without_duplicate_results() {
    resume_legacy_manifests(true).await;
}

async fn resume_legacy_manifests(include_events: bool) {
    let f = Fixture::new();
    let mut saved: BTreeMap<_, _> = NAMES
        .iter()
        .map(|name| (*name, (f.bundle(name), f.waits(name))))
        .collect();
    if include_events {
        saved.insert("mixed", f.add_events_to_mixed_manifest());
    }
    let (entered, mut requests) = mpsc::unbounded_channel();
    let bus = Arc::new(EventBus::new(256));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        Arc::new(ResumeModel { entered }),
    )
    .with_run_store(RunStore::open(&f.config, f.storage.handle()).unwrap());
    runtime.install_planned_recovery_fixture().unwrap();
    let plan = runtime.recovery_plan().unwrap();
    assert_eq!(plan.components.len(), 4, "{plan:?}");
    let ownership = f.dir.path().join("ownership");
    let leases =
        storage::shared_with_wait(&f.config.db_path, &ownership, std::time::Duration::ZERO)
            .unwrap();
    let owner = crate::ownership::OwnerHost::open(
        f.dir.sockets(),
        Default::default(),
        Arc::new(EventBus::new(64)),
    )
    .unwrap();
    let callers: Vec<_> = saved
        .values()
        .map(|(_, waits)| waits[0].requester)
        .collect();
    let mut ids = Vec::new();
    for component in &plan.components {
        let startup = leases
            .recovery_startup_complete(&f.config.db_path, &ownership)
            .unwrap();
        let mut paused = runtime
            .prepare_component(component, startup, &ownership)
            .unwrap();
        runtime
            .restore_relations_and_continuations(&mut paused)
            .unwrap();
        let authority = crate::CurrentRecoveryAuthority {
            members: component
                .runs
                .iter()
                .map(|id| {
                    let bundle = f.store.restore_bundle(*id).unwrap().unwrap();
                    let descriptor: RunRestoreDescriptor =
                        serde_json::from_str(&bundle.record.config_json).unwrap();
                    (
                        *id,
                        RunConfig {
                            ownership: Some(owner.start(&id.to_string()).unwrap()),
                            interactive: descriptor.interactive,
                            keep_alive: descriptor.keep_alive,
                            ..Default::default()
                        },
                    )
                })
                .collect(),
        };
        ids.extend(
            ({
                let reservation = runtime
                    .reserve_automatic_activation(&(paused).claim().manifest_id)
                    .unwrap();
                runtime.activate_component(paused, authority, reservation)
            })
            .unwrap(),
        );
    }
    assert_eq!(ids.len(), 12);
    let children: Vec<_> = ids
        .iter()
        .filter(|id| !callers.contains(id))
        .copied()
        .collect();
    for child in &children {
        runtime.cancel(*child).unwrap();
        runtime.wait(*child).await.unwrap();
    }
    // Receive the real next-provider entrances, without wall-clock polling.
    let mut resumed = BTreeMap::new();
    let mut releases = Vec::new();
    while resumed.len() != callers.len() {
        let (id, input, release) = requests.recv().await.unwrap();
        if callers.contains(&id) {
            assert!(resumed.insert(id, input).is_none());
            releases.push(release);
            if id == saved["reply"].1[0].requester {
                assert_reply_retirement_is_pure(&f, &runtime).await;
                runtime.cancel(id).unwrap();
                runtime.wait(id).await.unwrap();
            }
        }
    }
    assert_eq!(
        runtime
            .list_agents()
            .iter()
            .map(|a| a.run_id)
            .collect::<std::collections::BTreeSet<_>>(),
        ids.iter().copied().collect()
    );
    runtime.request_quiesce();
    for release in releases {
        let _ = release.send(ChatResponse {
            message: Message {
                role: providers::Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "continued".into(),
                }],
            },
            usage: Default::default(),
            usage_reported: true,
            finish_reason: providers::FinishReason::Stop,
        });
    }
    runtime.wait_quiesced().await.unwrap();
    for (name, (bundle, waits)) in &saved {
        let run = waits[0].requester;
        let input = &resumed[&run];
        let prefix: Vec<Message> = serde_json::from_str(&bundle.record.messages_json).unwrap();
        assert_eq!(&input.messages[..prefix.len()], prefix, "{name}");
        let results = outputs(&input.messages);
        let expected: Vec<_> = waits.iter().map(|w| w.tool_call_id.as_str()).collect();
        assert_eq!(
            results.iter().map(|r| r.0).collect::<Vec<_>>(),
            expected,
            "{name}"
        );
        if *name == "reply" {
            assert_eq!(
                results,
                [("wait-reply", true, crate::meta::RETIRED_REPLY_WAIT.into())]
            );
        } else {
            assert!(results.iter().all(|r| !r.1), "{name}: {results:?}");
        }
        // Quiescence saves the closed batch after the provider continuation.
        // The retired call must not become pending again on either reload.
        let continued = f.store.restore_bundle(run).unwrap().unwrap();
        for _ in 0..2 {
            let second = RestoredState::from_bundle(&continued).unwrap();
            assert!(second.recovery_waits.is_empty());
            assert_eq!(outputs(&second.messages), results);
        }
        let intent: crate::quiesce::ResumeIntent = serde_json::from_slice(
            &std::fs::read(source().join(format!("{name}.intent.json"))).unwrap(),
        )
        .unwrap();
        let mut before = intent.provider_input.unwrap().request;
        // The planned build/schema boundary has separate E2E coverage. Here
        // hold the current tools fixed and test the resumed history append.
        before.tools = input.tools.clone();
        crate::recovery_cache_test_support::assert_wait_cache(&before, input, &expected).await;
    }
    for caller in callers {
        runtime.cancel(caller).unwrap();
        runtime.wait(caller).await.unwrap();
    }
}
