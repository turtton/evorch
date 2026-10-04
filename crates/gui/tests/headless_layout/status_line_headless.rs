use egui::epaint::Shape;
use egui_kittest::{Harness, kittest::Queryable};
use gui::model::{
    composer::ComposerModel,
    model_picker::ModelPickerState,
    telemetry::{CacheReuseSummary, RequestReuse, ThreadMetrics},
    transcript::TranscriptModel,
};
use gui::panes::agent::{ConversationContext, agent_pane};
use workspace_ui::ThreadRunPhase;

fn harness(phase: Option<ThreadRunPhase>) -> Harness<'static> {
    let mut composer = ComposerModel::default();
    let mut picker = ModelPickerState::default();
    Harness::builder()
        .with_size(egui::vec2(900.0, 600.0))
        .build_ui(move |ui| {
            gui::theme::install(ui.ctx());
            agent_pane(
                ui,
                &TranscriptModel::default(),
                None,
                ConversationContext {
                    goal: None,
                    requests: None,
                    task_rows: &[],
                    phase_unread: true,
                    has_project: true,
                    active_thread_title: Some("Status test"),
                    parent_thread: None,
                    child_threads: Vec::new(),
                    thread_metrics: Some(ThreadMetrics {
                        cost: Some(0.225),
                        conversation_cost: Some(0.125),
                        cache_hit_rate: Some(50.0),
                        average_cache_hit_rate: Some(25.0),
                        cache_reuse: CacheReuseSummary {
                            latest: Some(RequestReuse::Compared {
                                read: 99,
                                previous: 100,
                            }),
                            retained_tokens: 97,
                            baseline_tokens: 100,
                            compared_requests: 2,
                            completed_requests: 3,
                        },
                        context_pressure: Some(25),
                        context_used_tokens: Some(200_900),
                        wall_time: std::time::Duration::from_secs(12),
                        ttft: phase.map(|_| std::time::Duration::from_millis(240)),
                        average_ttft: phase.map(|_| std::time::Duration::from_millis(400)),
                        tok_s: phase.map(|_| 40.0),
                        average_tok_s: phase.map(|_| 35.0),
                    }),
                    phase,
                    next_thread_title: String::new(),
                    sandbox_picker: Default::default(),
                    branch: None,
                    model_picker: gui::panes::model_picker::ModelPickerContext {
                        profiles: &[],
                        preference: None,
                        default_model: None,
                        enabled: false,
                    },
                },
                &mut composer,
                &mut picker,
            );
        })
}

#[test]
fn status_line_renders_below_composer_with_metrics_order() {
    // Given: a thread with metrics and no latency sample.
    let mut h = harness(None);
    // When: layout settles over real egui frames.
    h.run_steps(4);
    // Then: the status dot precedes ordered metrics below the composer.
    let labels = [
        "$0.125",
        "cache 99% (avg 97%)",
        "TTFT —",
        "— tok/s",
        "ctx 200K(25%)",
        "wall 12s",
    ];
    let rects: Vec<_> = labels
        .iter()
        .map(|label| h.get_by_label(label).rect())
        .collect();
    assert!(rects[0].top() > h.get_by_label("Message or /command").rect().bottom());
    for pair in rects.windows(2) {
        assert!(pair[0].right() < pair[1].left());
        assert!((pair[0].center().y - pair[1].center().y).abs() < 1.0);
    }
    assert!(h.output().shapes.iter().any(|shape| matches!(&shape.shape,
        Shape::Circle(circle) if circle.center.x < rects[0].left()
            && (circle.center.y - rects[0].center().y).abs() < 1.0)));
}

#[test]
fn running_phase_shows_spinner_without_badge_text() {
    // Given / When: a running thread is rendered.
    let mut h = harness(Some(ThreadRunPhase::Running));
    h.run_steps(4);
    // Then: the spinner is at the left of the status row with no running text.
    assert!(h.query_by_label("running").is_none());
    let cost = h.get_by_label("$0.125").rect();
    assert!(h.get_by_label("TTFT 240ms (avg 400ms)").rect().left() > cost.right());
    assert!(
        h.get_by_label("40.0 tok/s (avg 35.0 tok/s)").rect().left()
            > h.get_by_label("TTFT 240ms (avg 400ms)").rect().right()
    );
    assert!(
        h.output().shapes.iter().any(|shape| matches!(&shape.shape,
        Shape::Path(path) if !path.closed && path.points.iter().all(|point|
            point.x < cost.left() && point.y > cost.top() - 4.0 && point.y < cost.bottom() + 4.0)))
    );
}

#[test]
fn header_shows_thread_total_below_title() {
    // Given / When: the thread with metrics is rendered.
    let mut h = harness(Some(ThreadRunPhase::Done));
    h.run_steps(4);
    // Then: the family total is below the title; the composer shows only the main cost.
    let title = h.get_by_label("Thread: Status test").rect();
    let total = h.get_by_label("Total cost $0.225").rect();
    let main = h.get_by_label("$0.125").rect();
    assert!(total.top() >= title.bottom());
    assert!(main.top() > h.get_by_label("Message or /command").rect().bottom());
}
