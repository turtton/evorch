use super::*;
use crate::ChatForkSeed;

/// Replies with the user text it was asked about and records every request.
#[derive(Default)]
struct EchoModel {
    requests: std::sync::Mutex<Vec<Vec<providers::Message>>>,
}

fn user_texts(messages: &[providers::Message]) -> Vec<String> {
    messages
        .iter()
        .filter(|message| message.role == providers::Role::User)
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            providers::ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[async_trait::async_trait]
impl AgentModel for EchoModel {
    async fn complete(
        &self,
        _: &crate::AgentInvocationContext,
        _: Role,
        messages: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        self.requests.lock().unwrap().push(messages.to_vec());
        let last = user_texts(messages).pop().unwrap_or_default();
        Ok(providers::ChatResponse {
            message: providers::Message {
                role: providers::Role::Assistant,
                content: vec![providers::ContentBlock::Text {
                    text: format!("re: {last}"),
                }],
            },
            finish_reason: providers::FinishReason::Stop,
            usage: providers::Usage::default(),
        })
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test".into()
    }
}

fn chat_config() -> RunConfig {
    RunConfig {
        interactive: true,
        keep_alive: true,
        ..RunConfig::default()
    }
}

async fn next_turn(events: &mut event_bus::EventReceiver, run: RunId) -> u64 {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("turn completion")
            .expect("event bus open");
        if let event_bus::EventKind::Lifecycle(LifecycleEvent::TurnCompleted {
            run_id,
            context_len,
        }) = event.kind
            && run_id == run.to_string()
        {
            return context_len;
        }
    }
}

/// Two completed turns on a live source chat, returning their boundaries.
async fn two_turns(fixture: &Fixture) -> (RunId, u64, u64) {
    let mut events = fixture.runtime.shared.bus.subscribe();
    let source = fixture
        .runtime
        .delegate_chat("source", Role::Worker, "first".into(), chat_config())
        .unwrap();
    let first = next_turn(&mut events, source).await;
    fixture
        .runtime
        .send_message(source, "second".into())
        .unwrap();
    let second = next_turn(&mut events, source).await;
    (source, first, second)
}

#[tokio::test]
async fn fork_seed_replays_history_only_up_to_the_completed_turn() {
    // Given: a live source chat that completed two turns.
    let model = Arc::new(EchoModel::default());
    let fixture = Fixture::with_model(model.clone());
    let (source, first, second) = two_turns(&fixture).await;
    assert!(first < second);
    // When: a new thread starts from the first turn boundary while the source still waits.
    let mut events = fixture.runtime.shared.bus.subscribe();
    let fork = fixture
        .runtime
        .delegate_chat_seeded(
            "fork",
            Role::Worker,
            "branch".into(),
            chat_config(),
            Some(ChatForkSeed {
                source_run_id: source.to_string(),
                context_len: first,
            }),
        )
        .unwrap();
    let store = fixture.runtime.shared.run_store.get().unwrap();
    assert!(
        store
            .review_lineage_edges(&fork.to_string())
            .unwrap()
            .is_empty()
    );
    let review = crate::run_store::materialize_review_context(store, fork).unwrap();
    assert_eq!(
        review
            .requests()
            .0
            .iter()
            .map(|request| request.text.as_str())
            .collect::<Vec<_>>(),
        vec!["branch"]
    );
    assert_eq!(review.lineage_run_ids, vec![fork.to_string()]);

    next_turn(&mut events, fork).await;
    // Then: the model sees the first turn and the new prompt, never the later turn.
    let request = model.requests.lock().unwrap().last().unwrap().clone();
    let texts = user_texts(&request);
    assert!(texts.iter().any(|text| text == "first"));
    assert!(texts.iter().any(|text| text == "branch"));
    assert!(!texts.iter().any(|text| text == "second"));
    let bundle = store.restore_bundle(fork).unwrap().unwrap();
    let indexed = storage::thread_index::extract_snapshot_bundle(
        &bundle,
        &secret_guard::SecretRedactor::from_env(),
    )
    .unwrap();
    assert!(indexed.blocks.iter().any(|block| block.text == "branch"));
    assert!(!indexed.blocks.iter().any(|block| block.text == "first"));
    assert!(
        indexed
            .blocks
            .iter()
            .filter_map(|block| match block.locator {
                storage::thread_index::BlockLocator::Message { ordinal, .. } => Some(ordinal),
                _ => None,
            })
            .all(|ordinal| ordinal >= first as usize)
    );
    assert_eq!(
        *fixture.runtime.entry(source).unwrap().phase_rx.borrow(),
        AgentRunPhase::Waiting
    );
}

#[tokio::test]
async fn fork_seed_rejects_a_boundary_outside_saved_history() {
    // Given: a source chat with two completed turns.
    let fixture = Fixture::with_model(Arc::new(EchoModel::default()));
    let (source, _, second) = two_turns(&fixture).await;
    // When: the seed names a boundary past the saved conversation.
    let result = fixture.runtime.delegate_chat_seeded(
        "fork",
        Role::Worker,
        "branch".into(),
        chat_config(),
        Some(ChatForkSeed {
            source_run_id: source.to_string(),
            context_len: second + 1,
        }),
    );
    // Then: the fork fails closed instead of starting from a guessed history.
    assert!(matches!(
        result,
        Err(RuntimeError::RunRestoreFailed {
            reason: RunRestoreFailure::CorruptContext(_),
            ..
        })
    ));
}

#[tokio::test]
async fn fork_seed_is_ignored_once_the_thread_has_its_own_history() {
    // Given: a fork that already saved its own first turn.
    let model = Arc::new(EchoModel::default());
    let fixture = Fixture::with_model(model.clone());
    let (source, first, _) = two_turns(&fixture).await;
    let seed = ChatForkSeed {
        source_run_id: source.to_string(),
        context_len: first,
    };
    let mut events = fixture.runtime.shared.bus.subscribe();
    let fork = fixture
        .runtime
        .delegate_chat_seeded(
            "fork",
            Role::Worker,
            "branch".into(),
            chat_config(),
            Some(seed.clone()),
        )
        .unwrap();
    next_turn(&mut events, fork).await;
    for run in [source, fork] {
        fixture.runtime.stop(run, StopScope::SelfOnly).unwrap();
        fixture.runtime.wait(run).await.unwrap();
    }
    // When: the next chat start for the thread still carries the seed.
    let next = fixture
        .runtime
        .delegate_chat_seeded(
            "fork",
            Role::Worker,
            "again".into(),
            chat_config(),
            Some(seed),
        )
        .unwrap();
    next_turn(&mut events, next).await;
    // Then: its own saved branch history is restored, not the original seed boundary.
    let request = model.requests.lock().unwrap().last().unwrap().clone();
    let texts = user_texts(&request);
    assert!(texts.iter().any(|text| text == "branch"));
    assert!(texts.iter().any(|text| text == "again"));
}

#[tokio::test]
async fn fork_seed_restates_the_current_allowed_directories() {
    // Given: a source chat told one allowed directory, told a second one in its later turn
    let model = Arc::new(EchoModel::default());
    let fixture = Fixture::with_model(model.clone());
    let project = tempfile::tempdir().unwrap();
    let root = project.path().canonicalize().unwrap();
    let (first_dir, second_dir) = (root.join("first"), root.join("second"));
    for dir in [&first_dir, &second_dir] {
        std::fs::create_dir(dir).unwrap();
    }
    let runtime = fixture
        .runtime
        .clone()
        .with_project_rules(Arc::new(crate::RulesSource::new(
            crate::ProjectTrust::Approved,
            crate::RulesSettings::from(&config::RulesConfig::default()),
            None,
            Some(root.clone()),
            None,
        )));
    runtime.set_project_allowed_directories(root.clone(), vec![first_dir.clone()]);
    let mut events = fixture.runtime.shared.bus.subscribe();
    let source = runtime
        .delegate_chat("source", Role::Worker, "first".into(), chat_config())
        .unwrap();
    let first = next_turn(&mut events, source).await;
    runtime
        .set_project_allowed_directories(root.clone(), vec![first_dir.clone(), second_dir.clone()]);
    runtime.send_message(source, "second".into()).unwrap();
    next_turn(&mut events, source).await;
    let source_texts = user_texts(&model.requests.lock().unwrap().last().unwrap().clone());
    assert!(
        source_texts
            .iter()
            .any(|text| text.contains("+ ") && text.contains("second"))
    );

    // When: a new thread forks at the first turn, before that update
    let fork = runtime
        .delegate_chat_seeded(
            "fork",
            Role::Worker,
            "branch".into(),
            chat_config(),
            Some(ChatForkSeed {
                source_run_id: source.to_string(),
                context_len: first,
            }),
        )
        .unwrap();
    next_turn(&mut events, fork).await;

    // Then: it is told the whole current list once instead of a difference it never saw
    let texts = user_texts(&model.requests.lock().unwrap().last().unwrap().clone());
    let updates: Vec<_> = texts
        .iter()
        .filter(|text| text.starts_with("[workspace update]"))
        .collect();
    assert_eq!(updates.len(), 1, "{texts:?}");
    assert!(updates[0].contains("Current allowed directories"));
    for dir in [&first_dir, &second_dir] {
        assert!(updates[0].contains(&format!("* {} (read-write)", dir.display())));
    }
}
