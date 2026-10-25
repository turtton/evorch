use std::collections::VecDeque;

use super::fallback::fixture_with;
use super::*;
use providers::{CompactionResult, Compactor};

/// Answers with the queued results first, then succeeds; optionally replays compaction.
pub(super) struct QueueClient {
    replies: Mutex<VecDeque<Result<ChatResponse, ProviderError>>>,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    replays: bool,
}

impl QueueClient {
    pub(super) fn install(
        model: &mut RoutedModel,
        profile: &str,
        replies: Vec<Result<ChatResponse, ProviderError>>,
        replays: bool,
    ) -> Arc<Mutex<Vec<ChatRequest>>> {
        let requests = Arc::new(Mutex::new(Vec::new()));
        model.providers.get_mut(profile).unwrap().client = Arc::new(Self {
            replies: Mutex::new(replies.into()),
            requests: requests.clone(),
            replays,
        });
        requests
    }
}

#[async_trait]
impl Compactor for QueueClient {
    async fn compact(&self, _request: &ChatRequest) -> Result<CompactionResult, ProviderError> {
        Ok(CompactionResult {
            encrypted_content: "opaque".into(),
            usage: Usage::default(),
        })
    }
}

#[async_trait]
impl ProviderClient for QueueClient {
    fn compactor(&self) -> Option<&dyn Compactor> {
        self.replays.then_some(self as &dyn Compactor)
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: false,
            tool_use: true,
            reasoning: false,
        }
    }

    async fn send(
        &self,
        _auth: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        self.requests.lock().unwrap().push(request.clone());
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(response()))
    }

    async fn stream(
        &self,
        _auth: &ProviderAuth,
        _request: &ChatRequest,
    ) -> Result<DeltaStream, ProviderError> {
        Err(ProviderError::Request("stream unsupported".to_string()))
    }
}

fn text(role: MessageRole, text: impl Into<String>) -> Message {
    Message {
        role,
        content: vec![ContentBlock::Text { text: text.into() }],
    }
}

fn history_with_checkpoint_blob() -> Vec<Message> {
    vec![
        text(MessageRole::User, "goal"),
        Message {
            role: MessageRole::User,
            content: vec![ContentBlock::Compaction {
                encrypted_content: "opaque".into(),
            }],
        },
        text(MessageRole::User, "continue"),
    ]
}

fn carries_blob(request: &ChatRequest) -> bool {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .any(|block| matches!(block, ContentBlock::Compaction { .. }))
}

fn pin(position: usize) -> crate::RoutePinRecord {
    crate::RoutePinRecord {
        chain: "worker".into(),
        position,
        profile: format!("profile-{position}"),
        model: format!("model-{position}"),
    }
}

async fn send(
    model: &RoutedModel,
    purpose: event_bus::RequestPurpose,
    messages: &[Message],
) -> Result<ChatResponse, RuntimeError> {
    model
        .complete(
            &AgentInvocationContext {
                run_id: "run".into(),
                purpose,
                ..Default::default()
            },
            Role::Worker,
            messages,
            &[],
        )
        .await
}

#[tokio::test]
async fn checkpoint_blob_is_never_sent_to_a_member_that_cannot_replay_it() {
    // Given: the replaying head fails, and the next member has no official compaction.
    let (mut model, _) = fixture_with(vec![None, None], &[], &[]);
    let head = QueueClient::install(
        &mut model,
        "profile-0",
        vec![Ok(response()), Err(ProviderError::Timeout)],
        true,
    );
    let next = QueueClient::install(&mut model, "profile-1", vec![], false);
    send(&model, event_bus::RequestPurpose::Agent, &[])
        .await
        .unwrap();
    // When: the history carries the head's checkpoint blob.
    let result = send(
        &model,
        event_bus::RequestPurpose::Agent,
        &history_with_checkpoint_blob(),
    )
    .await;
    // Then: the loop is asked to re-summarize first, and the pin already moved.
    assert_eq!(
        result,
        Err(RuntimeError::ModelSwitch {
            selected: "profile-1/model-1".into(),
            resummarize: true,
        })
    );
    assert!(carries_blob(&head.lock().unwrap()[1]));
    assert!(next.lock().unwrap().is_empty());
    assert_eq!(model.route_pins("run"), [pin(1)]);
}

#[tokio::test]
async fn replaying_member_receives_the_checkpoint_blob() {
    // Given: the head replays official compaction.
    let (mut model, _) = fixture_with(vec![None], &[], &[]);
    let head = QueueClient::install(&mut model, "profile-0", vec![], true);
    send(&model, event_bus::RequestPurpose::Agent, &[])
        .await
        .unwrap();
    // When
    let result = send(
        &model,
        event_bus::RequestPurpose::Agent,
        &history_with_checkpoint_blob(),
    )
    .await;
    // Then: the blob is forwarded untouched.
    assert_eq!(result, Ok(response()));
    assert!(carries_blob(&head.lock().unwrap()[1]));
}

#[tokio::test]
async fn a_switch_returns_to_the_loop_before_the_new_member_is_sent_to() {
    // Given: a run pinned to a head that times out.
    let (model, requests) = fixture_with(vec![Some(ProviderError::Timeout), None], &[], &[]);
    model.restore_route_pins("run", &[pin(0)]);
    let history = [text(MessageRole::User, "history")];
    // When
    let switch = send(&model, event_bus::RequestPurpose::Agent, &history).await;
    // Then: the loop may compact for the new member's window before anything reaches it.
    assert_eq!(
        switch,
        Err(RuntimeError::ModelSwitch {
            selected: "profile-1/model-1".into(),
            resummarize: false,
        })
    );
    assert!(requests[1].lock().unwrap().is_empty());
    // When: the loop sends again on the new pin.
    let resent = send(&model, event_bus::RequestPurpose::Agent, &history).await;
    // Then: the pinned member is no longer a switch, so the request goes out.
    assert_eq!(resent, Ok(response()));
    assert_eq!(requests[1].lock().unwrap().len(), 1);
}

#[tokio::test]
async fn summary_skips_members_too_small_for_the_input_and_keeps_the_pin() {
    // Given: the run is pinned to a member whose window cannot hold the summary input.
    let (mut model, requests) = fixture_with(vec![None, None], &[], &[]);
    model.configured_windows = BTreeMap::from([("profile-0/model-0".into(), 1_000)]);
    model.restore_route_pins("run", &[pin(0)]);
    let history = [text(MessageRole::User, "history ".repeat(2_000))];
    // When: the loop summarizes.
    let result = send(&model, event_bus::RequestPurpose::Compaction, &history).await;
    // Then: the summary uses the next member that fits, and the run stays pinned.
    assert_eq!(result, Ok(response()));
    assert!(requests[0].lock().unwrap().is_empty());
    assert_eq!(requests[1].lock().unwrap().len(), 1);
    assert_eq!(model.route_pins("run"), [pin(0)]);
}

#[tokio::test]
async fn compactor_chain_overrides_the_role_chain_for_summaries() {
    // Given: a worker chain of route-0 → route-1 and a compactor chain of route-1 only.
    let (mut model, requests) = fixture_with(vec![None, None], &[None, None], &[]);
    model.agents.roles.compactor.models = vec![config::ChainEntryConfig {
        route: "route-1".into(),
        reasoning_effort: None,
    }];
    let history = [text(MessageRole::User, "work")];
    // When
    let summary = send(&model, event_bus::RequestPurpose::Compaction, &history).await;
    let reply = send(&model, event_bus::RequestPurpose::Agent, &history).await;
    // Then: only the summary used the compactor chain.
    assert_eq!((summary, reply), (Ok(response()), Ok(response())));
    assert_eq!(requests[0].lock().unwrap().len(), 1);
    assert_eq!(requests[1].lock().unwrap().len(), 1);
}

#[tokio::test]
async fn fallback_without_official_compaction_resummarizes_the_checkpoint_end_to_end() {
    fallback_resummarizes_checkpoint(false).await;
}

pub(super) async fn fallback_resummarizes_checkpoint(edit_switch: bool) {
    // Given: a replaying head with a 32K window that compacts once, then runs out of quota.
    let mut big = response();
    big.message.content = vec![ContentBlock::Text {
        text: "history ".repeat(16_000),
    }];
    let (mut model, _) = fixture_with(vec![None, None], &[], &[32_000, 1_000_000]);
    let head = QueueClient::install(
        &mut model,
        "profile-0",
        vec![
            Ok(big),
            Err(ProviderError::Http {
                status: 402,
                body: "quota exceeded".into(),
            }),
        ],
        true,
    );
    let next = QueueClient::install(&mut model, "profile-1", vec![], edit_switch);
    if edit_switch {
        model
            .providers
            .get_mut("profile-0")
            .unwrap()
            .profile
            .edit_tool
            .profile = Some(config::EditToolConfig::ApplyPatch);
    }
    // Run admission needs each profile's verified models, which the fixture leaves empty.
    model.verification = tokio::sync::OnceCell::new_with(Some(
        model
            .providers
            .iter()
            .map(|(name, provider)| (name.clone(), Ok(provider.profile.models.clone())))
            .collect(),
    ));
    let bus = Arc::new(EventBus::new(1024));
    let mut receiver = bus.subscribe();
    let executor = Arc::new(tools::ToolExecutor::with_standard_tools(
        bus.clone(),
        Arc::new(sandbox::DirectSandbox::new_unchecked()),
    ));
    let review_dir = tempfile::tempdir().unwrap();
    let review_config = storage::StorageConfig {
        db_path: review_dir.path().join("review.db"),
        ..Default::default()
    };
    let review_storage = storage::Storage::open(review_config.clone()).unwrap();
    let review_store = crate::RunStore::open(&review_config, review_storage.handle()).unwrap();
    let runtime = AgentRuntime::new(
        bus,
        executor,
        Arc::new(SwitchableModel::new(Arc::new(model))),
    )
    .with_run_store(review_store)
    .with_compaction(config::CompactionConfig {
        keep_recent_tokens: 1,
        ..config::CompactionConfig::default()
    });
    let run = runtime.delegate_background(
        Role::Worker,
        "retain the goal".into(),
        crate::RunConfig {
            interactive: true,
            ..crate::RunConfig::default()
        },
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !runtime
            .inspect_agent(run)
            .is_ok_and(|agent| agent.phase == crate::AgentRunPhase::Waiting)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // When: the next turn compacts officially on the head, which then fails over.
    runtime.send_message(run, "continue".into()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), runtime.wait(run))
        .await
        .unwrap()
        .unwrap();
    let mut reasons = Vec::new();
    // Events are emitted before the request goes out; the timeout only ends the drain.
    while let Ok(Ok(event)) =
        tokio::time::timeout(std::time::Duration::from_millis(10), receiver.recv()).await
    {
        if let event_bus::EventKind::Compaction(event_bus::CompactionEvent::Compacted {
            reason,
            ..
        }) = event.kind
        {
            reasons.push(reason);
        }
    }
    // Then: the head saw its blob, and the fallback member got a text checkpoint instead.
    let head = head.lock().unwrap();
    assert_eq!(head.len(), 2);
    assert!(carries_blob(&head[1]));
    let next = next.lock().unwrap();
    // One re-summary request and the resent turn.
    assert_eq!(next.len(), 2);
    assert!(next.iter().all(|request| !carries_blob(request)));
    let resent = next.last().unwrap();
    assert!(resent.messages.iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(block, ContentBlock::Text { text } if text.starts_with("[COMPACTION CHECKPOINT"))
        })
    }));
    assert_eq!(
        reasons.last(),
        Some(&event_bus::CompactionReason::ModelSwitch)
    );
}
