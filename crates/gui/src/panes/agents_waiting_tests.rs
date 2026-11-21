//! The Agents pane shows why a delegated run waits, and only while the run reads Waiting.
//! (Root runs are not rows here; the Run diagnostics window covers them.)

use super::*;
use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{AgentRunPhase, Event, LifecycleEvent, WaitingCause};
use runtime::{AgentSummary, RunId};

struct Source(Vec<AgentSummary>);
impl AgentRunSource for Source {
    fn list(&self) -> Vec<AgentSummary> {
        self.0.clone()
    }
}

type State = (TasksModel<Source>, TelemetryOverlay);

fn summary(id: u64, parent: Option<u64>, phase: AgentRunPhase) -> AgentSummary {
    AgentSummary {
        run_id: RunId::new(id),
        parent_run_id: parent.map(RunId::new),
        name: format!("Agent {id}"),
        role_name: "worker".into(),
        phase,
        model: "test".into(),
        category: None,
    }
}

fn changed(run: &str, to: AgentRunPhase, waiting: Option<WaitingCause>) -> Event {
    Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: run.into(),
        from: AgentRunPhase::Running,
        to,
        reason: None,
        waiting,
    })
}

fn harness(runs: Vec<AgentSummary>) -> Harness<'static, State> {
    let mut tasks = TasksModel::new(Source(runs));
    tasks.refresh();
    Harness::builder()
        .with_size(egui::vec2(1400.0, 600.0))
        .build_ui_state(
            |ui, (tasks, telemetry)| {
                agents_pane(ui, tasks, telemetry);
            },
            (tasks, TelemetryOverlay::new()),
        )
}

#[test]
fn child_rows_show_their_own_recorded_waiting_reason() {
    // Given: one child waiting after its turn and another waiting for a required answer.
    let mut h = harness(vec![
        summary(2, Some(1), AgentRunPhase::Waiting),
        summary(3, Some(1), AgentRunPhase::Waiting),
    ]);
    h.state_mut().1.apply_event(&changed(
        "run-2",
        AgentRunPhase::Waiting,
        Some(WaitingCause::TurnCompleted),
    ));
    h.state_mut().1.apply_event(&changed(
        "run-3",
        AgentRunPhase::Waiting,
        Some(WaitingCause::BlockingQuestions),
    ));

    // When: the pane is drawn.
    h.step();

    // Then: each row reads its own reason next to the phase.
    h.get_by_label("Waiting for input — turn complete, awaiting input");
    h.get_by_label("Waiting for input — required question");
}

#[test]
fn a_waiting_row_without_a_recorded_cause_keeps_the_plain_wording_and_other_phases_stay_plain() {
    // Given: a Waiting row recorded before causes existed, and a Running one.
    let mut h = harness(vec![
        summary(2, Some(1), AgentRunPhase::Waiting),
        summary(3, Some(1), AgentRunPhase::Running),
    ]);
    h.state_mut()
        .1
        .apply_event(&changed("run-2", AgentRunPhase::Waiting, None));
    h.state_mut().1.apply_event(&changed(
        "run-3",
        AgentRunPhase::Waiting,
        Some(WaitingCause::InputBoundary),
    ));
    h.state_mut()
        .1
        .apply_event(&changed("run-3", AgentRunPhase::Running, None));

    // When: the pane is drawn.
    h.step();

    // Then: the old row keeps the plain phase wording and its tooltip says nothing was
    // recorded; the resumed run keeps no stale reason.
    h.get_by_label("Waiting for input");
    assert!(h.query_by_label_contains("reason not recorded").is_none());
    let unrecorded = h.state().1.row("run-2").unwrap().waiting.clone().unwrap();
    assert_eq!(unrecorded.cause, None);
    assert!(
        unrecorded
            .detail()
            .contains("before waiting reasons existed")
    );
    h.get_by_label("Running");
    assert!(h.query_by_label_contains("awaiting input").is_none());
}
