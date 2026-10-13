//! Deterministic populated workbench state for headless capture and tests.

mod codex_auth;
mod events;
mod provider;
mod sidebar;

use std::sync::Arc;

use event_bus::AgentRunPhase;
use runtime::{AgentSummary, RunId};
use workspace_ui::SidebarState;

use crate::app::WorkbenchState;
use crate::diff::{DiffMode, FixtureDiffSource};
use crate::model::tasks::AgentRunSource;

pub use codex_auth::ScriptedCodexAuthBackend;
pub use events::{demo_error_events, demo_events};
pub use provider::demo_provider_config;
pub use sidebar::{FixtureError, demo_sidebar};

/// 引数がある要求とない要求を含む、承認待ちキャプチャ用イベントを返します。
pub fn demo_pending_approval_events() -> [event_bus::Event; 3] {
    use event_bus::{Event, ToolEvent};

    [
        Event::new(ToolEvent::ToolStarted {
            tool_name: "shell".into(),
            call_id: "call-1".into(),
            run_id: Some("run-2".into()),
            input: Some(serde_json::json!({"command": "rm -rf /tmp/build"})),
        }),
        Event::new(ToolEvent::ApprovalRequested {
            input: None,
            call_id: "run-2:call-1:17".into(),
            tool_name: "shell".into(),
        }),
        Event::new(ToolEvent::ApprovalRequested {
            input: None,
            call_id: "run-3:call-2:18".into(),
            tool_name: "write".into(),
        }),
    ]
}

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
            parent_run_id: Some(RunId::new(0)),
            name: "orchestrator".into(),
            role_name: "orchestrator".into(),
            phase: AgentRunPhase::Running,
            model: "demo-orchestrator".into(),
            category: None,
        },
        AgentSummary {
            run_id: RunId::new(2),
            parent_run_id: Some(RunId::new(0)),
            name: "implementer".into(),
            role_name: "worker".into(),
            phase: AgentRunPhase::Done,
            model: "demo-implementer".into(),
            category: None,
        },
        AgentSummary {
            run_id: RunId::new(3),
            parent_run_id: Some(RunId::new(0)),
            name: "reviewer".into(),
            role_name: "reviewer".into(),
            phase: AgentRunPhase::Waiting,
            model: "demo-reviewer".into(),
            category: None,
        },
    ]
}

/// sidebar / diff / transcript を demo 状態へ一括投入する。
pub fn populate<S: AgentRunSource>(
    state: WorkbenchState<S>,
    sidebar: SidebarState,
) -> WorkbenchState<S> {
    let mut state = state
        .with_sidebar(sidebar)
        .with_diff_source(Arc::new(FixtureDiffSource::demo()));
    state.apply_events(demo_events());
    state.request_diff(DiffMode::WorkingTree);
    state
}
