use super::*;
use providers::{ChatResponse, ContentBlock, Message, ToolSpec};

/// Replies with large, compressible text so that saved snapshots cross the
/// compression threshold, and records every request that is not a compaction.
#[derive(Default)]
struct BigModel {
    requests: Mutex<Vec<Vec<Message>>>,
}

fn big_text(label: &str) -> String {
    (0..80)
        .map(|line| {
            format!(
                "{label} line {line}: the quick brown fox jumps over {}\n",
                line * 7919 % 1009
            )
        })
        .collect()
}

fn text_message(role: providers::Role, text: String) -> Message {
    Message {
        role,
        content: vec![ContentBlock::Text { text }],
    }
}

fn texts(messages: &[Message]) -> Vec<&str> {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[async_trait::async_trait]
impl AgentModel for BigModel {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test".into()
    }

    async fn complete(
        &self,
        invocation: &crate::AgentInvocationContext,
        _: Role,
        messages: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let reply = if invocation.purpose == event_bus::RequestPurpose::Compaction {
            big_text("summary")
        } else {
            self.requests.lock().unwrap().push(messages.to_vec());
            big_text("answer")
        };
        Ok(ChatResponse {
            usage_reported: true,
            message: text_message(providers::Role::Assistant, reply),
            finish_reason: providers::FinishReason::Stop,
            usage: providers::Usage::default(),
        })
    }
}

async fn wait_for_waiting(runtime: &AgentRuntime, run: RunId) {
    runtime.wait_admission(run).await.unwrap();
    let mut phase = runtime.entry(run).unwrap().phase_rx.clone();
    loop {
        if *phase.borrow_and_update() == AgentRunPhase::Waiting {
            return;
        }
        phase.changed().await.unwrap();
    }
}

/// How `messages_json` and `checkpoints_json` of the run are stored.
fn storage_classes(fixture: &Fixture, run: RunId) -> (String, String) {
    rusqlite::Connection::open(fixture._dir.path().join("goal.sqlite3"))
        .unwrap()
        .query_row(
            "SELECT typeof(messages_json), typeof(checkpoints_json) FROM run_contexts \
             WHERE run_id = ?1",
            [run.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

#[tokio::test]
async fn compressed_snapshots_resume_compact_and_resume_again_with_an_exact_wire_prefix() {
    // Given: a stopped chat whose saved history is large enough to be stored compressed.
    let model = Arc::new(BigModel::default());
    let mut fixture = Fixture::with_model(model.clone());
    fixture.runtime = fixture.runtime.with_compaction(config::CompactionConfig {
        context_window_tokens: 1_000_000,
        keep_recent_tokens: 1,
        max_summary_bytes: 16_384,
        ..Default::default()
    });
    let prompt = big_text("prompt");
    let run = fixture.runtime.delegate_background(
        Role::Worker,
        prompt.clone(),
        RunConfig {
            name: Some("goal-1".into()),
            ..RunConfig::default()
        },
    );
    fixture.runtime.wait(run).await.unwrap();
    fixture
        .runtime
        .continue_goal(run, "second turn".into(), RunConfig::default())
        .unwrap();
    wait_for_waiting(&fixture.runtime, run).await;
    fixture.runtime.stop(run, StopScope::SelfOnly).unwrap();
    assert_eq!(
        fixture.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Stopped
    );

    // Then: the restore of the compressed history sent the saved turns byte for byte.
    assert_eq!(storage_classes(&fixture, run).0, "blob");
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let restored = texts(&requests[1]);
        let at = |wanted: &dyn Fn(&str) -> bool| restored.iter().position(|text| wanted(text));
        let prompt_at = at(&|text| text == prompt).expect("saved prompt is replayed exactly");
        let answer_at =
            at(&|text| text == big_text("answer")).expect("saved answer is replayed exactly");
        let follow_up_at = at(&|text| text.contains("second turn")).expect("new turn is appended");
        assert!(prompt_at < answer_at && answer_at < follow_up_at);
    }

    // When: the stopped chat is compacted, which saves checkpoints next to the messages.
    fixture
        .runtime
        .compact_saved_chat(run, RunConfig::default())
        .await
        .unwrap();

    // Then: both columns are compressed and the checkpoint summary reads back exactly.
    assert_eq!(
        storage_classes(&fixture, run),
        ("blob".into(), "blob".into())
    );
    let store = fixture.runtime.shared.run_store.get().unwrap();
    let bundle = store.restore_bundle(run).unwrap().unwrap();
    assert!(
        bundle
            .record
            .checkpoints_json
            .contains(big_text("summary").lines().next().unwrap())
    );
    let indexed = storage::thread_index::extract_snapshot_bundle(
        &bundle,
        &secret_guard::SecretRedactor::from_env(),
    )
    .unwrap();
    assert!(!indexed.blocks.is_empty());

    // And: resuming after the compaction replays the checkpoint summary.
    fixture
        .runtime
        .resume_chat(
            run,
            RunConfig {
                interactive: true,
                keep_alive: false,
                ..Default::default()
            },
        )
        .unwrap();
    wait_for_waiting(&fixture.runtime, run).await;
    let mut phase = fixture.runtime.entry(run).unwrap().phase_rx.clone();
    phase.borrow_and_update();
    fixture
        .runtime
        .send_message(run, "continue now".into())
        .unwrap();
    loop {
        phase.changed().await.unwrap();
        if *phase.borrow_and_update() == AgentRunPhase::Waiting {
            break;
        }
    }
    let requests = model.requests.lock().unwrap();
    let request = texts(requests.last().unwrap());
    assert!(request.iter().any(
        |text| text.contains("[COMPACTION CHECKPOINT ") && text.contains(&big_text("summary"))
    ));
    assert!(request.iter().any(|text| text.contains("continue now")));
}

#[tokio::test]
async fn a_fork_reads_and_indexes_the_compressed_snapshot_of_its_source() {
    // Given: a live source chat with two large turns, saved as a compressed snapshot.
    let model = Arc::new(BigModel::default());
    let fixture = Fixture::with_model(model.clone());
    let config = RunConfig {
        interactive: true,
        keep_alive: true,
        ..RunConfig::default()
    };
    let mut events = fixture.runtime.shared.bus.subscribe();
    let first_prompt = big_text("first");
    let source = fixture
        .runtime
        .delegate_chat("source", Role::Worker, first_prompt.clone(), config.clone())
        .unwrap();
    let first = next_turn(&mut events, source).await;
    fixture
        .runtime
        .send_message(source, big_text("second"))
        .unwrap();
    let second = next_turn(&mut events, source).await;
    assert!(first < second);
    assert_eq!(storage_classes(&fixture, source).0, "blob");

    // When: a new thread starts from the first turn boundary.
    let fork = fixture
        .runtime
        .delegate_chat_seeded(
            "fork",
            Role::Worker,
            "branch".into(),
            config,
            Some(crate::ChatForkSeed {
                source_run_id: source.to_string(),
                context_len: first,
            }),
        )
        .unwrap();
    next_turn(&mut events, fork).await;

    // Then: the fork's model saw the source's first turn exactly and never the second.
    let requests = model.requests.lock().unwrap();
    let request = texts(requests.last().unwrap());
    assert!(request.iter().any(|text| *text == first_prompt));
    assert!(request.contains(&"branch"));
    assert!(!request.iter().any(|text| text.contains("second line")));
    drop(requests);

    // And: the thread index extracts blocks from the fork's own snapshot only.
    let store = fixture.runtime.shared.run_store.get().unwrap();
    let bundle = store.restore_bundle(fork).unwrap().unwrap();
    let indexed = storage::thread_index::extract_snapshot_bundle(
        &bundle,
        &secret_guard::SecretRedactor::from_env(),
    )
    .unwrap();
    assert!(indexed.blocks.iter().any(|block| block.text == "branch"));
    assert!(
        !indexed
            .blocks
            .iter()
            .any(|block| block.text == first_prompt)
    );
}

async fn next_turn(events: &mut event_bus::EventReceiver, run: RunId) -> u64 {
    loop {
        let event = events.recv().await.expect("event bus open");
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
