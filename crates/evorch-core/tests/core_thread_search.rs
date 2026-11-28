//! `search_threads` through the production core: `HostBackend::request` over a
//! `BootstrappedHost` with a real run store. Only the model is scripted.
//!
//! A conversation the test starts is a real thread with a saved run, so the
//! search is the same one the GUI's modal reaches. The search is one page per
//! request, runs outside the host lock, and its cursor is the runtime's own
//! text, unchanged by the output guard.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use event_bus::{AgentRunPhase, Event, EventKind, EventReceiver, LifecycleEvent};
use evorch_core::backend::HostBackend;
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec};
use runtime::{AgentInvocationContext, AgentModel, Role, RuntimeError};
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
use workbench_host::provenance::local_desktop;
use workbench_protocol::commands::{Command, ComposerRole, SendChat};
use workbench_protocol::envelope::{
    Provenance, ReplyPayload, ReplyResult, RequestBody, RequestEnvelope, Surface,
};
use workbench_protocol::errors::{ErrorCode, WorkbenchError};
use workbench_protocol::handshake::WireLimits;
use workbench_protocol::ids::{ClientId, ProjectId, RequestId, SessionId, SubmissionId, ThreadId};
use workbench_protocol::namespaces::workspace::*;
use workbench_protocol::namespaces::{ConversationCommand, WorkspaceQuery, WorkspaceReply};
use workbench_protocol::queries::{Query, QueryReply};
use workspace_state::{ProjectId as StateProject, SidebarDocument, SidebarStore};

struct Idle;

#[async_trait::async_trait]
impl AgentModel for Idle {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        Ok(ChatResponse {
            usage_reported: true,
            message: Message {
                role: providers::Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "noted".into(),
                }],
            },
            usage: Default::default(),
            finish_reason: FinishReason::Stop,
        })
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "core-thread-search".into()
    }
}

struct Unisolated;

impl runtime::SandboxFactory for Unisolated {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}

struct World {
    dir: test_socket_dir::SplitDir,
    root: PathBuf,
    core: BootstrappedHost,
    backend: Arc<HostBackend>,
    events: EventReceiver,
    submissions: u32,
}

/// Project `p` (`repo`), listed in the sidebar.
async fn world() -> World {
    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path().canonicalize().unwrap();
    for name in ["repo", "config"] {
        std::fs::create_dir_all(root.join(name)).unwrap();
    }
    let store = SidebarStore::new(root.join("config/sidebar.json"));
    let mut document = SidebarDocument::default();
    document
        .sidebar
        .add_project(StateProject::new("p"), "Repo", &root.join("repo"))
        .unwrap();
    document.sidebar.refresh_derived();
    {
        let lock = store.lock().unwrap();
        store.write(&lock, &document).unwrap();
    }
    let core = BootstrappedHost::open_with_test_seams(
        BootstrapOptions {
            config_dir: root.join("config"),
            project_root: root.join("repo"),
            ownership_root: dir.sockets().to_path_buf(),
        },
        TestSeams {
            model: Arc::new(Idle),
            sandbox: Arc::new(Unisolated),
        },
    )
    .await
    .unwrap();
    let backend = Arc::new(HostBackend::new(core.host.clone(), "thread-search".into()));
    let events = core.runtime.event_bus().subscribe();
    World {
        dir,
        root,
        core,
        backend,
        events,
        submissions: 0,
    }
}

impl World {
    /// Starts a conversation and returns once its run is waiting for the next
    /// message, so its saved records are complete.
    async fn start(&mut self, project: &str, thread: &str, text: &str) {
        self.submissions += 1;
        let send = SendChat {
            project: ProjectId::new(project).unwrap(),
            thread: ThreadId::new(thread).unwrap(),
            submission_id: SubmissionId::new(format!("sub-{}", self.submissions)).unwrap(),
            composer_role: ComposerRole::Worker,
            text: text.into(),
            skills: vec![],
            attachments: vec![],
            model_preference: None,
            fork_seed: None,
        };
        let run = {
            let mut host = self.core.host.lock().unwrap();
            host.reserve_new_chat_thread(&send).unwrap();
            host.handle_command(
                Command::Conversation(ConversationCommand::SendChat(send)),
                local_desktop(),
            )
            .unwrap()
            .run_id
            .expect("a run")
        };
        loop {
            let ev = self.events.recv().await.unwrap();
            let Event {
                kind: EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. }),
                ..
            } = ev
            else {
                continue;
            };
            if run_id == run.as_str() {
                assert!(
                    !matches!(
                        to,
                        AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped
                    ),
                    "run ended as {to:?}"
                );
                if to == AgentRunPhase::Waiting {
                    break;
                }
            }
        }
    }

    fn request(&self, project: &str, query: &str, cursor: Option<&str>) -> RequestEnvelope {
        RequestEnvelope {
            request_id: RequestId::new("search").unwrap(),
            session_id: SessionId::new("session").unwrap(),
            body: RequestBody::Query(Query::Workspace(WorkspaceQuery::SearchThreads(
                SearchThreads {
                    project: ProjectId::new(project).unwrap(),
                    query: query.into(),
                    cursor: cursor.map(|c| ThreadSearchCursor::new(c).unwrap()),
                },
            ))),
        }
    }

    fn provenance() -> Provenance {
        Provenance::human(Surface::Cli, ClientId::new("client").unwrap())
    }

    /// One page through the production request path, after the output guard.
    fn search(
        &self,
        project: &str,
        query: &str,
        cursor: Option<&str>,
    ) -> Result<ThreadSearchResults, WorkbenchError> {
        let reply = self
            .backend
            .request(self.request(project, query, cursor), Self::provenance());
        match reply.result {
            ReplyResult::Success(ReplyPayload::Query(QueryReply::Workspace(
                WorkspaceReply::ThreadSearchResults(results),
            ))) => {
                results.validate().unwrap();
                Ok(results)
            }
            ReplyResult::Error(error) => Err(error),
            other => panic!("{other:?}"),
        }
    }

    fn runtime_page(&self, project: &str, query: &str, cursor: Option<&str>) -> Option<String> {
        assert_eq!(project, "p");
        let root = self.root.join("repo");
        self.core
            .runtime
            .search_threads_for_host(&root, query, 20, cursor)
            .unwrap()
            .next_cursor
    }

    async fn finish(mut self) {
        self.core.shutdown().await.unwrap();
        drop(self.dir);
    }
}

fn ids(results: &ThreadSearchResults) -> Vec<&str> {
    results.hits.iter().map(|h| h.thread.as_str()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_word_only_in_a_body_finds_the_thread_of_that_project() {
    let mut w = world().await;
    w.start("p", "plan", "Plan the release\nthe rabbit hutch is red")
        .await;
    w.start("p", "other-topic", "Something else\nnothing relevant")
        .await;

    let results = w.search("p", "rabbit", None).unwrap();
    assert_eq!(ids(&results), ["plan"], "{results:?}");
    let hit = &results.hits[0];
    assert!(!hit.archived);
    assert_eq!(hit.title.as_deref(), Some("Plan the release"));
    let snippet = hit.snippet.as_deref().expect("a snippet");
    assert!(snippet.contains("rabbit"), "{snippet}");
    // A body page says what the index covered.
    assert!(results.index.is_some());
    w.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn titles_come_first_and_a_title_only_page_says_nothing_about_the_index() {
    let mut w = world().await;
    for index in 0..21 {
        w.start(
            "p",
            &format!("t{index:02}"),
            &format!("Release note {index}\nbody {index}"),
        )
        .await;
    }
    // Titles match: the first page is full of title hits and ends in a title
    // continuation. It searched no body, so it reports no index state.
    let first = w.search("p", "release", None).unwrap();
    assert_eq!(first.hits.len(), 20, "{first:?}");
    assert!(first.next_cursor.is_some());
    assert_eq!(first.index, None, "{first:?}");
    assert!(
        first
            .hits
            .iter()
            .all(|h| h.snippet_kind.as_deref() == Some("title"))
    );
    // The next page continues from the cursor the core relayed, and reaches bodies.
    let second = w
        .search(
            "p",
            "release",
            first.next_cursor.as_ref().map(|c| c.as_str()),
        )
        .unwrap();
    assert!(!ids(&second).is_empty());
    w.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_body_cursor_survives_the_output_guard_and_continues_the_search() {
    let mut w = world().await;
    for index in 0..21 {
        w.start(
            "p",
            &format!("t{index:02}"),
            &format!("Thread {index}\nthe carrot number {index}"),
        )
        .await;
    }
    let first = w.search("p", "carrot", None).unwrap();
    assert_eq!(first.hits.len(), 20, "{first:?}");
    assert!(first.index.is_some(), "a body page reports the index");
    let cursor = first.next_cursor.as_ref().expect("a second page remains");
    // Byte for byte the cursor the runtime produced: the output guard left it alone.
    assert_eq!(
        Some(cursor.as_str().to_owned()),
        w.runtime_page("p", "carrot", None)
    );
    let second = w.search("p", "carrot", Some(cursor.as_str())).unwrap();
    assert_eq!(second.hits.len(), 1, "{second:?}");
    assert_eq!(second.next_cursor, None);
    let mut all: Vec<&str> = ids(&first).into_iter().chain(ids(&second)).collect();
    all.sort_unstable();
    all.dedup();
    assert_eq!(all.len(), 21, "every thread once: {all:?}");
    w.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cursor_the_runtime_refuses_is_a_conflict_to_search_again_from_the_start() {
    let mut w = world().await;
    for index in 0..21 {
        w.start(
            "p",
            &format!("t{index:02}"),
            &format!("Thread {index}\nthe carrot number {index}"),
        )
        .await;
    }
    let first = w.search("p", "carrot", None).unwrap();
    let cursor = first.next_cursor.unwrap();
    let conflict = |result: Result<ThreadSearchResults, WorkbenchError>| {
        assert_eq!(result.unwrap_err().code, ErrorCode::Conflict);
    };
    // Not a cursor at all, a title cursor that is not a number, a cursor of
    // another query.
    conflict(w.search("p", "carrot", Some("garbage")));
    conflict(w.search("p", "carrot", Some("host:titles:x")));
    conflict(w.search("p", "number", Some(cursor.as_str())));
    // Searching again from the start works.
    assert_eq!(w.search("p", "carrot", None).unwrap().hits.len(), 20);
    w.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_projects_and_bad_queries_are_structured_errors() {
    let w = world().await;
    assert_eq!(
        w.search("nope", "x", None).unwrap_err().code,
        ErrorCode::NotFound
    );
    let long = "a".repeat(1025);
    let reply = w
        .backend
        .request(w.request("p", &long, None), World::provenance());
    let ReplyResult::Error(error) = reply.result else {
        panic!("{reply:?}")
    };
    assert_eq!(error.code, ErrorCode::LimitExceeded);
    let reply = w
        .backend
        .request(w.request("p", "a\nb", None), World::provenance());
    let ReplyResult::Error(error) = reply.result else {
        panic!("{reply:?}")
    };
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    // A blank query lists nothing yet but is a valid request.
    assert!(w.search("p", "", None).unwrap().hits.is_empty());
    w.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prepared_search_runs_while_the_host_lock_is_held() {
    let mut w = world().await;
    w.start("p", "plan", "Plan the release\nthe rabbit hutch")
        .await;
    let query = Query::Workspace(WorkspaceQuery::SearchThreads(SearchThreads {
        project: ProjectId::new("p").unwrap(),
        query: "rabbit".into(),
        cursor: None,
    }));
    let job = w
        .core
        .host
        .lock()
        .unwrap()
        .prepare_thread_search(&query, &World::provenance())
        .expect("a search")
        .unwrap();
    // The job borrows nothing from the host: with the lock held by this
    // thread, another thread runs it to completion.
    let host = w.core.host.lock().unwrap();
    let reply = std::thread::scope(|scope| scope.spawn(move || job.run()).join().unwrap());
    drop(host);
    let QueryReply::Workspace(WorkspaceReply::ThreadSearchResults(results)) = reply.unwrap() else {
        panic!("not a search reply")
    };
    assert_eq!(ids(&results), ["plan"]);
    // The query path under the lock refuses a search rather than run it there.
    let refused = w.core.host.lock().unwrap().handle_query(
        &SessionId::new("session").unwrap(),
        &query,
        &World::provenance(),
    );
    assert_eq!(refused.unwrap_err().code, ErrorCode::Internal);
    w.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn no_read_transaction_outlives_a_page() {
    let mut w = world().await;
    for index in 0..21 {
        w.start(
            "p",
            &format!("t{index:02}"),
            &format!("Thread {index}\nthe carrot number {index}"),
        )
        .await;
    }
    let db = find_database(&w.root.join("config"));
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection.busy_timeout(std::time::Duration::ZERO).unwrap();
    // After each page a checkpoint that truncates the WAL is not blocked: the
    // search left no statement or transaction pinning a snapshot.
    let checkpoint = || -> i64 {
        connection
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))
            .unwrap()
    };
    let first = w.search("p", "carrot", None).unwrap();
    assert_eq!(checkpoint(), 0, "the first page left a reader behind");
    let cursor = first.next_cursor.unwrap();
    let second = w.search("p", "carrot", Some(cursor.as_str())).unwrap();
    assert_eq!(second.hits.len(), 1);
    assert_eq!(checkpoint(), 0, "the second page left a reader behind");
    w.finish().await;
}

fn find_database(dir: &Path) -> PathBuf {
    fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, found);
            } else if path.extension().is_some_and(|e| e == "db")
                && path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().contains("evorch"))
            {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    walk(dir, &mut found);
    found.sort();
    found
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("no database under {}", dir.display()))
}

#[test]
fn the_limits_are_the_protocols() {
    // The page size the host asks the runtime for is the protocol's.
    assert_eq!(THREAD_SEARCH_PAGE_SIZE, 20);
    let _ = WireLimits::default();
}
