//! ADR 0043 Phase 1: the pull request bar above the composer, drawn offscreen.
//!
//! The bar belongs to the thread (a subagent's transcript shows it too), offers Refresh and
//! Unlink only, never draws success for a state it cannot establish, and stays within 40% of
//! the conversation area.

use std::cell::RefCell;
use std::rc::Rc;

use egui::epaint::Shape;
use egui_kittest::{Harness, kittest::Queryable};
use gui::model::composer::ComposerModel;
use gui::model::model_picker::ModelPickerState;
use gui::model::pull_requests::{PrGlyph, PullRequestAction, ThreadPullRequest};
use gui::model::transcript::TranscriptModel;
use gui::panes::agent::{
    AgentIdentity, AgentPaneAction, ConversationContext, PullRequestBarContext, agent_pane,
};
use gui::theme::style::{ThemePreset, install_preset};
use storage::pull_requests::{
    CheckConclusion, CheckKind, CheckStatus, FetchErrorKind, FetchFailure, LinkOrigin,
    LocalHeadObservation, PullRequestCheck, PullRequestFacts, PullRequestKey, PullRequestLink,
    PullRequestState, ReviewDecision,
};

const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OTHER_SHA: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const WINDOW_HEIGHT: f32 = 800.0;

fn check(name: &str, status: CheckStatus, conclusion: Option<CheckConclusion>) -> PullRequestCheck {
    PullRequestCheck {
        provider_id: name.into(),
        kind: CheckKind::CheckRun,
        name: name.into(),
        status,
        conclusion,
        head_sha: SHA.into(),
    }
}

fn facts(
    state: PullRequestState,
    checks: Vec<PullRequestCheck>,
    complete: bool,
) -> PullRequestFacts {
    PullRequestFacts {
        state,
        base_branch: "main".into(),
        head_repository: Some("turtton/evorch".into()),
        head_branch: "feat/bar".into(),
        head_sha: SHA.into(),
        checks,
        checks_complete: complete,
        review_decision: ReviewDecision::ReviewRequired,
    }
}

fn view(facts: Option<PullRequestFacts>) -> ThreadPullRequest {
    ThreadPullRequest {
        thread_id: "thread".into(),
        lifecycle_revision: 1,
        transition_seq: 1,
        link: Some(PullRequestLink {
            link_id: 1,
            thread_id: "thread".into(),
            key: PullRequestKey::github("turtton/evorch", 12).unwrap(),
            origin: LinkOrigin::Explicit,
            parent_link_id: None,
            created_at_ns: 0,
            link_revision: 1,
            end: None,
        }),
        local_head: Some(LocalHeadObservation {
            link_revision: 1,
            head_sha: SHA.into(),
            observed_at_ns: 0,
        }),
        facts,
        fetched_at_ns: Some(gui::model::pull_requests::now_ns()),
        failure: None,
        candidates: Vec::new(),
        tracked_here: true,
    }
}

fn failing() -> PullRequestFacts {
    facts(
        PullRequestState::Open,
        vec![
            check(
                "build",
                CheckStatus::Completed,
                Some(CheckConclusion::Success),
            ),
            check(
                "lint",
                CheckStatus::Completed,
                Some(CheckConclusion::Failure),
            ),
        ],
        true,
    )
}

struct Bar {
    view: ThreadPullRequest,
    expanded: bool,
    subagent: bool,
}

type Clicked = Rc<RefCell<Option<PullRequestAction>>>;

fn harness(bar: Bar) -> (Harness<'static>, Clicked) {
    let clicked: Clicked = Rc::default();
    let sink = Rc::clone(&clicked);
    let mut composer = ComposerModel::default();
    let mut picker = ModelPickerState::default();
    let harness = Harness::builder()
        .with_size(egui::vec2(900.0, WINDOW_HEIGHT))
        .build_ui(move |ui| {
            gui::theme::install(ui.ctx());
            let identity = bar.subagent.then_some(AgentIdentity {
                run_id: "run-sub",
                name: Some("worker-1"),
                role: Some("worker"),
                ledger: &[],
            });
            let action = agent_pane(
                ui,
                &TranscriptModel::default(),
                identity,
                ConversationContext {
                    todo: None,
                    goal: None,
                    requests: None,
                    task_rows: &[],
                    phase_unread: false,
                    has_project: true,
                    active_thread_title: Some("Bar test"),
                    extra_read_dirs: &[],
                    parent_thread: None,
                    child_threads: Vec::new(),
                    lane: None,
                    lead: None,
                    thread_metrics: None,
                    phase: None,
                    sandbox_picker: Default::default(),
                    branch: None,
                    pull_request: Some(PullRequestBarContext {
                        view: &bar.view,
                        expanded: bar.expanded,
                        now_ns: gui::model::pull_requests::now_ns(),
                    }),
                    pull_request_chips: None,
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
            if let Some(AgentPaneAction::PullRequest(action)) = action {
                *sink.borrow_mut() = Some(action);
            }
        });
    (harness, clicked)
}

/// Every text the accessibility tree holds, as a label or as a value.
fn labels(harness: &Harness<'_>) -> Vec<String> {
    use egui_kittest::kittest::NodeT;
    harness
        .root()
        .children_recursive()
        .flat_map(|node| {
            let node = node.accesskit_node();
            [node.label(), node.value()]
        })
        .flatten()
        .map(|text| text.to_owned())
        .collect()
}

// Given: an expanded bar of a failing pull request / When: it is drawn / Then: the operations
// are Refresh and Unlink, and nothing of Phase 2 or of a trust-boundary notice is there
#[test]
fn the_bar_offers_refresh_and_unlink_and_nothing_else() {
    let (mut h, _) = harness(Bar {
        view: view(Some(failing())),
        expanded: true,
        subagent: false,
    });
    h.run_steps(4);
    h.get_by_label("Refresh");
    h.get_by_label("Unlink");
    for label in labels(&h) {
        let lower = label.to_lowercase();
        for forbidden in [
            "auto fix",
            "autofix",
            "send to agent",
            "agent に送る",
            "link…",
            "link...",
            "untrusted",
            "external data",
            "not from the user",
            "信頼",
            "ユーザー発言",
        ] {
            assert!(!lower.contains(forbidden), "{forbidden:?} in {label:?}");
        }
    }
}

// Given: a bar / When: Refresh and Unlink are clicked / Then: each reports its thread
#[test]
fn refresh_and_unlink_report_the_thread() {
    for (button, expected) in [
        (
            "Refresh",
            PullRequestAction::Refresh {
                thread_id: "thread".into(),
            },
        ),
        (
            "Unlink",
            PullRequestAction::Unlink {
                thread_id: "thread".into(),
            },
        ),
    ] {
        let (mut h, clicked) = harness(Bar {
            view: view(Some(failing())),
            expanded: false,
            subagent: false,
        });
        h.run_steps(3);
        h.get_by_label(button).click();
        h.run_steps(3);
        assert_eq!(clicked.borrow().as_ref(), Some(&expected));
    }
}

// Given: a collapsed bar / Then: one line with the number, the state, the CI summary and the
// branch; the check list waits for the expansion
#[test]
fn a_collapsed_bar_is_one_summary_line() {
    let (mut h, _) = harness(Bar {
        view: view(Some(failing())),
        expanded: false,
        subagent: false,
    });
    h.run_steps(4);
    h.get_by_label("#12");
    h.get_by_label("Open");
    h.get_by_label("CI failing 1/2");
    assert!(
        !labels(&h).iter().any(|label| label.contains("lint")),
        "the list of checks is for the expanded bar"
    );
}

// Given: an expanded bar / Then: failures one by one, passes on one line, branches, both
// commits, the review decision
#[test]
fn the_expanded_bar_lists_checks_and_facts() {
    let mut shown = view(Some(failing()));
    shown.local_head.as_mut().unwrap().head_sha = OTHER_SHA.into();
    let (mut h, _) = harness(Bar {
        view: shown,
        expanded: true,
        subagent: false,
    });
    h.run_steps(4);
    let all = labels(&h);
    let has = |text: &str| all.iter().any(|label| label.contains(text));
    assert!(has("lint · failure"), "{all:?}");
    assert!(has("1 passed"), "{all:?}");
    assert!(has("main ← feat/bar"), "{all:?}");
    assert!(has(SHA), "{all:?}");
    assert!(has(OTHER_SHA), "{all:?}");
    assert!(has("Review required"), "{all:?}");
    assert!(
        has("Local HEAD bbbbbbb differs from the pull request head aaaaaaa"),
        "{all:?}"
    );
}

// Given: states that do not establish success / Then: no success glyph or word is drawn
#[test]
fn an_unknown_state_is_never_drawn_as_success() {
    let mut failed_fetch = view(Some(facts(
        PullRequestState::Open,
        vec![check(
            "build",
            CheckStatus::Completed,
            Some(CheckConclusion::Success),
        )],
        true,
    )));
    failed_fetch.failure = Some(FetchFailure {
        kind: FetchErrorKind::Network,
        message: "offline".into(),
    });
    let cases = [
        view(None),
        view(Some(facts(PullRequestState::Open, vec![], true))),
        view(Some(facts(
            PullRequestState::Open,
            vec![check(
                "build",
                CheckStatus::Completed,
                Some(CheckConclusion::Success),
            )],
            false,
        ))),
        failed_fetch,
    ];
    for case in cases {
        assert_ne!(
            gui::model::pull_requests::glyph(&case),
            Some(PrGlyph::Passing)
        );
        let (mut h, _) = harness(Bar {
            view: case,
            expanded: false,
            subagent: false,
        });
        h.run_steps(4);
        for label in labels(&h) {
            assert!(!label.contains("passing"), "{label:?}");
            assert_ne!(label, gui::theme::icons::CHECK);
        }
    }
    let passing = view(Some(facts(
        PullRequestState::Open,
        vec![check(
            "build",
            CheckStatus::Completed,
            Some(CheckConclusion::Success),
        )],
        true,
    )));
    let (mut h, _) = harness(Bar {
        view: passing,
        expanded: false,
        subagent: false,
    });
    h.run_steps(4);
    h.get_by_label("CI passing 1/1");
}

// Given: a window that does not poll the thread / Then: the bar says so as a status, with the
// age of the last fetch
#[test]
fn a_window_that_does_not_track_the_thread_says_so() {
    let mut shown = view(Some(failing()));
    shown.tracked_here = false;
    let (mut h, _) = harness(Bar {
        view: shown,
        expanded: false,
        subagent: false,
    });
    h.run_steps(4);
    assert!(
        labels(&h)
            .iter()
            .any(|label| label.starts_with("Not tracked in this window · last fetched")),
        "{:?}",
        labels(&h)
    );
}

// Given: a failed fetch / Then: the failure is a visible status
#[test]
fn a_failed_fetch_is_a_visible_status() {
    let mut shown = view(Some(failing()));
    shown.failure = Some(FetchFailure {
        kind: FetchErrorKind::Unauthenticated,
        message: "gh auth".into(),
    });
    let (mut h, _) = harness(Bar {
        view: shown,
        expanded: false,
        subagent: false,
    });
    h.run_steps(4);
    assert!(
        labels(&h)
            .iter()
            .any(|label| label.starts_with("Fetch failed (unauthenticated)")),
        "{:?}",
        labels(&h)
    );
}

// Given: a subagent's transcript / Then: the thread's bar is there
#[test]
fn the_bar_is_drawn_in_a_subagents_transcript_too() {
    let (mut h, _) = harness(Bar {
        view: view(Some(failing())),
        expanded: false,
        subagent: true,
    });
    h.run_steps(4);
    h.get_by_label("#12");
    h.get_by_label("Refresh");
}

// Given: sixty failing checks / Then: the expanded bar stays within 40% of the area and its
// operations stay at the bottom
#[test]
fn the_expanded_bar_never_grows_past_forty_percent() {
    let checks = (0..60)
        .map(|index| {
            check(
                &format!("check-{index:03}"),
                CheckStatus::Completed,
                Some(CheckConclusion::Failure),
            )
        })
        .collect();
    let (mut h, _) = harness(Bar {
        view: view(Some(facts(PullRequestState::Open, checks, true))),
        expanded: true,
        subagent: false,
    });
    h.run_steps(5);
    let refresh = h.get_by_label("Refresh").rect();
    let first = h.get_by_label_contains("check-000").rect();
    let height = refresh.bottom() - first.top();
    let limit = gui::panes::agent::max_bar_height(WINDOW_HEIGHT);
    assert!(
        height <= limit,
        "bar {height} is taller than 40% ({limit}) of the window"
    );
    assert!(height > 100.0, "the list was drawn: {height}");
    assert!(refresh.bottom() < WINDOW_HEIGHT);
}

fn text_colors(harness: &Harness<'_>) -> Vec<egui::Color32> {
    harness
        .output()
        .shapes
        .iter()
        .filter_map(|shape| match &shape.shape {
            Shape::Text(text) => Some(text),
            _ => None,
        })
        .flat_map(|text| {
            text.galley
                .job
                .sections
                .iter()
                .map(|section| section.format.color)
        })
        .collect()
}

// Given: a merged pull request / When: drawn in each theme / Then: the purple of that theme is
// used, with the word and the glyph alongside
#[test]
fn merged_is_purple_in_both_themes_with_a_word() {
    for (preset, purple) in [
        (
            ThemePreset::Graphite,
            egui::Color32::from_rgb(0xa3, 0x71, 0xf7),
        ),
        (
            ThemePreset::TokyoNight,
            egui::Color32::from_rgb(0xbb, 0x9a, 0xf7),
        ),
    ] {
        let merged = view(Some(facts(PullRequestState::Merged, vec![], true)));
        let mut composer = ComposerModel::default();
        let mut picker = ModelPickerState::default();
        let mut h = Harness::builder()
            .with_size(egui::vec2(900.0, WINDOW_HEIGHT))
            .build_ui(move |ui| {
                install_preset(ui.ctx(), preset);
                agent_pane(
                    ui,
                    &TranscriptModel::default(),
                    None,
                    ConversationContext {
                        todo: None,
                        goal: None,
                        requests: None,
                        task_rows: &[],
                        phase_unread: false,
                        has_project: true,
                        active_thread_title: Some("Merged"),
                        extra_read_dirs: &[],
                        parent_thread: None,
                        child_threads: Vec::new(),
                        lane: None,
                        lead: None,
                        thread_metrics: None,
                        phase: None,
                        sandbox_picker: Default::default(),
                        branch: None,
                        pull_request: Some(PullRequestBarContext {
                            view: &merged,
                            expanded: false,
                            now_ns: gui::model::pull_requests::now_ns(),
                        }),
                        pull_request_chips: None,
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
            });
        h.run_steps(4);
        h.get_by_label("Merged");
        assert!(text_colors(&h).contains(&purple), "{preset:?}");
        assert_eq!(
            gui::panes::agent::pull_request_glyph_style(PrGlyph::Merged).1,
            purple
        );
    }
}

// Writes one PNG per representative state when `EVORCH_PR_BAR_CAPTURE_DIR` names a directory
// (the real render needs a wgpu adapter; without the variable nothing is rendered).
#[test]
fn representative_states_render_for_evidence() {
    let Some(directory) = std::env::var_os("EVORCH_PR_BAR_CAPTURE_DIR") else {
        return;
    };
    let directory = std::path::PathBuf::from(directory);
    std::fs::create_dir_all(&directory).unwrap();
    let mut untracked = view(Some(failing()));
    untracked.tracked_here = false;
    untracked.fetched_at_ns = Some(gui::model::pull_requests::now_ns() - 312 * 1_000_000_000);
    let mut fetch_failed = view(Some(failing()));
    fetch_failed.failure = Some(FetchFailure {
        kind: FetchErrorKind::Unauthenticated,
        message: "gh is not authenticated".into(),
    });
    let mut failing_view = view(Some(failing()));
    failing_view.local_head.as_mut().unwrap().head_sha = OTHER_SHA.into();
    let pending = facts(
        PullRequestState::Open,
        vec![
            check(
                "build",
                CheckStatus::Completed,
                Some(CheckConclusion::Success),
            ),
            check("test", CheckStatus::InProgress, None),
        ],
        true,
    );
    let states = [
        ("pending", view(Some(pending)), false),
        ("failure-expanded", failing_view, true),
        (
            "merged",
            view(Some(facts(PullRequestState::Merged, vec![], true))),
            false,
        ),
        ("not-tracked", untracked, false),
        ("fetch-failed", fetch_failed, false),
    ];
    for (name, state, expanded) in states {
        let (mut h, _) = harness(Bar {
            view: state,
            expanded,
            subagent: false,
        });
        h.run_steps(5);
        h.render()
            .unwrap()
            .save(directory.join(format!("pull-request-bar-{name}.png")))
            .unwrap();
    }
}
