//! The bounds of the scoped and live usage vocabulary (ADR 0045 E4).
use workbench_protocol::{
    events::UiEvent,
    handshake::{Capability, WireLimits},
    ids::{OperationId, RunId, ThreadId},
    namespaces::diagnostics::{
        DiagnosticsEvent, DiagnosticsQuery, DiagnosticsReply, GetLiveUsage, GetRunUsage,
        GetThreadUsage, LIVE_USAGE_MINUTES, LiveUsage, MAX_QUOTA_WINDOW_USAGES,
        MAX_USAGE_CHANGED_RUNS, QuotaWindowUsage, ReadOutcome, UsageChange, UsageTotals,
    },
    operation::{OperationState, OperationView},
    wire::Revision,
};

fn live() -> LiveUsage {
    LiveUsage {
        as_of_ms: 1_700_000_000_000,
        minutes: vec![UsageTotals::default(); LIVE_USAGE_MINUTES],
        local_day: "2026-01-07".into(),
        day_start_ms: 1_699_999_000_000,
        day_end_ms: 1_700_085_400_000,
        today: UsageTotals::default(),
        month: UsageTotals::default(),
        quota_windows: Vec::new(),
    }
}

fn change(runs: usize, all_runs: bool) -> UsageChange {
    UsageChange {
        runs: (0..runs)
            .map(|index| RunId::new(format!("run-{index}")).unwrap())
            .collect(),
        all_runs,
    }
}

#[test]
fn a_live_usage_holds_one_bucket_per_minute_of_the_window() {
    live().validate().unwrap();
    for len in [0, LIVE_USAGE_MINUTES - 1, LIVE_USAGE_MINUTES + 1] {
        let mut usage = live();
        usage.minutes = vec![UsageTotals::default(); len];
        assert!(usage.validate().is_err(), "{len} buckets");
    }
}

#[test]
fn a_live_usage_names_a_day_that_has_a_length_and_bounded_quota_windows() {
    let mut usage = live();
    usage.day_end_ms = usage.day_start_ms;
    assert!(usage.validate().is_err());

    let window = |index: usize| QuotaWindowUsage {
        profile: format!("profile-{index}"),
        window: "5h".into(),
        totals: UsageTotals::default(),
    };
    let mut usage = live();
    usage.quota_windows = (0..MAX_QUOTA_WINDOW_USAGES).map(window).collect();
    usage.validate().unwrap();
    usage.quota_windows.push(window(MAX_QUOTA_WINDOW_USAGES));
    assert!(usage.validate().is_err());

    let mut usage = live();
    usage.quota_windows = vec![window(0)];
    usage.quota_windows[0].profile.clear();
    assert!(usage.validate().is_err());
}

#[test]
fn a_usage_change_names_its_runs_or_says_that_all_changed() {
    change(1, false).validate().unwrap();
    change(MAX_USAGE_CHANGED_RUNS, false).validate().unwrap();
    change(0, true).validate().unwrap();
    for bad in [
        change(MAX_USAGE_CHANGED_RUNS + 1, false),
        change(0, false),
        change(2, true),
    ] {
        assert!(bad.validate().is_err(), "{bad:?}");
        assert!(
            DiagnosticsEvent::UsageChanged {
                change: bad.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            UiEvent::Diagnostics(DiagnosticsEvent::UsageChanged { change: bad })
                .validate()
                .is_err()
        );
    }
}

#[test]
fn the_usage_queries_are_plain_queries_and_data_only_comes_from_a_ready_operation() {
    let id = OperationId::new("op-1").unwrap();
    for query in [
        DiagnosticsQuery::GetThreadUsage(GetThreadUsage {
            operation_id: id.clone(),
            thread: ThreadId::new("thr-1").unwrap(),
        }),
        DiagnosticsQuery::GetRunUsage(GetRunUsage {
            operation_id: id.clone(),
            run: RunId::new("run-1").unwrap(),
        }),
        DiagnosticsQuery::GetLiveUsage(GetLiveUsage {
            operation_id: id.clone(),
        }),
    ] {
        assert_eq!(query.required_capability(), Capability::Queries);
        query.validate(&WireLimits::default()).unwrap();
    }

    let outcome = |state| ReadOutcome {
        operation: OperationView {
            operation_id: id.clone(),
            state,
            revision: Revision(1),
            result: None,
        },
        data: Some(live()),
    };
    DiagnosticsReply::LiveUsage(Box::new(outcome(OperationState::Ready)))
        .validate(&WireLimits::default())
        .unwrap();
    assert!(
        DiagnosticsReply::LiveUsage(Box::new(outcome(OperationState::Pending)))
            .validate(&WireLimits::default())
            .is_err()
    );
    let mut invalid = outcome(OperationState::Ready);
    invalid.data.as_mut().unwrap().minutes.clear();
    assert!(
        DiagnosticsReply::LiveUsage(Box::new(invalid))
            .validate(&WireLimits::default())
            .is_err()
    );
}
