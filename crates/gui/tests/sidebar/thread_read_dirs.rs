//! Thread read-only directories (ADR 0037): each run-starting path declares them,
//! branches and escalations carry them, and other threads never get them.

use super::{MockSource, sidebar_with_project, state};
use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{EscalationMemoSummary, Event, LifecycleEvent, MessageEvent};
use gui::app::WorkbenchState;
use gui::model::commands::{CommandSink, LoopEvent, WorkbenchCommand};
use gui::model::composer::ProviderStatus;
use gui::panes::self_improvement::TRIAGE_BUTTON;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use storage::improvement::{
    ImprovementSeverity, ImprovementSource, ImprovementWritePolicy, NewImprovementCandidate,
};
use workspace_ui::{PanelId, ProjectId, SidebarState, ThreadId, UiSettings};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Sent {
    Declared(String, Vec<PathBuf>),
    Chat(String),
}

#[derive(Clone, Default)]
struct ReadDirsSink(Arc<Mutex<Vec<Sent>>>);

impl ReadDirsSink {
    fn take(&self) -> Vec<Sent> {
        std::mem::take(&mut *self.0.lock().expect("sent"))
    }
}

impl CommandSink for ReadDirsSink {
    fn set_thread_read_only_directories(&mut self, thread_id: String, directories: Vec<PathBuf>) {
        self.0
            .lock()
            .expect("sent")
            .push(Sent::Declared(thread_id, directories));
    }

    fn submit(&mut self, command: WorkbenchCommand) -> Vec<LoopEvent> {
        if let WorkbenchCommand::SendChat(chat) = command {
            self.0
                .lock()
                .expect("sent")
                .push(Sent::Chat(chat.thread_id));
        }
        Vec::new()
    }
}

struct Fixture {
    temp: tempfile::TempDir,
    repo: PathBuf,
    /// Canonical, as the sidebar stores it.
    drafts: PathBuf,
    save: PathBuf,
}

impl Fixture {
    fn dirs(&self) -> &[PathBuf] {
        std::slice::from_ref(&self.drafts)
    }
}

fn fixture() -> Fixture {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    let drafts = temp.path().join("drafts");
    for path in [&repo, &drafts] {
        std::fs::create_dir_all(path).expect("directory");
    }
    let save = temp.path().join("sidebar.json");
    Fixture {
        drafts: drafts.canonicalize().expect("canonical drafts"),
        repo,
        save,
        temp,
    }
}

/// A project with one ordinary thread, `other`, open.
fn sidebar(repo: &Path) -> SidebarState {
    let mut sidebar = sidebar_with_project(repo);
    sidebar
        .create_thread(ThreadId::new("other"), ProjectId::new("demo"), "Other")
        .expect("thread");
    sidebar
        .switch_thread(&ThreadId::new("other"))
        .expect("open");
    sidebar
}

fn workbench(fixture: &Fixture, sink: &ReadDirsSink) -> WorkbenchState<MockSource> {
    state(MockSource::default(), sidebar(&fixture.repo))
        .with_sidebar_path(fixture.save.clone())
        .with_command_sink(Box::new(sink.clone()))
}

fn thread<'a>(state: &'a WorkbenchState<MockSource>, id: &str) -> &'a workspace_ui::ThreadRecord {
    state
        .sidebar()
        .threads
        .iter()
        .find(|thread| thread.id.to_string() == id)
        .unwrap_or_else(|| panic!("thread {id}"))
}

fn declared(thread: &ThreadId, dirs: &[PathBuf]) -> Sent {
    Sent::Declared(thread.to_string(), dirs.to_vec())
}

#[test]
fn a_triage_thread_declares_its_directory_and_waits_with_the_prompt() {
    // Given: a project whose `other` thread has an unsent draft of its own.
    let fixture = fixture();
    let sink = ReadDirsSink::default();
    let mut workbench = workbench(&fixture, &sink);
    workbench.composer_mut().input = "other draft".into();

    // When: a thread that reads the drafts is created with a prompt.
    let id = workbench
        .create_thread_with_read_dirs(
            "Triage drafts (1 candidates, new)",
            vec![fixture.temp.path().join("drafts")],
            "triage prompt".into(),
        )
        .expect("triage thread");

    // Then: only the new thread is declared, with the directory's real path.
    assert_eq!(sink.take(), [declared(&id, fixture.dirs())]);
    // Then: it is open with the prompt waiting, keeps the role open, and is saved.
    assert_eq!(workbench.sidebar().active_thread.as_ref(), Some(&id));
    assert_eq!(workbench.composer().input, "triage prompt");
    let created = thread(&workbench, &id.to_string());
    assert_eq!(created.title, "Triage drafts (1 candidates, new)");
    assert_eq!(created.extra_read_dirs, fixture.dirs());
    assert_eq!(created.draft_input, "triage prompt");
    assert_eq!(created.chat_role, None);
    let saved = workspace_ui::load_sidebar(&fixture.save).expect("saved sidebar");
    let saved = saved.threads.iter().find(|thread| thread.id == id).unwrap();
    assert_eq!(saved.extra_read_dirs, fixture.dirs());
    assert_eq!(saved.draft_input, "triage prompt");
    // Then: the other thread gains nothing and keeps its own draft.
    let other = thread(&workbench, "other");
    assert!(other.extra_read_dirs.is_empty());
    assert_eq!(other.draft_input, "other draft");

    // When: the operator moves between the threads.
    workbench.switch_thread(ThreadId::new("other")).unwrap();
    workbench.switch_thread(id.clone()).unwrap();
    // Then: unchanged declarations are not sent again.
    assert!(sink.take().is_empty());
}

#[test]
fn rejected_directories_leave_no_thread_behind() {
    let fixture = fixture();
    let sink = ReadDirsSink::default();
    let mut workbench = workbench(&fixture, &sink);
    std::fs::create_dir(fixture.repo.join("drafts")).unwrap();
    let before = workbench.sidebar().threads.len();
    for dir in [
        fixture.temp.path().join("missing"),
        fixture.repo.clone(),
        fixture.repo.join("drafts"),
        fixture.repo.parent().unwrap().to_owned(),
        PathBuf::from("/"),
        PathBuf::from("relative"),
    ] {
        let result =
            workbench.create_thread_with_read_dirs("Triage", vec![dir.clone()], "p".into());
        assert!(result.is_err(), "{} must be rejected", dir.display());
        assert_eq!(workbench.sidebar().threads.len(), before);
        assert_eq!(
            workbench.sidebar().active_thread,
            Some(ThreadId::new("other"))
        );
        assert!(workbench.composer().input.is_empty());
    }
    assert!(sink.take().is_empty());
}

#[test]
fn a_fork_carries_the_directories_and_declares_them_for_itself() {
    let fixture = fixture();
    let sink = ReadDirsSink::default();
    let mut workbench = workbench(&fixture, &sink);
    let id = workbench
        .create_thread_with_read_dirs("Triage", vec![fixture.drafts.clone()], "p".into())
        .unwrap();
    sink.take();

    let fork = workbench.fork_thread(id.clone()).expect("fork");

    assert_eq!(
        thread(&workbench, &fork.to_string()).extra_read_dirs,
        fixture.dirs()
    );
    assert_eq!(sink.take(), [declared(&fork, fixture.dirs())]);
    // A plain thread forked stays without them.
    let plain = workbench.fork_thread(ThreadId::new("other")).expect("fork");
    assert!(
        thread(&workbench, &plain.to_string())
            .extra_read_dirs
            .is_empty()
    );
    assert!(sink.take().is_empty());
}

fn escalation(source: &str, new: &str) -> Event {
    Event::new(LifecycleEvent::EscalationRequested {
        source_run_id: source.into(),
        new_run_id: new.into(),
        summary: EscalationMemoSummary {
            original_request: "Triage the drafts".into(),
            escalation_reason: "Coordination required".into(),
            files_touched: Vec::new(),
            blockers: Vec::new(),
            suggested_next: "Group the candidates".into(),
        },
    })
}

fn started(run: &str, agent: &str) -> Event {
    Event::new(LifecycleEvent::AgentRunStarted {
        run_id: run.into(),
        parent_run_id: None,
        agent_name: agent.into(),
        role: "worker".into(),
    })
}

#[test]
fn a_rewind_carries_the_directories_and_declares_them_for_its_new_thread() {
    let fixture = fixture();
    let sink = ReadDirsSink::default();
    let mut workbench = workbench(&fixture, &sink);
    let id = workbench
        .create_thread_with_read_dirs("Triage", vec![fixture.drafts.clone()], "p".into())
        .unwrap();
    sink.take();

    workbench.apply_events([
        Event::new(LifecycleEvent::AgentRunStarted {
            run_id: "run-1".into(),
            parent_run_id: None,
            agent_name: format!("chat:Worker:{id}"),
            role: "worker".into(),
        }),
        Event::new(MessageEvent::MessageCompleted {
            run_id: "run-1".into(),
            text: "answer".into(),
        }),
        Event::new(LifecycleEvent::TurnCompleted {
            run_id: "run-1".into(),
            context_len: 2,
        }),
    ]);
    let turn = workbench
        .transcript()
        .entries()
        .iter()
        .position(|entry| {
            matches!(
                entry,
                gui::model::transcript::TranscriptEntry::TurnEnd { .. }
            )
        })
        .unwrap();

    let rewind = workbench.rewind_to_turn(id, turn).expect("rewind");

    assert_eq!(
        thread(&workbench, &rewind.to_string()).extra_read_dirs,
        fixture.dirs()
    );
    assert_eq!(sink.take(), [declared(&rewind, fixture.dirs())]);
}

#[test]
fn an_escalation_thread_copies_its_parent_directories() {
    let fixture = fixture();
    let sink = ReadDirsSink::default();
    let mut workbench = workbench(&fixture, &sink);
    let id = workbench
        .create_thread_with_read_dirs("Triage", vec![fixture.drafts.clone()], "p".into())
        .unwrap();
    sink.take();

    workbench.apply_events([
        started("run-1", &format!("chat:Worker:{id}")),
        escalation("run-1", "run-2"),
        started("run-3", "chat:Worker:other"),
        escalation("run-3", "run-4"),
    ]);

    let child = thread(&workbench, "escalation-run-2");
    assert_eq!(child.parent_thread_id.as_ref(), Some(&id));
    assert_eq!(child.extra_read_dirs, fixture.dirs());
    assert!(
        thread(&workbench, "escalation-run-4")
            .extra_read_dirs
            .is_empty()
    );
    let saved = workspace_ui::load_sidebar(&fixture.save).expect("saved sidebar");
    assert!(
        saved
            .threads
            .iter()
            .any(|thread| thread.id == ThreadId::new("escalation-run-2")
                && thread.extra_read_dirs == fixture.dirs())
    );
    // Folding events sends nothing; the next sync declares the child's own record.
    assert!(sink.take().is_empty());
    workbench
        .switch_thread(ThreadId::new("escalation-run-2"))
        .unwrap();
    assert_eq!(
        sink.take(),
        [Sent::Declared(
            "escalation-run-2".into(),
            vec![fixture.drafts.clone()]
        )]
    );
}

#[test]
fn a_restart_declares_saved_directories_before_the_first_send() {
    // Given: a session that created a triage thread and saved the sidebar.
    let fixture = fixture();
    let first = ReadDirsSink::default();
    let mut session = workbench(&fixture, &first);
    let id = session
        .create_thread_with_read_dirs("Triage", vec![fixture.drafts.clone()], "p".into())
        .unwrap();
    drop(session);
    let saved = workspace_ui::load_sidebar(&fixture.save).expect("saved sidebar");

    // When: the GUI starts as production does, the sink installed before the sidebar.
    let sink = ReadDirsSink::default();
    let _restored = WorkbenchState::new(MockSource::default(), &UiSettings::default())
        .unwrap()
        .with_command_sink(Box::new(sink.clone()))
        .with_sidebar(saved.clone());
    // Then: the saved record is declared at once, for that thread only.
    assert_eq!(sink.take(), [declared(&id, fixture.dirs())]);

    // When: a sink is installed after the sidebar and the restored draft is sent.
    let sink = ReadDirsSink::default();
    let mut restored = state(MockSource::default(), saved)
        .with_provider_status(ProviderStatus::Configured)
        .with_command_sink(Box::new(sink.clone()));
    assert_eq!(restored.composer().input, "p");
    restored.submit_composer();
    // Then: the declaration precedes the first send.
    assert_eq!(
        sink.take(),
        [declared(&id, fixture.dirs()), Sent::Chat(id.to_string())]
    );
}

#[test]
fn a_directory_that_disappears_is_withdrawn_and_returns_when_restored() {
    let fixture = fixture();
    let sink = ReadDirsSink::default();
    let mut workbench = workbench(&fixture, &sink);
    let id = workbench
        .create_thread_with_read_dirs("Triage", vec![fixture.drafts.clone()], "p".into())
        .unwrap();
    sink.take();

    std::fs::remove_dir(&fixture.drafts).unwrap();
    workbench.switch_thread(ThreadId::new("other")).unwrap();
    workbench.switch_thread(id.clone()).unwrap();
    // The record survives; the runtime only stops mounting it.
    assert_eq!(sink.take(), [declared(&id, &[])]);
    assert_eq!(
        thread(&workbench, &id.to_string()).extra_read_dirs,
        fixture.dirs()
    );

    std::fs::create_dir(&fixture.drafts).unwrap();
    workbench.switch_thread(ThreadId::new("other")).unwrap();
    assert_eq!(sink.take(), [declared(&id, fixture.dirs())]);
}

fn record_candidate(store: &storage::Storage, project: &str, id: &str) {
    store
        .handle()
        .record_improvement_candidate(
            project,
            NewImprovementCandidate {
                id: id.into(),
                source: ImprovementSource::Diagnostic,
                severity: ImprovementSeverity::Warning,
                code: "NoProgress".into(),
                title: format!("Candidate {id}"),
                evidence: "Repeated tools".into(),
                dedup_key: id.into(),
                run_id: None,
            },
            ImprovementWritePolicy {
                duplicate_cooldown: std::time::Duration::ZERO,
                daily_limit: 20,
                max_candidates: 200,
            },
        )
        .unwrap();
    store
        .handle()
        .attach_improvement_draft(id, &format!("{id}.md"))
        .unwrap();
}

/// A workbench showing the Self-improvement tab with one drafted candidate,
/// its storage kept outside the draft directory.
fn triage_workbench(
    fixture: &Fixture,
    sink: &ReadDirsSink,
) -> (WorkbenchState<MockSource>, storage::Storage) {
    let config = storage::StorageConfig {
        db_path: fixture.temp.path().join("store/store.db"),
        ..Default::default()
    };
    std::fs::create_dir_all(config.db_path.parent().unwrap()).unwrap();
    let store = storage::Storage::open(config.clone()).unwrap();
    let sidebar = sidebar(&fixture.repo);
    let slug = gui::runtime_sink::derive_repo_slug(&sidebar.projects[0].repo_root);
    record_candidate(&store, &slug, "c1");
    let mut workbench = state(MockSource::default(), sidebar)
        .with_sidebar_path(fixture.save.clone())
        .with_memory_storage(config)
        .with_self_improvement(store.handle(), true, Some(fixture.drafts.clone()))
        .with_command_sink(Box::new(sink.clone()));
    let tab = workbench
        .dock()
        .find_tab(&PanelId::new("self-improvement-main"))
        .unwrap();
    workbench.dock_mut().set_active_tab(tab).unwrap();
    (workbench, store)
}

#[test]
fn the_triage_button_opens_a_badged_thread_with_the_prompt() {
    // Given: a candidate with a draft, and storage outside the draft directory.
    let fixture = fixture();
    let sink = ReadDirsSink::default();
    let (workbench, _store) = triage_workbench(&fixture, &sink);
    let mut harness = Harness::builder()
        .with_size(egui::vec2(1400.0, 900.0))
        .build_ui_state(
            |ui, state: &mut WorkbenchState<MockSource>| {
                state.ui(ui, &mut eframe::Frame::_new_kittest())
            },
            workbench,
        );
    harness.run_steps(4);
    harness.get_by_label("Thread: Other");
    assert!(
        harness
            .query_by_label("Extra read-only directories")
            .is_none()
    );

    // When: the operator presses the triage button.
    harness.get_by_label(TRIAGE_BUTTON).click();
    harness.run_steps(4);

    // Then: a new thread reads the drafts and its composer holds the prompt.
    let state = harness.state();
    let id = state
        .sidebar()
        .active_thread
        .clone()
        .expect("active thread");
    let created = thread(state, &id.to_string());
    assert_eq!(created.title, "Triage drafts (1 candidates, new)");
    assert_eq!(created.extra_read_dirs, fixture.dirs());
    let draft = fixture.drafts.join("c1.md").display().to_string();
    assert!(state.composer().input.contains(&draft));
    assert_eq!(state.composer().input, created.draft_input);
    assert_eq!(sink.take(), [declared(&id, fixture.dirs())]);
    // Then: its header names the grant, and the tooltip lists the directory.
    harness.get_by_label("Thread: Triage drafts (1 candidates, new)");
    harness.get_by_label("Extra read-only directories").hover();
    harness.run_steps(3);
    harness.get_by_label_contains(&fixture.drafts.display().to_string());

    // When: the operator returns to the other thread, the badge goes with it.
    harness
        .state_mut()
        .switch_thread(ThreadId::new("other"))
        .unwrap();
    harness.run_steps(4);
    harness.get_by_label("Thread: Other");
    assert!(
        harness
            .query_by_label("Extra read-only directories")
            .is_none()
    );
}

#[test]
#[ignore = "writes PNG review evidence using an offscreen GPU adapter"]
fn capture_triage_button_and_badge() {
    let fixture = fixture();
    let sink = ReadDirsSink::default();
    let (workbench, _store) = triage_workbench(&fixture, &sink);
    let mut harness = gui::headless::HeadlessWorkbench::new(workbench, [1400.0, 900.0]);
    harness.run();
    let directory =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/gui-evidence/thread-read-dirs");
    std::fs::create_dir_all(&directory).expect("evidence directory");
    if let Some(frame) = gui::evidence::capture_or_skip(&mut harness) {
        frame
            .save_png(&directory.join("triage-button.png"))
            .expect("button PNG");
    }
    harness.click_label(TRIAGE_BUTTON);
    harness.run();
    assert!(harness.has_label("Extra read-only directories"));
    if let Some(frame) = gui::evidence::capture_or_skip(&mut harness) {
        frame
            .save_png(&directory.join("triage-thread-badge.png"))
            .expect("badge PNG");
    }
}

#[test]
fn restarting_with_database_moved_inside_saved_drafts_omits_the_grant_and_badge() {
    let fixture = fixture();
    let sink = ReadDirsSink::default();
    let mut first = workbench(&fixture, &sink);
    let id = first
        .create_thread_with_read_dirs("Triage", vec![fixture.drafts.clone()], "p".into())
        .unwrap();
    drop(first);
    sink.take();
    let saved = workspace_ui::load_sidebar(&fixture.save).unwrap();
    let config = storage::StorageConfig {
        db_path: fixture.drafts.join("db/store.db"),
        ..Default::default()
    };
    let session = state(MockSource::default(), saved)
        .with_memory_storage(config)
        .with_command_sink(Box::new(sink.clone()));
    let mut harness = Harness::builder()
        .with_size(egui::vec2(1400.0, 900.0))
        .build_ui_state(
            |ui, state: &mut WorkbenchState<MockSource>| {
                state.ui(ui, &mut eframe::Frame::_new_kittest())
            },
            session,
        );
    harness.run_steps(4);
    assert!(sink.take().is_empty());
    assert_eq!(
        thread(harness.state(), &id.to_string()).extra_read_dirs,
        fixture.dirs(),
        "keep the saved declaration, not the grant"
    );
    assert!(
        harness
            .query_by_label("Extra read-only directories")
            .is_none()
    );
}

#[test]
fn installing_storage_after_sidebar_withdraws_a_now_protected_directory() {
    let fixture = fixture();
    let sink = ReadDirsSink::default();
    let mut session = workbench(&fixture, &sink);
    let id = session
        .create_thread_with_read_dirs("Triage", vec![fixture.drafts.clone()], "p".into())
        .unwrap();
    assert_eq!(sink.take(), [declared(&id, fixture.dirs())]);
    let config = storage::StorageConfig {
        db_path: fixture.drafts.join("candidate.db"),
        ..Default::default()
    };
    let _session = session.with_memory_storage(config);
    assert_eq!(sink.take(), [declared(&id, &[])]);
}
