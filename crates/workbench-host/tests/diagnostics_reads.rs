//! The `diagnostics` reads (PR 13): the usage ledger reduced to totals that keep
//! unpriced and unreported requests apart, and a run's saved context reduced to
//! an overview and one line per message.
#[path = "memory_reads/support.rs"]
mod support;

use std::time::{SystemTime, UNIX_EPOCH};

use storage::usage::{UsageRequestRecord, UsageStatus};
use support::{World, operation, ready, world};
use workbench_host::areas::{Areas, diagnostics::DiagnosticsArea};
use workbench_protocol::WorkbenchError;
use workbench_protocol::errors::ErrorCode;
use workbench_protocol::ids::RunId;
use workbench_protocol::namespaces::diagnostics::{
    ContextMessageRole, DiagnosticsQuery, DiagnosticsReply, GetRunContext, GetUsageSummary,
    ReadOutcome, RunContext, UsageRange, UsageSummary,
};
use workbench_protocol::operation::OperationState;
use workbench_protocol::queries::{PageRequest, Query, QueryReply};

fn diagnostics_world() -> World {
    world(|parts| Areas {
        diagnostics: DiagnosticsArea::from_parts(
            parts.tasks,
            parts.projection.source("diagnostics").unwrap(),
            parts.database,
            parts.runtime,
            &parts.storage,
            90,
        ),
        ..Default::default()
    })
}

fn usage(
    w: &World,
    id: &str,
    range: UsageRange,
) -> Result<ReadOutcome<UsageSummary>, WorkbenchError> {
    match w.query(Query::Diagnostics(DiagnosticsQuery::GetUsageSummary(
        GetUsageSummary {
            operation_id: operation(id),
            range,
        },
    )))? {
        QueryReply::Diagnostics(DiagnosticsReply::Usage(outcome)) => Ok(*outcome),
        other => panic!("{other:?}"),
    }
}

fn context(
    w: &World,
    id: &str,
    run: &str,
    page: PageRequest,
) -> Result<ReadOutcome<RunContext>, WorkbenchError> {
    match w.query(Query::Diagnostics(DiagnosticsQuery::GetRunContext(
        GetRunContext {
            operation_id: operation(id),
            run: RunId::new(run).unwrap(),
            page,
        },
    )))? {
        QueryReply::Diagnostics(DiagnosticsReply::Context(outcome)) => Ok(*outcome),
        other => panic!("{other:?}"),
    }
}

fn record(
    id: &str,
    model: &str,
    cost: Option<f64>,
    reported: bool,
    status: UsageStatus,
) -> UsageRequestRecord {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    UsageRequestRecord {
        request_id: id.into(),
        at_ns: i64::try_from(now.as_nanos()).unwrap(),
        provider: "main".into(),
        profile: None,
        model: model.into(),
        run_id: None,
        parent_run_id: None,
        role: Some("Worker".into()),
        purpose: None,
        status,
        failure: None,
        finish_reason: None,
        input_tokens: if reported { 100 } else { 0 },
        output_tokens: if reported { 10 } else { 0 },
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: None,
        ttft_ms: None,
        duration_ms: 5,
        cost_usd: cost,
        usage_reported: reported,
        service_tier: None,
    }
}

#[test]
fn usage_keeps_priced_unpriced_and_unreported_requests_apart() {
    let w = diagnostics_world();
    let writer = w.store.handle();
    writer
        .record_usage_requests(vec![
            record("r1", "model-a", Some(0.5), true, UsageStatus::Ok),
            record("r2", "model-a", Some(0.25), true, UsageStatus::Failed),
            record("r3", "model-b", None, true, UsageStatus::Ok),
            record("r4", "model-b", None, false, UsageStatus::Ok),
        ])
        .unwrap();
    writer.flush_usage_now().unwrap();

    assert!(usage(&w, "u", UsageRange::AllTime).unwrap().data.is_none());
    w.settle();
    let outcome = usage(&w, "u", UsageRange::AllTime).unwrap();
    assert!(ready(&outcome.operation.state), "{:?}", outcome.operation);
    let summary = outcome.data.unwrap();
    let totals = &summary.totals;
    assert_eq!((totals.requests, totals.failed), (4, 1));
    assert_eq!(totals.recorded_cost_micro_usd, 750_000);
    assert_eq!(
        totals.unpriced_tokens, 110,
        "only the reported unpriced request"
    );
    assert_eq!(totals.unreported_requests, 1);
    assert_eq!(totals.input_tokens, 300);
    assert_eq!(summary.days.len(), 1);
    assert_eq!(summary.days[0].totals, *totals);
    assert_eq!(summary.models.len(), 2);
    assert_eq!(summary.models[0].model, "model-a");
    assert_eq!(summary.models[0].totals.requests, 2);
    assert_eq!(summary.to_day, summary.days[0].day);

    let today = w
        .read(|| usage(&w, "today", UsageRange::Today))
        .data
        .unwrap();
    assert_eq!(today.totals, summary.totals);
    assert_eq!(today.from_day, today.to_day);
    let other = usage(&w, "u", UsageRange::Today);
    assert_eq!(other.unwrap_err().code, ErrorCode::Conflict);
}

#[test]
fn an_empty_ledger_is_an_empty_summary() {
    let w = diagnostics_world();
    let summary = w
        .read(|| usage(&w, "empty", UsageRange::Last30Days))
        .data
        .unwrap();
    assert_eq!(summary.totals.requests, 0);
    assert!(summary.days.is_empty() && summary.models.is_empty());
}

#[test]
fn a_run_context_is_an_overview_and_one_summary_line_per_message() {
    let w = diagnostics_world();
    let _enter = w.rt.enter();
    let run = w.runtime.delegate_background(
        runtime::Role::Worker,
        "investigate the failing build\nwith details".into(),
        runtime::RunConfig::default(),
    );
    w.rt.block_on(w.reached.notified());
    let run_id = run;
    let run = run.to_string();

    let first = w.read(|| {
        context(
            &w,
            "ctx",
            &run,
            PageRequest {
                cursor: None,
                limit: 1,
            },
        )
    });
    let data = first.data.expect("the context is read");
    assert_eq!(data.overview.role, "Worker");
    assert_eq!(data.overview.run.as_str(), run);
    assert!(data.overview.message_count >= 1, "{:?}", data);
    assert!(data.overview.tools.iter().any(|tool| tool == "send"));
    assert!(!data.overview.segments.is_empty());
    assert_eq!(data.messages.items.len(), 1);

    let mut roles = vec![];
    let mut page = data.messages;
    loop {
        for message in &page.items {
            assert!(message.summary.len() <= 120);
            assert!(!message.summary.contains('\n'));
            roles.push((message.index, message.role, message.summary.clone()));
        }
        let Some(cursor) = page.next_cursor.clone() else {
            break;
        };
        page = context(
            &w,
            "ctx",
            &run,
            PageRequest {
                cursor: Some(cursor),
                limit: 1,
            },
        )
        .unwrap()
        .data
        .unwrap()
        .messages;
    }
    assert_eq!(roles.len(), data.overview.message_count as usize);
    assert!(
        roles
            .iter()
            .enumerate()
            .all(|(n, (index, ..))| *index as usize == n)
    );
    assert!(
        roles
            .iter()
            .any(|(_, role, summary)| *role == ContextMessageRole::User
                && summary == "investigate the failing build")
    );
    let _ = w.runtime.cancel(run_id);
}

#[test]
fn an_unknown_run_fails_as_not_found_and_a_bad_id_is_refused() {
    let w = diagnostics_world();
    let unknown = w.read(|| context(&w, "unknown", "run-9999", PageRequest::default()));
    assert!(unknown.data.is_none());
    assert_eq!(
        unknown.operation.state,
        OperationState::Failed {
            code: ErrorCode::NotFound
        }
    );
    let bad = context(&w, "bad", "not-a-run", PageRequest::default());
    assert_eq!(bad.unwrap_err().code, ErrorCode::InvalidRequest);

    let unwired = world(|_| Areas::default());
    assert_eq!(
        usage(&unwired, "x", UsageRange::Today).unwrap_err().code,
        ErrorCode::UnsupportedCapability
    );
}

// ---- scoped and live usage (ADR 0045 E4) --------------------------------------

use storage::{RunContextRecord, threads::ThreadUpsert};
use workbench_protocol::events::{Subscribe, SubscriptionScope, UiEvent};
use workbench_protocol::ids::ThreadId;
use workbench_protocol::namespaces::diagnostics::{
    DiagnosticsEvent, GetLiveUsage, GetRunUsage, GetThreadUsage, LiveUsage, RunUsage, ThreadUsage,
};

const DAY_NS: i64 = 86_400 * 1_000_000_000;

fn now_ns() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    )
    .unwrap()
}

fn ask_thread_usage(
    w: &World,
    id: &str,
    thread: &str,
) -> Result<ReadOutcome<ThreadUsage>, WorkbenchError> {
    match w.query(Query::Diagnostics(DiagnosticsQuery::GetThreadUsage(
        GetThreadUsage {
            operation_id: operation(id),
            thread: ThreadId::new(thread).unwrap(),
        },
    )))? {
        QueryReply::Diagnostics(DiagnosticsReply::ThreadUsage(outcome)) => Ok(*outcome),
        other => panic!("{other:?}"),
    }
}

fn ask_run_usage(w: &World, id: &str, run: &str) -> Result<ReadOutcome<RunUsage>, WorkbenchError> {
    match w.query(Query::Diagnostics(DiagnosticsQuery::GetRunUsage(
        GetRunUsage {
            operation_id: operation(id),
            run: RunId::new(run).unwrap(),
        },
    )))? {
        QueryReply::Diagnostics(DiagnosticsReply::RunUsage(outcome)) => Ok(*outcome),
        other => panic!("{other:?}"),
    }
}

fn ask_live_usage(w: &World, id: &str) -> Result<ReadOutcome<LiveUsage>, WorkbenchError> {
    match w.query(Query::Diagnostics(DiagnosticsQuery::GetLiveUsage(
        GetLiveUsage {
            operation_id: operation(id),
        },
    )))? {
        QueryReply::Diagnostics(DiagnosticsReply::LiveUsage(outcome)) => Ok(*outcome),
        other => panic!("{other:?}"),
    }
}

fn request_of(id: &str, run: Option<&str>, cost: Option<f64>, at_ns: i64) -> UsageRequestRecord {
    let mut one = record(id, "model-a", cost, true, UsageStatus::Ok);
    one.run_id = run.map(str::to_owned);
    one.at_ns = at_ns;
    one
}

fn thread_row(id: &str, created_at_ns: i64) -> ThreadUpsert {
    ThreadUpsert {
        thread_id: id.into(),
        project_root: Some("/repo".into()),
        created_at_ns,
        updated_at_ns: created_at_ns,
        ..Default::default()
    }
}

/// A saved run of `thread`; a run with a parent belongs where its parent does.
fn run_row(run: &str, parent: Option<&str>, thread: &str) -> RunContextRecord {
    RunContextRecord {
        run_id: run.into(),
        role: "worker".into(),
        name: format!("name-{run}"),
        parent_run_id: parent.map(str::to_owned),
        config_json: if parent.is_some() {
            "{}".into()
        } else {
            format!(r#"{{"thread_id":"{thread}"}}"#)
        },
        messages_json: "[]".into(),
        checkpoints_json: "[]".into(),
        terminal_phase: "Done".into(),
        restorable: false,
        updated_at_ns: 1,
        host_inputs_json: "[]".into(),
    }
}

/// Thread `t` has a conversation root, a continued root and a child of the first;
/// thread `other` has a root of its own; `stray` belongs to no thread.
fn seeded_world(created_at_ns: i64) -> World {
    let w = diagnostics_world();
    let writer = w.store.handle();
    writer
        .upsert_thread_runtime(&thread_row("t", created_at_ns))
        .unwrap();
    writer
        .upsert_thread_runtime(&thread_row("other", created_at_ns))
        .unwrap();
    for run in [
        run_row("root-1", None, "t"),
        run_row("root-2", None, "t"),
        run_row("child-1", Some("root-1"), "t"),
        run_row("root-o", None, "other"),
    ] {
        writer.upsert_run_context(&run).unwrap();
    }
    let at = now_ns();
    writer
        .record_usage_requests(vec![
            request_of("q-root-1", Some("root-1"), Some(1.0), at),
            request_of("q-root-2", Some("root-2"), Some(2.0), at),
            request_of("q-child", Some("child-1"), Some(4.0), at),
            request_of("q-child-unpriced", Some("child-1"), None, at),
            request_of("q-other", Some("root-o"), Some(8.0), at),
            request_of("q-stray", Some("stray"), Some(16.0), at),
            request_of("q-no-run", None, Some(32.0), at),
        ])
        .unwrap();
    w
}

#[test]
fn a_thread_counts_its_roots_continued_roots_and_children_and_no_other_thread() {
    let w = seeded_world(now_ns());
    let usage = w
        .read(|| ask_thread_usage(&w, "t", "t"))
        .data
        .expect("the thread's usage is read");
    let total = &usage.total.totals;
    assert_eq!(usage.thread.as_str(), "t");
    assert_eq!(total.requests, 4);
    assert_eq!(total.recorded_cost_micro_usd, 7_000_000);
    assert_eq!(total.unpriced_tokens, 110, "the unpriced child request");
    let conversation = &usage.conversation.totals;
    assert_eq!(conversation.requests, 2, "both roots, not the child");
    assert_eq!(conversation.recorded_cost_micro_usd, 3_000_000);
    assert!(!usage.total.incomplete && !usage.conversation.incomplete);

    let other = w.read(|| ask_thread_usage(&w, "o", "other")).data.unwrap();
    assert_eq!(other.total.totals.recorded_cost_micro_usd, 8_000_000);
    assert_eq!(other.total.totals, other.conversation.totals);
}

#[test]
fn a_run_counts_itself_and_not_the_runs_it_started() {
    let w = seeded_world(now_ns());
    let root = w.read(|| ask_run_usage(&w, "r1", "root-1")).data.unwrap();
    assert_eq!(root.run.as_str(), "root-1");
    assert_eq!(root.total.totals.requests, 1);
    assert_eq!(root.total.totals.recorded_cost_micro_usd, 1_000_000);
    let child = w.read(|| ask_run_usage(&w, "r2", "child-1")).data.unwrap();
    assert_eq!(child.total.totals.requests, 2);
    assert_eq!(child.total.totals.recorded_cost_micro_usd, 4_000_000);
    assert!(!root.total.incomplete && !child.total.incomplete);
}

#[test]
fn what_the_ledger_cannot_know_is_unknown_or_incomplete_and_never_a_zero() {
    let w = seeded_world(now_ns());
    let missing = w.read(|| ask_run_usage(&w, "no-run", "no-such-run"));
    assert_eq!(
        missing.operation.state,
        OperationState::Failed {
            code: ErrorCode::NotFound
        }
    );
    assert!(missing.data.is_none());
    let missing = w.read(|| ask_thread_usage(&w, "no-thread", "no-such-thread"));
    assert_eq!(
        missing.operation.state,
        OperationState::Failed {
            code: ErrorCode::NotFound
        }
    );

    // A request of a run no thread knows is a lower bound: its start is unknown.
    let stray = w.read(|| ask_run_usage(&w, "stray", "stray")).data.unwrap();
    assert_eq!(stray.total.totals.requests, 1);
    assert!(stray.total.incomplete);

    // A thread older than the ledger keeps its rows is a lower bound too.
    let old = seeded_world(now_ns() - 200 * DAY_NS);
    let usage = old.read(|| ask_thread_usage(&old, "t", "t")).data.unwrap();
    assert_eq!(usage.total.totals.requests, 4);
    assert!(usage.total.incomplete && usage.conversation.incomplete);
    let run = old
        .read(|| ask_run_usage(&old, "r", "root-1"))
        .data
        .unwrap();
    assert!(run.total.incomplete);
}

#[test]
fn the_live_usage_holds_this_minute_today_and_the_month() {
    let w = diagnostics_world();
    let empty = w.read(|| ask_live_usage(&w, "empty")).data.unwrap();
    assert!(empty.validate().is_ok());
    assert_eq!(empty.today.requests + empty.month.requests, 0);
    assert!(empty.minutes.iter().all(|minute| minute.requests == 0));

    let writer = w.store.handle();
    writer
        .record_usage_requests(vec![
            request_of("live-1", None, Some(0.5), now_ns()),
            request_of("live-2", Some("run-x"), None, now_ns()),
        ])
        .unwrap();
    let live = w.read(|| ask_live_usage(&w, "live")).data.unwrap();
    assert!(live.validate().is_ok());
    assert_eq!(live.minutes.iter().map(|m| m.requests).sum::<u64>(), 2);
    assert_eq!(live.today.requests, 2);
    assert_eq!(live.month.requests, 2);
    assert_eq!(live.month.recorded_cost_micro_usd, 500_000);
    assert_eq!(live.month.unpriced_tokens, 110);
    assert!(live.quota_windows.is_empty());
    assert!(live.day_start_ms <= live.as_of_ms && live.as_of_ms < live.day_end_ms);
}

#[test]
fn a_commit_is_announced_once_it_is_visible_and_a_refused_write_is_not() {
    let w = diagnostics_world();
    let mut events = w
        .host
        .projection()
        .subscribe_with_snapshot(&Subscribe {
            scope: SubscriptionScope::All,
            after: None,
        })
        .unwrap();
    let writer = w.store.handle();

    // A write the ledger cannot store, then a good one.
    let mut unstorable = request_of("bad", Some("run-refused"), None, now_ns());
    unstorable.input_tokens = u64::MAX;
    writer.record_usage_requests(vec![unstorable]).unwrap_err();
    writer
        .record_usage_requests(vec![request_of("good", Some("run-good"), None, now_ns())])
        .unwrap();

    let change = w.rt.block_on(async {
        loop {
            let envelope = events.recv().await.expect("the subscription is open");
            if let UiEvent::Diagnostics(DiagnosticsEvent::UsageChanged { change }) = envelope.event
            {
                return change;
            }
        }
    });
    assert_eq!(
        change.runs.iter().map(RunId::as_str).collect::<Vec<_>>(),
        ["run-good"],
        "only the committed run is named"
    );
    assert!(!change.all_runs);
    // The announcement is a reason to ask: what it announced can already be read.
    let live = w.read(|| ask_live_usage(&w, "after")).data.unwrap();
    assert_eq!(live.today.requests, 1);
}
