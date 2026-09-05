//! Deterministic populated workbench state for headless capture and tests.

mod events;
mod sidebar;

use std::sync::Arc;

use event_bus::{AgentRunPhase, CiState, GateSnapshot, MergeBinding};
use runtime::{AgentSummary, RunId};
use workspace_ui::SidebarState;

use crate::app::WorkbenchState;
use crate::diff::{DiffMode, FixtureDiffSource};
use crate::model::commands::{
    CiStatus, GateItemView, LoopEvent, MergeApprovalView, PrRef, ReviewerStatus,
};
use crate::model::tasks::AgentRunSource;

pub use events::demo_events;
pub use sidebar::{FixtureError, demo_sidebar};

/// demo 固定 run 一覧を返す [`AgentRunSource`]。
pub struct DemoSource(pub Vec<AgentSummary>);

impl AgentRunSource for DemoSource {
    fn list(&self) -> Vec<AgentSummary> {
        self.0.clone()
    }
}

/// demo モードの run 構成 (orchestrator / implementer / reviewer)。
pub fn demo_runs() -> Vec<AgentSummary> {
    vec![
        AgentSummary {
            run_id: RunId::new(1),
            name: "orchestrator".into(),
            role_name: "orchestrator".into(),
            phase: AgentRunPhase::Running,
            model: "demo-orchestrator".into(),
        },
        AgentSummary {
            run_id: RunId::new(2),
            name: "implementer".into(),
            role_name: "worker".into(),
            phase: AgentRunPhase::Done,
            model: "demo-implementer".into(),
        },
        AgentSummary {
            run_id: RunId::new(3),
            name: "reviewer".into(),
            role_name: "reviewer".into(),
            phase: AgentRunPhase::Waiting,
            model: "demo-reviewer".into(),
        },
    ]
}

/// issue #81 の PR 状態を模した merge approval view。
pub fn demo_merge_view() -> MergeApprovalView {
    MergeApprovalView {
        pr: Some(PrRef {
            number: 81,
            title: "v0.3 GUI design system refinement".into(),
            url: "https://github.com/turtton/evorch/pull/81".into(),
        }),
        ci: CiStatus::Pending,
        reviewer: ReviewerStatus::Pending,
        diff_summary: Some("theme tokens and pane restyle".into()),
        resolution: None,
        binding: Some(MergeBinding {
            token_id: "token-81".into(),
            repo: "turtton/evorch".into(),
            pr_number: 81,
            head_sha: DEMO_HEAD_SHA.into(),
            snapshot: GateSnapshot {
                repo: "turtton/evorch".into(),
                pr_number: 81,
                base_ref: "main".into(),
                head_sha: DEMO_HEAD_SHA.into(),
                ci: CiState::Green,
                criteria_round: 1,
                review_round: 1,
                reviewer_run_id: "run-3".into(),
            },
        }),
        gate: vec![
            GateItemView {
                label: "pull_request".into(),
                ok: true,
                detail: "#81 (turtton/evorch)".into(),
            },
            GateItemView {
                label: "ci".into(),
                ok: false,
                detail: "pending".into(),
            },
        ],
        blocked: None,
    }
}

const DEMO_HEAD_SHA: &str = "81b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9";

/// sidebar / diff / transcript / merge を demo 状態へ一括投入する。
pub fn populate<S: AgentRunSource>(
    state: WorkbenchState<S>,
    sidebar: SidebarState,
) -> WorkbenchState<S> {
    let mut state = state
        .with_sidebar(sidebar)
        .with_diff_source(Arc::new(FixtureDiffSource::demo()));
    state.apply_events(demo_events());
    state.apply_loop_event(LoopEvent::MergeStateUpdated(Box::new(demo_merge_view())));
    state.request_diff(DiffMode::WorkingTree);
    state
}
