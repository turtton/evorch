//! A chat the runtime refuses never reaches the agent. It must not look sent,
//! now or after a restart, and the user's text must not be lost.
use std::path::Path;
use std::sync::{Arc, Mutex};

use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::headless::HeadlessWorkbench;
use gui::model::commands::{CommandSink, LoopEvent, WorkbenchCommand};
use gui::model::composer::ProviderStatus;
use gui::model::transcript::TranscriptEntry;
use runtime::FollowUpStatus;
use storage::{Database, Storage, StorageConfig};
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

/// Refuses every chat while a refusal is set. Otherwise the first chat starts
/// a run and later ones wait in that run's inbox.
#[derive(Clone, Default)]
struct FakeRuntime {
    refusal: Arc<Mutex<Option<String>>>,
    inbox: Arc<Mutex<Option<FollowUpStatus>>>,
}

impl FakeRuntime {
    fn refuse(&self, reason: &str) {
        *self.refusal.lock().unwrap() = Some(reason.into());
    }

    fn accept(&self) {
        *self.refusal.lock().unwrap() = None;
    }
}

impl CommandSink for FakeRuntime {
    fn follow_up_status(&self, _thread: &str) -> Option<FollowUpStatus> {
        *self.inbox.lock().unwrap()
    }

    fn submit(&mut self, command: WorkbenchCommand) -> Vec<LoopEvent> {
        let WorkbenchCommand::SendChat(chat) = command else {
            return Vec::new();
        };
        if let Some(reason) = self.refusal.lock().unwrap().clone() {
            return vec![LoopEvent::ChatRejected {
                thread_id: chat.thread_id,
                reason,
            }];
        }
        let mut inbox = self.inbox.lock().unwrap();
        match inbox.as_mut() {
            Some(inbox) if !inbox.closed => inbox.pending += 1,
            _ => *inbox = Some(FollowUpStatus::default()),
        }
        vec![LoopEvent::ChatAccepted {
            thread_id: chat.thread_id,
            run_id: "root".into(),
        }]
    }
}

fn state(root: &Path, runtime: &FakeRuntime) -> WorkbenchState<DemoSource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "Project", root)
        .unwrap();
    sidebar.select_project(&project).unwrap();
    sidebar
        .create_thread(ThreadId::new("thread"), project, "Thread")
        .unwrap();
    sidebar.switch_thread(&ThreadId::new("thread")).unwrap();
    WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_sidebar_path(root.join("sidebar.json"))
        .with_provider_status(ProviderStatus::Configured)
        .with_command_sink(Box::new(runtime.clone()))
}

fn workbench(root: &Path, runtime: &FakeRuntime) -> HeadlessWorkbench<DemoSource> {
    HeadlessWorkbench::new(state(root, runtime), [1200.0, 900.0])
}

/// A new GUI process on the same files; no run survived the restart.
fn reopen(root: &Path) -> HeadlessWorkbench<DemoSource> {
    let config = StorageConfig {
        db_path: root.join("events.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let sidebar = workspace_ui::load_sidebar(&root.join("sidebar.json")).unwrap();
    let mut state = state(root, &FakeRuntime::default()).with_sidebar(sidebar);
    state
        .restore_history(&Database::open(&config).unwrap())
        .unwrap();
    drop(storage);
    let mut gui = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    gui.run();
    gui
}

fn send(gui: &mut HeadlessWorkbench<DemoSource>, text: &str) {
    gui.state_mut().composer_mut().input = text.into();
    gui.state_mut().submit_composer();
    gui.run();
}

fn last_submission_id(gui: &HeadlessWorkbench<DemoSource>) -> String {
    gui.state()
        .issued()
        .iter()
        .rev()
        .find_map(|command| match command {
            WorkbenchCommand::SendChat(chat) => Some(chat.submission_id.clone()),
            _ => None,
        })
        .expect("a chat was sent")
}

fn user_messages(gui: &HeadlessWorkbench<DemoSource>) -> Vec<String> {
    gui.state()
        .transcripts()
        .thread()
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::UserMessage { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn saved_messages(root: &Path) -> Vec<serde_json::Value> {
    let bytes = std::fs::read(root.join("sidebar.json")).unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    saved["user_messages"].as_array().unwrap().clone()
}

fn saved_message(root: &Path, text: &str) -> serde_json::Value {
    saved_messages(root)
        .into_iter()
        .find(|message| message["text"] == text)
        .unwrap_or_else(|| panic!("{text:?} is not saved"))
}

#[test]
fn refused_chat_goes_back_to_the_composer_instead_of_the_transcript() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = FakeRuntime::default();
    let mut gui = workbench(temp.path(), &runtime);
    // Given: the runtime refuses chats, and the message carries an image.
    runtime.refuse("provider unavailable");
    let composer = gui.state_mut().composer_mut();
    composer.image_input_supported = true;
    assert!(composer.add_pasted_image("data:image/png;base64,aGVsbG8="));
    // When: it is sent.
    send(&mut gui, "hello agent");
    // Then: nothing in the conversation claims it was sent, and the composer
    // still holds the text and the image, next to the reason.
    assert!(user_messages(&gui).is_empty());
    assert!(!gui.has_label("You: hello agent"));
    assert!(!gui.has_label("Not delivered: hello agent"));
    assert!(gui.has_label("chat failed: provider unavailable"));
    assert_eq!(gui.state().composer().input, "hello agent");
    assert_eq!(gui.state().composer().attachments.len(), 1);
    let saved = saved_message(temp.path(), "hello agent");
    assert_eq!(saved["id"], last_submission_id(&gui).as_str());
    assert_eq!(
        saved["status"],
        serde_json::json!({"state": "rejected", "reason": "provider unavailable"})
    );
    assert!(saved["resolution"]["moved_to_composer"]["draft_id"].is_string());

    // When: the runtime accepts it on the next try.
    runtime.accept();
    gui.state_mut().submit_composer();
    gui.run();
    // Then: it is shown and saved as sent, like any accepted message.
    assert_eq!(user_messages(&gui), ["hello agent"]);
    assert!(gui.has_label("You: hello agent"));
    assert!(gui.state().composer().input.is_empty());
    assert!(gui.state().composer().attachments.is_empty());
    let sent = saved_messages(temp.path());
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1]["id"], last_submission_id(&gui).as_str());
    assert_eq!(sent[1]["status"], serde_json::json!({"state": "delivered"}));
    assert!(sent[1].get("resolution").is_none());
}

#[test]
fn refused_follow_up_to_a_busy_run_is_not_shown_as_sent() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = FakeRuntime::default();
    let mut gui = workbench(temp.path(), &runtime);
    // Given: a busy run whose inbox now refuses messages.
    send(&mut gui, "start the task");
    runtime.refuse("the run no longer takes follow-ups");
    // When: a follow-up is sent.
    send(&mut gui, "also check the docs");
    // Then: it neither joins the transcript nor waits in the queue.
    assert_eq!(user_messages(&gui), ["start the task"]);
    assert!(!gui.has_label("Queued: also check the docs"));
    assert!(!gui.has_label("Not delivered: also check the docs"));
    assert!(gui.has_label("chat failed: the run no longer takes follow-ups"));
    assert_eq!(gui.state().composer().input, "also check the docs");
}

#[test]
fn late_refusal_fills_an_empty_composer() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = FakeRuntime::default();
    let mut gui = workbench(temp.path(), &runtime);
    // Given: a follow-up waiting for a busy run, and an empty composer.
    send(&mut gui, "start the task");
    send(&mut gui, "also check the docs");
    assert!(gui.has_label("Queued: also check the docs"));
    let id = last_submission_id(&gui);
    // When: the runtime refuses it after the composer moved on.
    assert!(
        gui.state_mut()
            .reject_submission(&id, "the run was replaced")
    );
    gui.run();
    // Then: its text is back in the composer and it no longer waits.
    assert_eq!(gui.state().composer().input, "also check the docs");
    assert!(!gui.has_label("Queued: also check the docs"));
    assert!(!gui.has_label("Not delivered: also check the docs"));
    assert_eq!(user_messages(&gui), ["start the task"]);
    assert!(saved_message(temp.path(), "also check the docs")["resolution"]["moved_to_composer"]["draft_id"].is_string());
    // A verdict arrives once.
    assert!(!gui.state_mut().reject_submission(&id, "again"));
}

#[test]
fn late_refusal_keeps_a_newer_draft_and_offers_the_message_above_the_composer() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = FakeRuntime::default();
    let mut gui = workbench(temp.path(), &runtime);
    // Given: a follow-up waiting for a busy run, and a newer draft.
    send(&mut gui, "start the task");
    send(&mut gui, "also check the docs");
    let id = last_submission_id(&gui);
    gui.state_mut().composer_mut().input = "a newer draft".into();
    // When: the runtime refuses the follow-up.
    assert!(
        gui.state_mut()
            .reject_submission(&id, "the run was replaced")
    );
    gui.run();
    // Then: the draft stays, and the refused message waits above the composer.
    assert_eq!(gui.state().composer().input, "a newer draft");
    assert!(gui.has_label("Not delivered: also check the docs"));
    assert!(!gui.has_label("Queued: also check the docs"));
    assert_eq!(user_messages(&gui), ["start the task"]);
    assert!(
        saved_message(temp.path(), "also check the docs")
            .get("resolution")
            .is_none()
    );

    // When: the user edits it back in.
    gui.click_label("Edit in composer: also check the docs");
    gui.run();
    // Then: it joins the draft instead of replacing it, and is settled.
    assert_eq!(
        gui.state().composer().input,
        "a newer draft\n\nalso check the docs"
    );
    assert!(!gui.has_label("Not delivered: also check the docs"));
    assert!(saved_message(temp.path(), "also check the docs")["resolution"]["moved_to_composer"]["draft_id"].is_string());
}

#[test]
fn refused_messages_are_not_replayed_as_sent_after_restart() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = FakeRuntime::default();
    let mut gui = workbench(temp.path(), &runtime);
    // Given: one accepted message, one refused while sending, and one refused
    // later while another draft was in the composer.
    send(&mut gui, "hello agent");
    runtime.refuse("provider unavailable");
    send(&mut gui, "refused while sending");
    runtime.accept();
    send(&mut gui, "refused later");
    let late = last_submission_id(&gui);
    gui.state_mut().composer_mut().input = "a newer draft".into();
    assert!(
        gui.state_mut()
            .reject_submission(&late, "the run was replaced")
    );
    gui.run();
    drop(gui);
    let statuses: Vec<_> = saved_messages(temp.path())
        .iter()
        .map(|message| message["status"]["state"].clone())
        .collect();
    assert_eq!(statuses, ["delivered", "rejected", "rejected"]);
    // When: the GUI restarts.
    let gui = reopen(temp.path());
    // Then: only the accepted message is a sent bubble. The one never dealt
    // with is offered again above the composer.
    assert_eq!(user_messages(&gui), ["hello agent"]);
    assert!(gui.has_label("You: hello agent"));
    assert!(!gui.has_label("You: refused while sending"));
    assert!(!gui.has_label("You: refused later"));
    assert!(!gui.has_label("Not delivered: refused while sending"));
    assert!(gui.has_label("Not delivered: refused later"));
}

#[test]
fn dismissed_refusal_stays_hidden_after_restart() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = FakeRuntime::default();
    let mut gui = workbench(temp.path(), &runtime);
    send(&mut gui, "start the task");
    send(&mut gui, "also check the docs");
    let id = last_submission_id(&gui);
    gui.state_mut().composer_mut().input = "a newer draft".into();
    assert!(
        gui.state_mut()
            .reject_submission(&id, "the run was replaced")
    );
    gui.run();
    gui.click_label("Dismiss: also check the docs");
    gui.run();
    assert_eq!(
        saved_message(temp.path(), "also check the docs")["resolution"],
        "dismissed"
    );
    drop(gui);
    let gui = reopen(temp.path());
    assert_eq!(user_messages(&gui), ["start the task"]);
    assert!(!gui.has_label("Not delivered: also check the docs"));
}

#[test]
fn sidebar_saved_before_delivery_states_replays_messages_as_sent() {
    let temp = tempfile::tempdir().unwrap();
    // Given: a sidebar file whose messages predate IDs, prompts and statuses.
    let original = state(temp.path(), &FakeRuntime::default());
    original.save_sidebar();
    drop(original);
    let path = temp.path().join("sidebar.json");
    let mut saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    saved["user_messages"] = serde_json::json!([{
        "thread_id": "thread",
        "text": "from an older version",
        "at": {"secs_since_epoch": 1_700_000_000, "nanos_since_epoch": 0},
    }]);
    std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
    // When: the GUI restores it.
    let gui = reopen(temp.path());
    // Then: the message is shown as sent, as before.
    assert_eq!(user_messages(&gui), ["from an older version"]);
    assert!(gui.has_label("You: from an older version"));
    assert!(!gui.has_label("Not delivered: from an older version"));
}

#[test]
fn offline_compaction_preserves_legacy_sidebar_input_between_root_deltas() {
    use event_bus::{Event, LifecycleEvent, MessageEvent};
    use std::time::{Duration, UNIX_EPOCH};
    use storage::maintenance::{CompactOptions, compact_events};

    for reasoning in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let original = state(temp.path(), &FakeRuntime::default());
        original.save_sidebar();
        drop(original);
        let sidebar = temp.path().join("sidebar.json");
        let mut saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&sidebar).unwrap()).unwrap();
        saved["user_messages"] = serde_json::json!([{
            "thread_id": "thread", "text": "legacy input",
            "at": {"secs_since_epoch": 150, "nanos_since_epoch": 0}
        }]);
        let sidebar_bytes = serde_json::to_vec(&saved).unwrap();
        std::fs::write(&sidebar, &sidebar_bytes).unwrap();
        let path = temp.path().join("events.db");
        let storage = Storage::open(StorageConfig {
            db_path: path.clone(),
            ..Default::default()
        })
        .unwrap();
        let mut started = Event::new(LifecycleEvent::AgentRunStarted {
            run_id: "root".into(),
            parent_run_id: None,
            agent_name: "chat:thread".into(),
            role: "orchestrator".into(),
        });
        started.meta.wall_clock = UNIX_EPOCH + Duration::from_secs(99);
        storage.handle().append_event(None, &started).unwrap();
        for (at, text) in [
            (100, "before "),
            (110, "input"),
            (200, "after "),
            (210, "input"),
        ] {
            let mut event = Event::new(if reasoning {
                MessageEvent::ReasoningDelta {
                    run_id: Some("root".into()),
                    delta: text.into(),
                }
            } else {
                MessageEvent::MessageDelta {
                    run_id: Some("root".into()),
                    delta: text.into(),
                }
            });
            event.meta.wall_clock = UNIX_EPOCH + Duration::from_secs(at);
            storage.handle().append_event(None, &event).unwrap();
        }
        storage.close();
        let restored = reopen(temp.path());
        let before_thread = restored.state().transcripts().thread().entries().to_vec();
        let before_run = restored
            .state()
            .transcripts()
            .run("root")
            .unwrap()
            .entries()
            .to_vec();
        let visible: Vec<_> = before_thread
            .iter()
            .filter_map(|entry| match entry {
                TranscriptEntry::Reasoning { text, .. } | TranscriptEntry::Message { text, .. } => {
                    Some(text.as_str())
                }
                TranscriptEntry::UserMessage { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(visible, ["before input", "legacy input", "after input"]);
        drop(restored);
        let options = CompactOptions {
            sidebar: Some(sidebar.clone()),
            provider_retention_days: 0,
            batch_rows: 2,
            ..Default::default()
        };
        let preview = compact_events(&path, &options).unwrap();
        assert!(preview.delta_aggregation_enabled);
        assert_eq!(preview.sidebar_boundary_count, 1);
        for _ in 0..2 {
            let result = compact_events(
                &path,
                &CompactOptions {
                    apply: true,
                    ..options.clone()
                },
            )
            .unwrap();
            assert_eq!(preview.projected_event_rows, result.projected_event_rows);
            let restored = reopen(temp.path());
            assert_eq!(
                before_thread,
                restored.state().transcripts().thread().entries()
            );
            assert_eq!(
                before_run,
                restored
                    .state()
                    .transcripts()
                    .run("root")
                    .unwrap()
                    .entries()
            );
            assert_eq!(sidebar_bytes, std::fs::read(&sidebar).unwrap());
        }
    }
}
