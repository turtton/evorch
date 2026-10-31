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
            usage_reported: true,
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
            service_tier: None,
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

#[test]
fn current_mode_prices_requests_by_tier_and_speed_and_daily_totals_at_base_rates() {
    // Given: $1/M input with a $3/M tier above 1.5M and $5/M fast input, and a 1M-input
    // fast request, a 2M-input request and a 2M-input daily rollup, none with a recorded price.
    let mut entry = config::ModelEntryConfig::enabled("tiered");
    entry.input_price = Some(1.0);
    entry.price_tiers = Some(vec![config::types::provider::PriceTier {
        above_input_tokens: 1_500_000,
        input: Some(3.0),
        output: None,
        cache_read: None,
        cache_write: None,
    }]);
    entry.fast_price = Some(config::types::provider::ModelPricing {
        input: Some(5.0),
        ..Default::default()
    });
    let profile = config::ProviderProfileConfig {
        models: vec![entry],
        ..config::ProviderProfileConfig::default()
    };
    let pricing = UsagePricing::new([("main".to_owned(), profile)].into(), None);
    let mut fast = request("2026-10-04", "tiered", "fast", None);
    fast.record.output_tokens = 0;
    fast.record.service_tier = Some("priority".into());
    let mut large = request("2026-10-04", "tiered", "large", None);
    large.record.input_tokens = 2_000_000;
    large.record.output_tokens = 0;
    let daily = UsageDailyRow {
        unreported_count: 0,
        day: "2026-10-03".into(),
        provider: "openai".into(),
        profile: Some("main".into()),
        model: "tiered".into(),
        project_id: Some("daily".into()),
        role: None,
        purpose: None,
        request_count: 4,
        failed_count: 0,
        input_tokens: 2_000_000,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: 0,
        cost_usd: 0.0,
        unpriced_input_tokens: 2_000_000,
        unpriced_output_tokens: 0,
        unpriced_cache_read_tokens: 0,
        unpriced_cache_write_tokens: 0,
        ttft_sum_ms: 0,
        ttft_count: 0,
        duration_sum_ms: 0,
    };
    let data = UsageDataset::from_rows(
        LocalDay::parse("2026-10-04").unwrap(),
        &[fast, large],
        &[daily],
    );

    // When: the facts are repriced at current rates.
    let (rows, _) = breakdown(
        &data.facts,
        &UsageFilter::default(),
        UsageDimension::Project,
        &mut CostCalculator::new(&pricing, CostMode::Current),
    );

    // Then: fast $5, the large request $6 at its tier, the daily bucket $2 at base rates.
    let costs: BTreeMap<_, _> = rows
        .iter()
        .map(|row| (row.key.as_str(), row.totals.cost))
        .collect();
    assert_eq!(
        costs,
        [("daily", 2.0), ("fast", 5.0), ("large", 6.0)].into()
    );
}

/// A request that finished without a reported usage: zero placeholder tokens, no cost.
fn unreported_request(project: &str) -> UsageRequestRow {
    let mut row = request("2026-10-04", "cheap", project, None);
    row.record.usage_reported = false;
    row.record.input_tokens = 0;
    row.record.output_tokens = 0;
    row
}

fn totals(data: &UsageDataset, mode: CostMode) -> UsageTotals {
    breakdown(
        &data.facts,
        &UsageFilter::default(),
        UsageDimension::Day,
        &mut CostCalculator::new(&pricing(), mode),
    )
    .1
}

// Given: a priced model with one request that reported no usage and one that reported zero
// When: totals are computed in both cost modes
// Then: the unreported request is $0 but marks the total as a lower bound and is counted; the
// reported zero stays a known $0 (no marker)
#[test]
fn unreported_requests_mark_totals_incomplete_in_both_cost_modes() {
    let unreported = dataset(&[unreported_request("a")]);
    let mut reported_zero = request("2026-10-04", "cheap", "b", Some(0.0));
    reported_zero.record.input_tokens = 0;
    reported_zero.record.output_tokens = 0;
    let reported = dataset(&[reported_zero]);

    for mode in [CostMode::Recorded, CostMode::Current] {
        let total = totals(&unreported, mode);
        assert_eq!(total.cost, 0.0, "{mode:?}");
        assert!(
            total.unpriced,
            "{mode:?}: unknown cost must not look complete"
        );
        assert_eq!(total.unreported_requests, 1);
        assert_eq!(total.requests, 1);

        let total = totals(&reported, mode);
        assert_eq!(total.cost, 0.0, "{mode:?}");
        assert!(!total.unpriced, "{mode:?}: a reported zero is a known zero");
        assert_eq!(total.unreported_requests, 0);
    }
}

// Given: a reported request next to an unreported one, plus a rolled-up day with unreported requests
// When: grouped by project / merged / priced
// Then: only the groups containing unreported requests are incomplete, and the rollup keeps its count
#[test]
fn unreported_counts_survive_grouping_merging_and_rollups() {
    let rollup = UsageDailyRow {
        day: "2026-09-01".into(),
        provider: "openai".into(),
        profile: Some("main".into()),
        model: "cheap".into(),
        project_id: Some("rolled".into()),
        request_count: 5,
        input_tokens: 1_000_000,
        cost_usd: 1.0,
        unreported_count: 2,
        ..UsageDailyRow::default()
    };
    let data = UsageDataset::from_rows(
        LocalDay::parse("2026-10-04").unwrap(),
        &[
            request("2026-10-04", "cheap", "ok", Some(3.0)),
            unreported_request("gap"),
        ],
        &[rollup],
    );
    for mode in [CostMode::Recorded, CostMode::Current] {
        let (rows, total) = breakdown(
            &data.facts,
            &UsageFilter::default(),
            UsageDimension::Project,
            &mut CostCalculator::new(&pricing(), mode),
        );
        let by_key: BTreeMap<_, _> = rows
            .iter()
            .map(|row| (row.key.as_str(), row.totals))
            .collect();
        assert!(!by_key["ok"].unpriced, "{mode:?}");
        assert!(by_key["gap"].unpriced && by_key["gap"].unreported_requests == 1);
        assert!(by_key["rolled"].unpriced && by_key["rolled"].unreported_requests == 2);
        assert_eq!(total.unreported_requests, 3);
        assert!(total.unpriced);
        let mut merged = UsageTotals::default();
        merged.merge(&by_key["ok"]);
        assert!(!merged.unpriced);
        merged.merge(&by_key["gap"]);
        assert!(merged.unpriced && merged.unreported_requests == 1);
    }
}
