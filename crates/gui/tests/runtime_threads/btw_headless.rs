use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use gui::app::WorkbenchState;
use gui::app::btw::BtwStatus;
use gui::fixture::DemoSource;
use gui::headless::HeadlessWorkbench;
use gui::model::commands::{
    BtwCancel, BtwJob, BtwOutcome, BtwStartError, CommandSink, LoopEvent, WorkbenchCommand,
};
use gui::model::composer::ProviderStatus;
use gui::model::transcript::TranscriptEntry;
use runtime::btw::{BtwAnswer, BtwError, BtwSource};
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

#[derive(Default)]
struct Script {
    asked: Mutex<Vec<(String, String)>>,
    senders: Mutex<Vec<Sender<BtwOutcome>>>,
    submitted: Mutex<Vec<WorkbenchCommand>>,
    cancelled: AtomicUsize,
    refuse: Mutex<Option<BtwStartError>>,
}

struct Stub(Arc<Script>);

impl CommandSink for Stub {
    fn start_btw(&self, thread: &str, question: String) -> Result<BtwJob, BtwStartError> {
        if let Some(error) = self.0.refuse.lock().unwrap().clone() {
            return Err(error);
        }
        self.0
            .asked
            .lock()
            .unwrap()
            .push((thread.to_owned(), question));
        let (sender, receiver) = std::sync::mpsc::channel();
        self.0.senders.lock().unwrap().push(sender);
        let script = Arc::clone(&self.0);
        Ok(BtwJob {
            source: source(true),
            receiver,
            cancel: BtwCancel::new(move || {
                script.cancelled.fetch_add(1, Ordering::SeqCst);
            }),
        })
    }

    fn submit(&mut self, command: WorkbenchCommand) -> Vec<LoopEvent> {
        self.0.submitted.lock().unwrap().push(command);
        Vec::new()
    }
}

fn source(settled: bool) -> BtwSource {
    BtwSource {
        run_id: "run-1".into(),
        settled,
    }
}

fn answer(text: &str) -> BtwOutcome {
    Ok(BtwAnswer {
        text: text.into(),
        source: source(true),
    })
}

fn workbench(root: &std::path::Path) -> (HeadlessWorkbench<DemoSource>, Arc<Script>) {
    let script = Arc::new(Script::default());
    let mut sidebar = SidebarState::default();
    let project_id = ProjectId::new("demo");
    sidebar
        .add_project(project_id.clone(), "demo", root)
        .unwrap();
    sidebar.select_project(&project_id).unwrap();
    sidebar
        .create_thread(ThreadId::new("thread-1"), project_id, "thread-1")
        .unwrap();
    sidebar.switch_thread(&ThreadId::new("thread-1")).unwrap();
    let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_provider_status(ProviderStatus::Configured)
        .with_command_sink(Box::new(Stub(Arc::clone(&script))));
    (HeadlessWorkbench::new(state, [1200.0, 900.0]), script)
}

fn submit(harness: &mut HeadlessWorkbench<DemoSource>, input: &str) {
    harness.state_mut().composer_mut().input = input.into();
    harness.state_mut().submit_composer();
    harness.run();
}

fn transcript_text(harness: &HeadlessWorkbench<DemoSource>) -> String {
    harness
        .state()
        .transcripts()
        .thread()
        .entries()
        .iter()
        .map(|entry| format!("{entry:?}"))
        .collect()
}

#[test]
fn btw_uses_the_dedicated_sink_call_and_shows_a_window_not_a_chat_turn() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, script) = workbench(temp.path());
    submit(&mut harness, "/btw  what is the cwd? ");
    assert_eq!(
        *script.asked.lock().unwrap(),
        [("thread-1".to_owned(), "what is the cwd?".to_owned())]
    );
    assert!(script.submitted.lock().unwrap().is_empty());
    assert!(harness.state().issued().is_empty());
    assert!(harness.state().composer().input.is_empty());
    let view = harness.state().btw_view().expect("window open");
    assert_eq!(view.status, BtwStatus::Running);
    assert_eq!(view.question, "what is the cwd?");
    assert!(harness.has_label("Side question"));
    assert!(harness.has_label("Cancel"));
    assert!(!transcript_text(&harness).contains("what is the cwd?"));
}

#[test]
fn btw_answer_stays_in_the_window_and_out_of_the_conversation() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, script) = workbench(temp.path());
    submit(&mut harness, "/btw why");
    script.senders.lock().unwrap()[0]
        .send(answer("because of reasons"))
        .unwrap();
    harness.run();
    let view = harness.state().btw_view().unwrap();
    assert_eq!(
        view.status,
        BtwStatus::Answered {
            text: "because of reasons".into()
        }
    );
    assert!(harness.has_label("because of reasons"));
    assert!(harness.has_label("Dismiss"));
    let transcript = transcript_text(&harness);
    assert!(!transcript.contains("because of reasons"));
    assert!(!transcript.contains("why"));
    assert!(harness.state().issued().is_empty());

    harness.click_label("Dismiss");
    harness.run();
    assert!(harness.state().btw_view().is_none());
}

#[test]
fn btw_needs_a_question_and_keeps_the_input() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, script) = workbench(temp.path());
    submit(&mut harness, "/btw");
    assert!(harness.has_label("usage: /btw <question>"));
    assert_eq!(harness.state().composer().input, "/btw");
    assert!(script.asked.lock().unwrap().is_empty());
    assert!(harness.state().btw_view().is_none());
}

#[test]
fn btw_refuses_image_attachments_and_keeps_input_and_images() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, script) = workbench(temp.path());
    harness
        .state_mut()
        .composer_mut()
        .add_pasted_image("data:image/png;base64,aGVsbG8=");
    submit(&mut harness, "/btw look at this");
    assert!(script.asked.lock().unwrap().is_empty());
    assert_eq!(harness.state().composer().input, "/btw look at this");
    assert_eq!(harness.state().composer().attachments.len(), 1);
    assert!(harness.has_label("/btw does not take image attachments yet"));
}

#[test]
fn a_second_side_question_is_refused_while_one_runs() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, script) = workbench(temp.path());
    submit(&mut harness, "/btw first");
    submit(&mut harness, "/btw second");
    assert_eq!(script.asked.lock().unwrap().len(), 1);
    assert_eq!(harness.state().composer().input, "/btw second");
    assert_eq!(harness.state().btw_view().unwrap().question, "first");
}

#[test]
fn start_failures_show_a_notice_without_the_question_and_keep_the_input() {
    let temp = tempfile::tempdir().unwrap();
    for error in [
        BtwStartError::Refused(BtwError::NoLocalContext),
        BtwStartError::Refused(BtwError::ContextNotReady),
        BtwStartError::Refused(BtwError::UnsupportedProvider),
        BtwStartError::Unavailable,
    ] {
        let (mut harness, script) = workbench(temp.path());
        *script.refuse.lock().unwrap() = Some(error);
        submit(&mut harness, "/btw secret-question");
        assert_eq!(harness.state().composer().input, "/btw secret-question");
        assert!(harness.state().btw_view().is_none());
        assert!(script.submitted.lock().unwrap().is_empty());
        let transcript = transcript_text(&harness);
        assert!(transcript.contains("Notice"), "{transcript}");
        assert!(!transcript.contains("secret-question"));
    }
}

#[test]
fn a_failed_answer_is_shown_in_the_window() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, script) = workbench(temp.path());
    submit(&mut harness, "/btw why");
    script.senders.lock().unwrap()[0]
        .send(Err(BtwError::UnexpectedToolCall))
        .unwrap();
    harness.run();
    assert!(matches!(
        harness.state().btw_view().unwrap().status,
        BtwStatus::Failed { .. }
    ));
}

#[test]
fn cancel_stops_the_request_and_a_late_answer_is_dropped() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, script) = workbench(temp.path());
    submit(&mut harness, "/btw why");
    harness.click_label("Cancel");
    harness.run();
    assert_eq!(script.cancelled.load(Ordering::SeqCst), 1);
    assert_eq!(
        harness.state().btw_view().unwrap().status,
        BtwStatus::Cancelled
    );
    assert!(
        script.senders.lock().unwrap()[0]
            .send(answer("late"))
            .is_err()
    );
    harness.run();
    assert_eq!(
        harness.state().btw_view().unwrap().status,
        BtwStatus::Cancelled
    );
}

#[test]
fn escape_closes_the_side_question_without_stopping_the_run() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, script) = workbench(temp.path());
    submit(&mut harness, "/btw why");
    harness.key_press(egui::Modifiers::NONE, egui::Key::Escape);
    harness.run();
    assert!(harness.state().btw_view().is_none());
    assert_eq!(script.cancelled.load(Ordering::SeqCst), 1);
    assert!(script.submitted.lock().unwrap().is_empty());
    assert!(
        !harness
            .state()
            .issued()
            .iter()
            .any(|command| matches!(command, WorkbenchCommand::StopChat { .. }))
    );
}

#[test]
fn switching_threads_disposes_the_window() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, script) = workbench(temp.path());
    submit(&mut harness, "/btw why");
    submit(&mut harness, "/new");
    harness.run();
    assert!(harness.state().btw_view().is_none());
    assert_eq!(script.cancelled.load(Ordering::SeqCst), 1);
}

#[test]
fn btw_is_listed_in_completions_and_help() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, _script) = workbench(temp.path());
    harness.state_mut().composer_mut().input = "/bt".into();
    harness.run();
    harness.click_label("/btw <question>");
    harness.run();
    assert_eq!(harness.state().composer().input.trim(), "/btw");
    submit(&mut harness, "/help");
    assert!(harness.state().transcripts().thread().entries().iter().any(
        |entry| matches!(entry, TranscriptEntry::Notice { text } if text.contains("/btw <question> —"))
    ));
}

#[test]
fn only_a_btw_command_is_a_side_question() {
    let temp = tempfile::tempdir().unwrap();
    let (mut harness, _script) = workbench(temp.path());
    let model = harness.state_mut().composer_mut();
    model.input = "/btw hi".into();
    assert!(model.is_side_question());
    for input in ["/btw", "hello", "/goal /btw x", "/run x"] {
        model.input = input.into();
        assert_eq!(model.is_side_question(), input == "/btw", "{input}");
    }
}
