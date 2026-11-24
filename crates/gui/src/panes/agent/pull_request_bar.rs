//! The pull request bar (ADR 0043): one line above the composer for the thread's linked pull
//! request, expanding upward into its checks and facts.
//!
//! The bar belongs to the thread, not to a run, so it is drawn in a subagent's transcript too.
//! It shows what the database holds and offers two operations: Refresh and Unlink. A state that
//! cannot be established is never drawn as success.
//!
//! The drawing is shared with the core client (`panes::pull_request_widgets`); this builds its
//! view from the database's read.

use crate::model::pull_requests::{
    PullRequestAction, ThreadPullRequest, auto_fix_controls, check_rows, ci_summary, glyph,
    mismatches, protocol_review, protocol_state, statuses,
};
use crate::panes::pull_request_widgets::{self as widgets, BarChoice, BarView, FactsRows, PrGlyph};
pub use crate::panes::pull_request_widgets::{glyph_style, max_bar_height};

/// What the bar draws.
pub struct PullRequestBarContext<'a> {
    pub view: &'a ThreadPullRequest,
    pub expanded: bool,
    /// Wall clock, nanoseconds since the Unix epoch.
    pub now_ns: i64,
}

fn bar_view<'a>(bar: &PullRequestBarContext<'a>) -> BarView<'a> {
    let view = bar.view;
    BarView {
        number: view.number(),
        label: view.label(),
        title: view.display.title.as_ref().map(|title| title.as_str()),
        glyph: glyph(view).unwrap_or(PrGlyph::Unknown),
        ci: ci_summary(view),
        statuses: statuses(view, bar.now_ns),
        fetched_at_ns: view.fetched_at_ns,
        failure_message: view
            .failure
            .as_ref()
            .map(|failure| failure.message.as_str()),
        facts: view.facts.as_ref().map(|facts| FactsRows {
            state: protocol_state(facts.state),
            base_branch: &facts.base_branch,
            head_branch: &facts.head_branch,
            head_sha: &facts.head_sha,
            review: protocol_review(facts.review_decision),
            checks: check_rows(facts),
            checks_complete: facts.checks_complete,
        }),
        local_head: view.local_head.as_ref().map(|head| head.head_sha.as_str()),
        mismatches: mismatches(view),
        auto_fix: auto_fix_controls(view, bar.now_ns),
        expanded: bar.expanded,
        collapsible: true,
        now_ns: bar.now_ns,
        actions_enabled: true,
    }
}

/// Draws the bar as a bottom panel. `conversation_height` is the height of the area the bar
/// shares with the transcript; the bar never grows past 40% of it.
pub(super) fn show(
    ui: &mut egui::Ui,
    bar: &PullRequestBarContext<'_>,
    conversation_height: f32,
    action: &mut Option<super::AgentPaneAction>,
) {
    let Some(choice) = widgets::bar(ui, &bar_view(bar), conversation_height) else {
        return;
    };
    let thread_id = bar.view.thread_id.clone();
    let asked = match choice {
        BarChoice::Refresh => PullRequestAction::Refresh { thread_id },
        BarChoice::Unlink => PullRequestAction::Unlink { thread_id },
        BarChoice::ToggleAutoFix => PullRequestAction::ToggleAutoFix { thread_id },
        BarChoice::SendToAgent => PullRequestAction::SendToAgent { thread_id },
        BarChoice::ResetCount => PullRequestAction::ResetAutoFixBudget { thread_id },
        BarChoice::ToggleExpanded => PullRequestAction::ToggleExpanded { thread_id },
    };
    *action = Some(super::AgentPaneAction::PullRequest(asked));
}
