//! The shared pull request bar, glyph and badge, built from the core's view of a thread.
//!
//! The core decides; this only says it in the embedded workbench's words. Whether the budget
//! is spent is the core's `limit_reached`, never a count compared here.
use workbench_protocol::namespaces::pull_requests::{
    FactsView, HeadAttribution, PullRequestRef, PullRequestState, ThreadPullRequest,
};
use workbench_protocol::wire::UnixMillis;

use crate::panes::pull_request_widgets::{
    AutoFixControls, AutoFixInput, BarView, CheckRow, CiCounts, FactsRows, PrGlyph,
    PullRequestBadge, auto_fix_controls_of, checks_pending, ci_words, glyph_from, mismatches_of,
    statuses_of, verdict_of_labels,
};

const NANOS_PER_MILLI: i64 = 1_000_000;

/// `owner/repo#number`, the label of a pull request everywhere in the interface.
pub(crate) fn label(reference: &PullRequestRef) -> String {
    format!("{}#{}", reference.repository, reference.number)
}

fn nanos(at: UnixMillis) -> i64 {
    i64::try_from(at.get())
        .unwrap_or(i64::MAX)
        .saturating_mul(NANOS_PER_MILLI)
}

fn facts(view: &ThreadPullRequest) -> Option<&FactsView> {
    view.snapshot.as_ref()?.facts.as_ref()
}

fn fetch_failed(view: &ThreadPullRequest) -> bool {
    view.snapshot
        .as_ref()
        .is_some_and(|snapshot| snapshot.last_failure.is_some())
}

fn rows(facts: &FactsView) -> Vec<CheckRow<'_>> {
    facts
        .checks
        .iter()
        .map(|check| CheckRow {
            name: &check.name,
            status: &check.status,
            conclusion: check.conclusion.as_deref(),
        })
        .collect()
}

pub(crate) fn glyph(view: &ThreadPullRequest) -> Option<PrGlyph> {
    let facts = facts(view);
    let verdict = facts.map_or(crate::panes::pull_request_widgets::Verdict::Unknown, |f| {
        verdict_of_labels(&rows(f), f.checks_complete)
    });
    glyph_from(
        view.link.is_some(),
        facts.map(|f| f.state),
        fetch_failed(view),
        verdict,
    )
}

pub(crate) fn ci(view: &ThreadPullRequest) -> String {
    let counts = facts(view).map(|facts| {
        let rows = rows(facts);
        CiCounts::of(
            facts.state == PullRequestState::Open,
            verdict_of_labels(&rows, facts.checks_complete),
            &rows,
        )
    });
    ci_words(counts, fetch_failed(view))
}

pub(crate) fn badge(view: &ThreadPullRequest) -> Option<PullRequestBadge> {
    Some(PullRequestBadge {
        number: view.link.as_ref()?.pull_request.number,
        glyph: glyph(view)?,
    })
}

/// The auto fix controls; `None` without a link or the core's auto fix view. What the
/// protocol does not carry (the head notice, a stopped or paused thread) reads as absent.
pub(crate) fn controls(view: &ThreadPullRequest, now_ns: i64) -> Option<AutoFixControls> {
    let link = view.link.as_ref()?;
    let auto = view.auto_fix.as_ref()?;
    let facts = facts(view);
    let open = facts.is_some_and(|facts| facts.state == PullRequestState::Open);
    let input = AutoFixInput {
        thread: view.thread.as_str(),
        link_revision: link.link_revision.get(),
        open,
        failing: glyph(view) == Some(PrGlyph::Failing),
        checks_pending: facts.is_none_or(|f| checks_pending(&rows(f), f.checks_complete)),
        enabled: auto.enabled,
        destination: auto.destination.as_ref().map(|thread| thread.as_str()),
        policy_link_revision: auto.link_revision.map(|revision| revision.get()),
        policy_revision: auto.policy_revision.get(),
        used: auto.used,
        reserved: auto.reserved,
        limit: auto.limit,
        limit_reached: auto.limit_reached,
        global_enabled: auto.globally_enabled,
        cooldown_until_ns: auto.cooldown_until.map(nanos),
        stop_reason: auto.stop_reason.as_deref(),
        head_notice: None,
        this_stopped: false,
        suppressed: false,
        head_from_agent: view.head_attribution == HeadAttribution::Agent,
        head_sha: facts.map(|facts| facts.head_sha.as_str()),
    };
    Some(auto_fix_controls_of(&input, now_ns))
}

pub(crate) struct BarShape {
    pub expanded: bool,
    pub collapsible: bool,
    pub actions_enabled: bool,
    pub now_ns: i64,
}

pub(crate) fn bar_view(view: &ThreadPullRequest, shape: BarShape) -> Option<BarView<'_>> {
    let link = view.link.as_ref()?;
    let snapshot = view.snapshot.as_ref();
    let failure = snapshot.and_then(|snapshot| snapshot.last_failure.as_ref());
    let fetched_at_ns = snapshot.and_then(|snapshot| snapshot.fetched_at.map(nanos));
    let facts = facts(view);
    Some(BarView {
        number: Some(link.pull_request.number),
        label: Some(label(&link.pull_request)),
        title: snapshot.and_then(|snapshot| snapshot.title.as_deref()),
        glyph: glyph(view).unwrap_or(PrGlyph::Unknown),
        ci: ci(view),
        statuses: statuses_of(
            fetched_at_ns,
            failure.map(|failure| (failure.kind.as_str(), failure.message.as_str())),
            shape.now_ns,
        ),
        fetched_at_ns,
        failure_message: failure.map(|failure| failure.message.as_str()),
        facts: facts.map(|facts| FactsRows {
            state: facts.state,
            base_branch: &facts.base_branch,
            head_branch: &facts.head_branch,
            head_sha: &facts.head_sha,
            review: facts.review_decision,
            checks: rows(facts),
            checks_complete: facts.checks_complete,
        }),
        local_head: view.local_head.as_deref(),
        mismatches: facts.map_or_else(Vec::new, |facts| {
            mismatches_of(
                facts.state == PullRequestState::Open && failure.is_none(),
                &facts.head_branch,
                &facts.head_sha,
                view.local_head.as_deref(),
            )
        }),
        auto_fix: controls(view, shape.now_ns),
        expanded: shape.expanded,
        collapsible: shape.collapsible,
        now_ns: shape.now_ns,
        actions_enabled: shape.actions_enabled,
    })
}
