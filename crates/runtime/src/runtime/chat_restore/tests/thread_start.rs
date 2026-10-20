use super::*;
use crate::ChatForkSeed;
use crate::thread_start::tests::git;
use storage::thread_start::{ThreadStartCapture, ThreadStartState};

/// Edits the project on every request, as an agent's first tool call would.
struct EditingModel {
    project: PathBuf,
}

#[async_trait::async_trait]
impl AgentModel for EditingModel {
    async fn complete(
        &self,
        _: &crate::AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        std::fs::write(self.project.join("agent-edit.txt"), "edited").unwrap();
        std::fs::write(self.project.join("tracked.txt"), "edited\n").unwrap();
        Ok(providers::ChatResponse {
            message: providers::Message {
                role: providers::Role::Assistant,
                content: vec![providers::ContentBlock::Text {
                    text: "done".into(),
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

struct Project {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

/// A committed repository with one pre-existing untracked note.
fn project() -> Project {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("project")).unwrap();
    let root = dir.path().join("project").canonicalize().unwrap();
    git(&root, &["init", "--quiet"]);
    std::fs::write(root.join("tracked.txt"), "one\n").unwrap();
    git(&root, &["add", "tracked.txt"]);
    git(&root, &["commit", "--quiet", "-m", "initial"]);
    std::fs::write(root.join("notes.txt"), "before the thread\n").unwrap();
    Project { _dir: dir, root }
}

fn database(fixture: &Fixture) -> storage::Database {
    storage::Database::open(&storage::StorageConfig {
        db_path: fixture._dir.path().join("goal.sqlite3"),
        ..storage::StorageConfig::default()
    })
    .unwrap()
}

fn recorded(fixture: &Fixture, thread_id: &str) -> Option<ThreadStartState> {
    database(fixture).thread_start_state(thread_id).unwrap()
}

async fn turn_completed(events: &mut event_bus::EventReceiver, run: RunId) -> u64 {
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

fn in_project(project: &Project, config: RunConfig) -> RunConfig {
    RunConfig {
        project_root: Some(project.root.clone()),
        ..config
    }
}

#[tokio::test]
async fn new_thread_records_its_start_before_the_first_request_and_keeps_it() {
    // Given: a committed project and a model that edits it on its first request.
    let project = project();
    let fixture = Fixture::with_model(Arc::new(EditingModel {
        project: project.root.clone(),
    }));

    // When: a new thread runs its first root run to completion.
    let first = fixture
        .runtime
        .delegate_chat(
            "thread",
            Role::Worker,
            "first".into(),
            in_project(&project, RunConfig::default()),
        )
        .unwrap();
    fixture.runtime.wait(first).await.unwrap();

    // Then: exactly one start is recorded, from before the model's edit.
    let start = recorded(&fixture, "thread").unwrap();
    assert_eq!(start.root_run_id, first.to_string());
    assert_eq!(start.project_root, project.root.to_string_lossy());
    let ThreadStartCapture::Replayable(snapshot) = &start.capture else {
        panic!("expected a replayable start, got {:?}", start.capture);
    };
    assert!(snapshot.diff.is_empty());
    assert_eq!(
        snapshot
            .untracked
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>(),
        ["notes.txt"]
    );
    assert_eq!(
        database(&fixture)
            .thread_start_states_since(0)
            .unwrap()
            .len(),
        1
    );

    // When: the thread continues with another run.
    let next = fixture
        .runtime
        .delegate_chat(
            "thread",
            Role::Worker,
            "second".into(),
            in_project(&project, RunConfig::default()),
        )
        .unwrap();
    fixture.runtime.wait(next).await.unwrap();

    // Then: the continuation keeps the original start.
    assert_ne!(next, first);
    assert_eq!(recorded(&fixture, "thread"), Some(start));
}

#[tokio::test]
async fn fork_seeded_thread_records_no_start() {
    // Given: a live source chat in a project that completed one turn.
    let project = project();
    let fixture = Fixture::with_model(Arc::new(EditingModel {
        project: project.root.clone(),
    }));
    let chat = || {
        in_project(
            &project,
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..RunConfig::default()
            },
        )
    };
    let mut events = fixture.runtime.shared.bus.subscribe();
    let source = fixture
        .runtime
        .delegate_chat("source", Role::Worker, "first".into(), chat())
        .unwrap();
    let context_len = turn_completed(&mut events, source).await;

    // When: a new thread starts from the source's history.
    let fork = fixture
        .runtime
        .delegate_chat_seeded(
            "fork",
            Role::Worker,
            "branch".into(),
            chat(),
            Some(ChatForkSeed {
                source_run_id: source.to_string(),
                context_len,
            }),
        )
        .unwrap();
    turn_completed(&mut events, fork).await;

    // Then: only the source thread has a recorded start.
    assert!(recorded(&fixture, "source").is_some());
    assert_eq!(recorded(&fixture, "fork"), None);
}
