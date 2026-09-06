use std::sync::{Arc, Mutex};

use event_bus::{AgentRunPhase, Event, LifecycleEvent, MessageEvent};
use gui::app::WorkbenchState;
use gui::model::tasks::AgentRunSource;
use gui::model::transcript::TranscriptEntry;
use runtime::AgentSummary;
use workspace_ui::UiSettings;

#[derive(Clone)]
struct MockSource(Vec<AgentSummary>);

impl AgentRunSource for MockSource {
    fn list(&self) -> Vec<AgentSummary> {
        self.0.clone()
    }
}

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<String>>);

impl std::io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("log buffer lock")
            .push_str(&String::from_utf8_lossy(buf));
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = LogBuffer;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn sole_running_state() -> WorkbenchState<MockSource> {
    let mut state = WorkbenchState::new(MockSource(vec![]), &UiSettings::default()).expect("state");
    state.apply_events([Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: "run-1".into(),
        from: AgentRunPhase::Pending,
        to: AgentRunPhase::Running,
        reason: None,
    })]);
    state
}

#[test]
fn runless_delta_is_dropped_and_warned_even_when_one_run_is_running() {
    // Given: one Running run and a scoped warning subscriber.
    let mut state = sole_running_state();
    let buffer = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buffer.clone())
        .with_ansi(false)
        .without_time()
        .finish();

    // When: both run-less stream variants arrive.
    tracing::subscriber::with_default(subscriber, || {
        state.apply_events([
            Event::new(MessageEvent::MessageDelta {
                delta: "legacy".into(),
                run_id: None,
            }),
            Event::new(MessageEvent::ReasoningDelta {
                delta: "legacy-r".into(),
                run_id: None,
            }),
        ]);
    });

    // Then: neither transcript receives text and each dropped event warns.
    assert!(state.transcripts().thread().entries().is_empty());
    assert!(state.transcripts().run("run-1").is_none());
    let logs = buffer.0.lock().expect("log buffer lock");
    assert!(logs.contains("WARN"), "{logs}");
    assert_eq!(logs.matches("run-less stream delta").count(), 2, "{logs}");
}

#[test]
fn attributed_delta_display_is_unchanged_after_mirror_removal() {
    // Given: one Running run.
    let mut state = sole_running_state();

    // When: both stream variants explicitly target that run.
    state.apply_events([
        Event::new(MessageEvent::MessageDelta {
            delta: "once".into(),
            run_id: Some("run-1".into()),
        }),
        Event::new(MessageEvent::ReasoningDelta {
            delta: "why".into(),
            run_id: Some("run-1".into()),
        }),
    ]);

    // Then: the run and thread each contain both entries exactly once.
    let expected = [
        TranscriptEntry::Message {
            text: "once".into(),
        },
        TranscriptEntry::Reasoning { text: "why".into() },
    ];
    assert_eq!(
        state
            .transcripts()
            .run("run-1")
            .expect("run transcript")
            .entries(),
        &expected
    );
    assert_eq!(state.transcripts().thread().entries(), &expected);
}
