use super::*;
use storage::usage::UsageRequestRecord;

/// "cheap" costs $1/M input and $2/M output; "free" has no price.
fn pricing() -> UsagePricing {
    let mut cheap = config::ModelEntryConfig::enabled("cheap");
    cheap.input_price = Some(1.0);
    cheap.output_price = Some(2.0);
    let profile = config::ProviderProfileConfig {
        models: vec![cheap],
        ..config::ProviderProfileConfig::default()
    };
    UsagePricing::new([("main".to_owned(), profile)].into(), None)
}

fn request(day: &str, model: &str, project: &str, cost: Option<f64>) -> UsageRequestRow {
    UsageRequestRow {
        record: UsageRequestRecord {
            request_id: format!("{day}-{model}-{project}"),
            at_ns: 0,
            provider: "openai".into(),
            profile: Some("main".into()),
            model: model.into(),
            run_id: None,
            parent_run_id: None,
            role: Some("worker".into()),
            purpose: Some("agent".into()),
            status: UsageStatus::Ok,
            failure: None,
            finish_reason: None,
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: None,
            ttft_ms: Some(100),
            duration_ms: 1_000,
            cost_usd: cost,
        },
        day: day.into(),
        time: "12:00:00".into(),
        thread_id: None,
        project_id: Some(project.into()),
    }
}

fn dataset(rows: &[UsageRequestRow]) -> UsageDataset {
    UsageDataset::from_rows(LocalDay::parse("2026-10-04").unwrap(), rows, &[])
}

#[test]
fn recorded_mode_keeps_recorded_prices_and_prices_only_unrecorded_tokens() {
    // Given: one request recorded at an old price and one recorded without a price.
    let pricing = pricing();
    let data = dataset(&[
        request("2026-10-04", "cheap", "a", Some(10.0)),
        request("2026-10-04", "cheap", "a", None),
    ]);

    // When: totals are computed in each cost mode.
    let recorded = breakdown(
        &data.facts,
        &UsageFilter::default(),
        UsageDimension::Day,
        &mut CostCalculator::new(&pricing, CostMode::Recorded),
    )
    .1;
    let current = breakdown(
        &data.facts,
        &UsageFilter::default(),
        UsageDimension::Day,
        &mut CostCalculator::new(&pricing, CostMode::Current),
    )
    .1;

    // Then: recorded keeps $10 and adds $3 at today's rate; current reprices both.
    assert_eq!(recorded.cost, 13.0);
    assert_eq!(current.cost, 6.0);
    assert!(!recorded.unpriced && !current.unpriced);
}

#[test]
fn unknown_prices_are_flagged_instead_of_counted_as_free() {
    // Given: a request for a model without any price.
    let pricing = pricing();
    let data = dataset(&[request("2026-10-04", "free", "a", None)]);

    // When: its cost is computed.
    let (_, total) = breakdown(
        &data.facts,
        &UsageFilter::default(),
        UsageDimension::Model,
        &mut CostCalculator::new(&pricing, CostMode::Recorded),
    );

    // Then: the total is marked as a lower bound.
    assert_eq!(total.cost, 0.0);
    assert!(total.unpriced);
}

#[test]
fn breakdown_groups_filters_and_orders_rows() {
    // Given: requests across two days, two projects and two models, plus a rollup.
    let pricing = pricing();
    let rollup = UsageDailyRow {
        day: "2026-09-01".into(),
        provider: "openai".into(),
        profile: Some("main".into()),
        model: "cheap".into(),
        project_id: Some("b".into()),
        request_count: 4,
        input_tokens: 10,
        output_tokens: 10,
        cost_usd: 0.5,
        ..UsageDailyRow::default()
    };
    let data = UsageDataset::from_rows(
        LocalDay::parse("2026-10-04").unwrap(),
        &[
            request("2026-10-03", "cheap", "a", Some(1.0)),
            request("2026-10-04", "cheap", "a", Some(1.0)),
            request("2026-10-04", "free", "a", Some(0.0)),
            request("2026-10-04", "cheap", "b", Some(5.0)),
        ],
        &[rollup],
    );
    let mut costs = CostCalculator::new(&pricing, CostMode::Recorded);

    // When: grouped by project, by day, and by day for project "a" only.
    let (projects, total) = breakdown(
        &data.facts,
        &UsageFilter::default(),
        UsageDimension::Project,
        &mut costs,
    );
    let (days, _) = breakdown(
        &data.facts,
        &UsageFilter::default(),
        UsageDimension::Day,
        &mut costs,
    );
    let filter = UsageFilter {
        project: Some("a".into()),
        ..UsageFilter::default()
    };
    let (filtered, filtered_total) =
        breakdown(&data.facts, &filter, UsageDimension::Day, &mut costs);

    // Then: projects sort by cost, days newest first, and filters narrow totals.
    let keys = |rows: &[BreakdownRow]| rows.iter().map(|row| row.key.clone()).collect::<Vec<_>>();
    assert_eq!(keys(&projects), ["b", "a"]);
    assert_eq!(projects[0].totals.requests, 5);
    assert_eq!(total.requests, 8);
    assert_eq!(
        projects[1]
            .models
            .iter()
            .map(|(model, totals)| (model.as_str(), totals.requests))
            .collect::<Vec<_>>(),
        [("cheap", 2), ("free", 1)]
    );
    assert_eq!(keys(&days), ["2026-10-04", "2026-10-03", "2026-09-01"]);
    assert_eq!(keys(&filtered), ["2026-10-04", "2026-10-03"]);
    assert_eq!(filtered_total.cost, 2.0);
}

#[test]
fn ranges_start_on_the_expected_day() {
    let today = LocalDay::parse("2026-10-04").unwrap();
    let first = |range: UsageRange| range.first_day(today).map(|day| day.to_string());
    assert_eq!(first(UsageRange::Today).as_deref(), Some("2026-10-04"));
    assert_eq!(first(UsageRange::Last7Days).as_deref(), Some("2026-09-28"));
    assert_eq!(first(UsageRange::ThisMonth).as_deref(), Some("2026-10-01"));
    assert_eq!(first(UsageRange::AllTime), None);
}
