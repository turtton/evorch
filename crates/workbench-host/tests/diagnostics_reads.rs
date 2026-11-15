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
