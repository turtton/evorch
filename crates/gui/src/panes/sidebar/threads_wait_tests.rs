use egui_kittest::{
    Harness,
    kittest::{By, NodeT, Queryable},
};
use event_bus::{AgentRunPhase, Event, LifecycleEvent, WorkspaceLockHolder, WorkspaceWait};
use workspace_ui::{ProjectId, ThreadId};

use super::*;

struct Fixture {
    sidebar: SidebarState,
    telemetry: TelemetryOverlay,
    phases: BTreeMap<String, ThreadRunPhase>,
    questions: BTreeSet<ThreadId>,
}

fn wait_event(call_id: &str, holder_run: Option<&str>) -> Event {
    Event::new(LifecycleEvent::WorkspaceWaitChanged {
        run_id: "child".into(),
        call_id: call_id.into(),
        waiting: Some(WorkspaceWait {
            workspace_root: "/repo/shared".into(),
            tool_name: "shell".into(),
            command: Some(format!("rg {call_id}")),
            holder: holder_run.map(|run| WorkspaceLockHolder {
                run_id: run.into(),
                call_id: "watch-call".into(),
                tool_name: "shell".into(),
                command: Some("gh pr checks --watch".into()),
            }),
        }),
    })
}

fn fixture(now: Instant) -> Fixture {
    let project = ProjectId::new("current");
    let mut thread = ThreadRecord::new(ThreadId::new("waiting"), project.clone(), "Investigation");
    // A running sibling/root keeps its trailing spinner while its child waits.
    thread.run_ids = vec!["root".into(), "child".into()];
    let mut unrelated = ThreadRecord::new(ThreadId::new("unrelated"), project, "Unrelated");
    unrelated.run_ids = vec!["other".into()];
    let mut holder = ThreadRecord::new(
        ThreadId::new("holder-thread"),
        ProjectId::new("other-project"),
        "CI monitor",
    );
    holder.run_ids = vec!["holder".into()];
    holder.archived = true;
    let mut telemetry = TelemetryOverlay::new();
    telemetry.apply_event_at(&wait_event("first", Some("holder")), now);
    Fixture {
        sidebar: SidebarState {
            threads: vec![thread, unrelated, holder],
            ..Default::default()
        },
        telemetry,
        phases: [
            ("root".into(), ThreadRunPhase::Running),
            ("child".into(), ThreadRunPhase::Running),
        ]
        .into(),
        questions: [ThreadId::new("waiting")].into(),
    }
}

fn harness(width: f32, fixture: Fixture) -> Harness<'static, Fixture> {
    Harness::builder()
        .with_size(egui::vec2(width, 240.0))
        .build_ui_state(
            |ui, fixture| {
                crate::theme::install(ui.ctx());
                ui.ctx().global_style_mut(|style| {
                    style.interaction.tooltip_delay = 0.0;
                    style.interaction.show_tooltips_only_when_still = false;
                });
                let indicators = ThreadIndicators {
                    phases: &fixture.phases,
                    telemetry: &fixture.telemetry,
                    question_threads: &fixture.questions,
                };
                let threads = fixture.sidebar.threads.iter().take(2).collect::<Vec<_>>();
                render_tree(
                    ui,
                    &fixture.sidebar,
                    &threads,
                    &indicators,
                    false,
                    &mut None,
                );
            },
            fixture,
        )
}

#[test]
fn child_wait_is_supplemental_and_unrelated_thread_has_no_indicator() {
    let mut harness = harness(600.0, fixture(Instant::now()));
    // The running spinner continually repaints; render bounded frames.
    harness.run_steps(2);
    harness.get_by_label("Thread status: Running");
    harness.get_by_label("Answer needed: this thread has an unanswered question");
    harness.get_by_label("作業領域の使用待ち: waiting");
    assert!(
        harness
            .query_by_label("作業領域の使用待ち: unrelated")
            .is_none()
    );
    harness.get_by_label("作業領域の使用待ち: waiting").hover();
    harness.run_steps(3);
    // The title is resolved from all threads, although the holder is in another
    // project's archive and is not rendered in this list.
    let label = harness
        .query_all(By::new().label_contains("CI monitor"))
        .next()
        .expect("tooltip contains holder thread title");
    // AccessKit stores the displayed text of Role::Label nodes in value().
    let text = label.accesskit_node().value().expect("tooltip text");
    assert!(text.contains("holder"));
    assert!(text.contains("gh pr checks --watch"));
    assert!(text.contains("rg first"));
    assert!(text.contains("/repo/shared"));
}

#[test]
fn multiple_waits_share_one_icon_and_terminal_event_removes_it() {
    let now = Instant::now();
    let mut fixture = fixture(now);
    fixture
        .telemetry
        .apply_event_at(&wait_event("second", None), now);
    let mut harness = harness(600.0, fixture);
    harness.run_steps(2);
    assert_eq!(
        harness
            .query_all_by_label("作業領域の使用待ち: waiting")
            .count(),
        1
    );
    harness.get_by_label("作業領域の使用待ち: waiting").hover();
    harness.run_steps(3);
    for command in ["rg first", "rg second"] {
        assert!(
            harness
                .query_all(By::new().label_contains(command))
                .next()
                .is_some(),
            "tooltip enumerates {command}"
        );
    }
    harness.state_mut().telemetry.apply_event_at(
        &Event::new(LifecycleEvent::WorkspaceWaitChanged {
            run_id: "child".into(),
            call_id: "first".into(),
            waiting: None,
        }),
        now,
    );
    harness.run_steps(2);
    harness.get_by_label("作業領域の使用待ち: waiting");
    harness.state_mut().telemetry.apply_event_at(
        &Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: "child".into(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Stopped,
            reason: None,
        }),
        now,
    );
    harness.run_steps(2);
    assert!(
        harness
            .query_by_label("作業領域の使用待ち: waiting")
            .is_none()
    );
    harness.get_by_label("Thread status: Running");
}

#[test]
fn tooltip_elapsed_uses_injected_time_and_authoritative_holder_only() {
    let now = Instant::now();
    let mut fixture = fixture(now);
    fixture.telemetry.apply_event_at(
        &wait_event("first", Some("unmapped-run")),
        now + Duration::from_secs(45),
    );
    let (run, call, entry) = fixture
        .telemetry
        .workspace_waits(&fixture.sidebar.threads[0].run_ids)
        .next()
        .unwrap();
    let text = workspace_wait_tooltip(
        &fixture.sidebar,
        run,
        call,
        entry,
        now + Duration::from_secs(134),
    );
    assert!(text.contains("2分14秒"));
    assert!(text.contains("使用中: unmapped-run"));
    assert!(!text.contains("CI monitor"));
    fixture
        .telemetry
        .apply_event_at(&wait_event("first", None), now);
    let (run, call, entry) = fixture
        .telemetry
        .workspace_waits(&fixture.sidebar.threads[0].run_ids)
        .next()
        .unwrap();
    let text = workspace_wait_tooltip(&fixture.sidebar, run, call, entry, now);
    assert!(text.contains("使用中: 不明"));
    assert!(!text.contains("CI monitor"));
}

#[test]
fn narrow_row_keeps_wait_icon_and_title_hit_regions_separate() {
    let mut harness = harness(220.0, fixture(Instant::now()));
    harness.run_steps(2);
    let wait = harness.get_by_label("作業領域の使用待ち: waiting").rect();
    let title = harness.get_by_label("Investigation").rect();
    let question = harness
        .get_by_label("Answer needed: this thread has an unanswered question")
        .rect();
    let running = harness.get_by_label("Thread status: Running").rect();
    assert!(title.right() <= wait.left());
    assert!(wait.right() <= question.left());
    assert!(question.right() <= running.left());
    assert!(running.right() <= 220.0);
    harness.get_by_label("Investigation").click();
    harness.run_steps(2);
    // A title click cannot accidentally open the action menu or archive the row.
    assert!(harness.query_by_label("Fork").is_none());
}

#[test]
fn wait_coexists_with_error_and_question_indicators() {
    let mut fixture = fixture(Instant::now());
    fixture.phases.insert("root".into(), ThreadRunPhase::Error);
    let mut harness = harness(600.0, fixture);
    harness.run_steps(2);
    harness.get_by_label("Thread status: Error");
    harness.get_by_label("Answer needed: this thread has an unanswered question");
    harness.get_by_label("作業領域の使用待ち: waiting");
    assert!(harness.query_by_label("Thread status: Running").is_none());
    let archives = harness.query_all_by_label("Archive").collect::<Vec<_>>();
    assert!(
        archives[0].accesskit_node().is_disabled(),
        "a running child still blocks family archival"
    );
}
