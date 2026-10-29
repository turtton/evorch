//! Follow-ups sent while a run is busy wait above the composer, oldest first,
//! and join the transcript only once the runtime reports them delivered.
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::headless::HeadlessWorkbench;
use gui::model::commands::{CommandSink, LoopEvent, WorkbenchCommand};
use gui::model::composer::ProviderStatus;
use gui::model::transcript::TranscriptEntry;
use runtime::FollowUpStatus;
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

/// Simulates one live run's inbox; `None` means the thread has no run.
#[derive(Clone, Default)]
struct InboxSink(
    Arc<Mutex<Option<FollowUpStatus>>>,
    Arc<Mutex<HashSet<String>>>,
);

impl InboxSink {
    fn update(&self, change: impl FnOnce(&mut FollowUpStatus)) {
        change(self.0.lock().unwrap().as_mut().expect("live run"));
    }
}

impl CommandSink for InboxSink {
    fn follow_up_status(&self, _thread: &str) -> Option<FollowUpStatus> {
        *self.0.lock().unwrap()
    }

    fn withdraw_follow_up(
        &mut self,
        _thread: &str,
        prompt_id: &str,
    ) -> Result<(), runtime::RuntimeError> {
        let mut status = self.0.lock().unwrap();
        let inbox = status
            .as_mut()
            .ok_or(runtime::FollowUpWithdrawalError::Closed)?;
        if inbox.pending == 0 || !self.1.lock().unwrap().remove(prompt_id) {
            return Err(runtime::FollowUpWithdrawalError::AlreadyConsumed.into());
        }
        inbox.pending -= 1;
        inbox.next_turn_requested &= inbox.pending > 0;
        Ok(())
    }

    fn submit(&mut self, command: WorkbenchCommand) -> Vec<LoopEvent> {
        let mut status = self.0.lock().unwrap();
        match command {
            WorkbenchCommand::SendChat(chat) => {
                match status.as_mut() {
                    Some(inbox) if !inbox.closed => {
                        inbox.pending += 1;
                        self.1.lock().unwrap().insert(chat.submission_id.clone());
                    }
                    _ => *status = Some(FollowUpStatus::default()),
                }
                vec![LoopEvent::ChatAccepted {
                    thread_id: chat.thread_id,
                    run_id: "root".into(),
                }]
            }
            WorkbenchCommand::DeliverFollowUpsNextTurn { .. } => {
                if let Some(inbox) = status.as_mut() {
                    inbox.next_turn_requested = inbox.pending > 0;
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
}

fn workbench(root: &std::path::Path, sink: InboxSink) -> HeadlessWorkbench<DemoSource> {
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
    let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_provider_status(ProviderStatus::Configured)
        .with_command_sink(Box::new(sink));
    HeadlessWorkbench::new(state, [1200.0, 900.0])
}

fn send(gui: &mut HeadlessWorkbench<DemoSource>, text: &str) {
    gui.state_mut().composer_mut().input = text.into();
    gui.state_mut().submit_composer();
    gui.run();
}

fn user_messages(gui: &HeadlessWorkbench<DemoSource>) -> Vec<String> {
    gui.state()
        .transcripts()
        .thread()
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::UserMessage { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn capture(gui: &mut HeadlessWorkbench<DemoSource>, name: &str) {
    if let Some(directory) = std::env::var_os("FOLLOW_UP_QUEUE_CAPTURE") {
        gui.capture()
            .unwrap()
            .save_png(&std::path::PathBuf::from(directory).join(format!("{name}.png")))
            .unwrap();
    }
}

#[test]
fn busy_run_queues_follow_ups_above_composer_in_delivery_order() {
    let temp = tempfile::tempdir().unwrap();
    let sink = InboxSink::default();
    let mut gui = workbench(temp.path(), sink.clone());
    // Given: the first message starts a run, so it goes straight to the transcript.
    send(&mut gui, "start the task");
    assert_eq!(user_messages(&gui), ["start the task"]);
    // When: two follow-ups arrive while that run is busy.
    send(&mut gui, "also check the docs");
    send(&mut gui, "and run the tests");
    // Then: they wait above the composer, oldest first, outside the transcript.
    assert_eq!(user_messages(&gui), ["start the task"]);
    let first = gui.label_rects("Queued: also check the docs")[0];
    let second = gui.label_rects("Queued: and run the tests")[0];
    let composer = gui.label_rects("Message or /command")[0];
    assert!(first.max.y <= second.min.y && second.max.y <= composer.min.y);
    // Only the next message carries the control, and nothing else explains the queue.
    assert_eq!(gui.count_labels("Send sooner"), 1);
    assert!(!gui.has_label("Deliver next turn"));
    capture(&mut gui, "queued");

    gui.click_label("Send sooner");
    gui.run();
    assert_eq!(
        gui.state().issued().last(),
        Some(&WorkbenchCommand::DeliverFollowUpsNextTurn {
            thread_id: "thread".into(),
        })
    );
    assert!(!gui.has_label("Send sooner"));
    assert!(gui.has_label("Sending after the current tools"));

    // When: the runtime consumes the oldest one.
    sink.update(|inbox| inbox.pending = 1);
    gui.run();
    // Then: it joins the transcript and the next one moves up.
    assert_eq!(
        user_messages(&gui),
        ["start the task", "also check the docs"]
    );
    assert!(!gui.has_label("Queued: also check the docs"));
    assert!(gui.has_label("Queued: and run the tests"));

    sink.update(|inbox| {
        inbox.pending = 0;
        inbox.next_turn_requested = false;
    });
    gui.run();
    assert_eq!(
        user_messages(&gui),
        ["start the task", "also check the docs", "and run the tests"]
    );
    assert!(!gui.has_label("Queued: and run the tests"));
}

#[test]
fn follow_ups_dropped_by_a_finished_run_can_be_edited_or_dismissed() {
    let temp = tempfile::tempdir().unwrap();
    let sink = InboxSink::default();
    let mut gui = workbench(temp.path(), sink.clone());
    send(&mut gui, "start the task");
    send(&mut gui, "first follow-up");
    send(&mut gui, "second follow-up");
    // When: the run ends before reading either.
    sink.update(|inbox| inbox.closed = true);
    gui.run();
    // Then: both stay visible as undelivered instead of silently disappearing.
    assert!(gui.has_label("Not delivered: first follow-up"));
    assert!(gui.has_label("Not delivered: second follow-up"));
    assert!(!gui.has_label("Send sooner"));
    assert_eq!(user_messages(&gui), ["start the task"]);
    capture(&mut gui, "undelivered");

    gui.state_mut().composer_mut().input = "draft".into();
    gui.click_label("Edit in composer: first follow-up");
    gui.run();
    assert_eq!(gui.state().composer().input, "draft\n\nfirst follow-up");
    assert!(!gui.has_label("Not delivered: first follow-up"));

    gui.click_label("Dismiss: second follow-up");
    gui.run();
    assert!(!gui.has_label("Not delivered: second follow-up"));
    assert_eq!(user_messages(&gui), ["start the task"]);
}

#[test]
fn live_queue_edit_and_delete_are_distinct_and_preserve_the_other_rows() {
    let temp = tempfile::tempdir().unwrap();
    let sink = InboxSink::default();
    let mut gui = workbench(temp.path(), sink.clone());
    send(&mut gui, "start");
    send(&mut gui, "keep queued");
    let image = gui::model::composer::ImageAttachment {
        media_type: "image/png".into(),
        data: "aW1hZ2U=".into(),
    };
    gui.state_mut()
        .composer_mut()
        .attachments
        .push(image.clone());
    gui.state_mut().composer_mut().image_input_supported = true;
    send(&mut gui, "edit queued");
    send(&mut gui, "delete queued");
    assert!(gui.has_label("Edit: keep queued"));
    assert!(gui.has_label("Delete: edit queued"));
    gui.click_label("Send sooner");
    gui.run();
    gui.state_mut().composer_mut().input = "current draft".into();
    gui.click_label("Edit: edit queued");
    gui.run();
    assert_eq!(gui.state().composer().input, "current draft\n\nedit queued");
    assert_eq!(gui.state().composer().attachments, [image]);
    assert_eq!(gui.state().composer().images_needed, 0);
    assert!(!gui.has_label("Queued: edit queued"));
    assert!(!gui.has_label("Not delivered: edit queued"));
    assert!(gui.has_label("Queued: keep queued"));
    assert_eq!(sink.0.lock().unwrap().unwrap().pending, 2);
    gui.click_label("Delete: delete queued");
    gui.run();
    assert!(!gui.has_label("Queued: delete queued"));
    assert!(!gui.has_label("Not delivered: delete queued"));
    assert!(gui.has_label("Queued: keep queued"));
    assert_eq!(gui.state().composer().input, "current draft\n\nedit queued");
    assert_eq!(sink.0.lock().unwrap().unwrap().pending, 1);
    assert_eq!(user_messages(&gui), ["start"]);
}
