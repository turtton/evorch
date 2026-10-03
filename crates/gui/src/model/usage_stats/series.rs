//! Chart series derived from usage facts: daily totals, shares, matrices and latency.

use std::collections::BTreeMap;

use super::{CostCalculator, LocalDay, UsageDimension, UsageFact, UsageFilter, UsageTotals};

/// Daily totals for every day in `[first, last]`, including days without usage.
pub fn daily_totals(
    facts: &[UsageFact],
    filter: &UsageFilter,
    costs: &mut CostCalculator<'_>,
    first: LocalDay,
    last: LocalDay,
) -> Vec<(LocalDay, UsageTotals)> {
    let mut days: BTreeMap<LocalDay, UsageTotals> = BTreeMap::new();
    for fact in facts.iter().filter(|fact| filter.matches(fact)) {
        if fact.day < first || fact.day > last {
            continue;
        }
        let cost = costs.cost(fact);
        days.entry(fact.day).or_default().add(fact, cost);
    }
    (0..=last.days_since(first))
        .map(|offset| {
            let day = first.add_days(offset);
            (day, days.get(&day).copied().unwrap_or_default())
        })
        .collect()
}

/// One slice of a part-to-whole view. `key` is `None` for the folded tail.
#[derive(Debug, Clone, PartialEq)]
pub struct Share {
    pub key: Option<String>,
    pub totals: UsageTotals,
    /// Fraction of the whole by cost, or by tokens when nothing is priced.
    pub fraction: f64,
}

/// The `limit` largest groups by cost, then by tokens, with the rest folded
/// into one trailing slice.
pub fn top_shares(
    facts: &[UsageFact],
    filter: &UsageFilter,
    dimension: UsageDimension,
    costs: &mut CostCalculator<'_>,
    limit: usize,
) -> Vec<Share> {
    let (rows, total) = super::breakdown(facts, filter, dimension, costs);
    let by_cost = total.cost > 0.0;
    let weight = |totals: &UsageTotals| {
        if by_cost {
            totals.cost
        } else {
            totals.total_tokens() as f64
        }
    };
    let whole = weight(&total);
    let mut ranked: Vec<_> = rows.into_iter().map(|row| (row.key, row.totals)).collect();
    ranked.sort_by(|left, right| weight(&right.1).total_cmp(&weight(&left.1)));
    let mut shares = Vec::new();
    let mut rest = UsageTotals::default();
    let mut folded = 0;
    for (index, (key, totals)) in ranked.into_iter().enumerate() {
        if index < limit {
            shares.push(Share {
                key: Some(key),
                totals,
                fraction: if whole > 0.0 {
                    weight(&totals) / whole
                } else {
                    0.0
                },
            });
        } else {
            rest.merge(&totals);
            folded += 1;
        }
    }
    if folded > 0 {
        shares.push(Share {
            key: None,
            totals: rest,
            fraction: if whole > 0.0 {
                weight(&rest) / whole
            } else {
                0.0
            },
        });
    }
    shares
}

/// Keys of `dimension` ordered by descending cost over all facts. Charts index
/// their categorical colors by this order so filters never repaint an entity.
pub fn stable_order(
    facts: &[UsageFact],
    dimension: UsageDimension,
    costs: &mut CostCalculator<'_>,
) -> Vec<String> {
    let (mut rows, _) = super::breakdown(facts, &UsageFilter::default(), dimension, costs);
    rows.sort_by(|left, right| {
        right
            .totals
            .cost
            .total_cmp(&left.totals.cost)
            .then_with(|| right.totals.total_tokens().cmp(&left.totals.total_tokens()))
            .then_with(|| left.key.cmp(&right.key))
    });
    rows.into_iter().map(|row| row.key).collect()
}

/// Totals for each (row, column) pair, limited to the largest keys by cost.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Matrix {
    pub rows: Vec<String>,
    pub columns: Vec<String>,
    /// `cells[row][column]`.
    pub cells: Vec<Vec<UsageTotals>>,
}

pub fn matrix(
    facts: &[UsageFact],
    filter: &UsageFilter,
    rows: UsageDimension,
    columns: UsageDimension,
    costs: &mut CostCalculator<'_>,
    limit: usize,
) -> Matrix {
    let ranked = |dimension, costs: &mut CostCalculator<'_>| {
        let (mut keys, _) = super::breakdown(facts, filter, dimension, costs);
        keys.truncate(limit);
        keys.into_iter().map(|row| row.key).collect::<Vec<_>>()
    };
    let row_keys = ranked(rows, costs);
    let column_keys = ranked(columns, costs);
    let mut cells = vec![vec![UsageTotals::default(); column_keys.len()]; row_keys.len()];
    for fact in facts.iter().filter(|fact| filter.matches(fact)) {
        let (Some(row), Some(column)) = (
            row_keys.iter().position(|key| *key == rows.key(fact)),
            column_keys.iter().position(|key| *key == columns.key(fact)),
        ) else {
            continue;
        };
        let cost = costs.cost(fact);
        cells[row][column].add(fact, cost);
    }
    Matrix {
        rows: row_keys,
        columns: column_keys,
        cells,
    }
}

/// One request's first-token and total latency, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatencySample {
    pub ttft_ms: f64,
    pub duration_ms: f64,
    /// Output tokens per second over the whole request, when measurable.
    pub tokens_per_second: Option<f64>,
}

/// Successful streamed requests with a first token; rolled-up days are excluded.
pub fn latency_samples(facts: &[UsageFact], filter: &UsageFilter) -> Vec<LatencySample> {
    facts
        .iter()
        .filter(|fact| {
            fact.at_ns.is_some() && fact.ttft_count == 1 && fact.failed == 0 && filter.matches(fact)
        })
        .map(|fact| LatencySample {
            ttft_ms: fact.ttft_sum_ms as f64,
            duration_ms: fact.duration_sum_ms as f64,
            tokens_per_second: (fact.duration_sum_ms > 0 && fact.tokens.output > 0)
                .then(|| fact.tokens.output as f64 / (fact.duration_sum_ms as f64 / 1_000.0)),
        })
        .collect()
}

/// Nearest-rank percentile of `values`; `None` when empty.
pub fn percentile(values: impl IntoIterator<Item = f64>, quantile: f64) -> Option<f64> {
    let mut values: Vec<f64> = values
        .into_iter()
        .filter(|value| value.is_finite())
        .collect();
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let rank = (quantile.clamp(0.0, 1.0) * values.len() as f64).ceil() as usize;
    Some(values[rank.clamp(1, values.len()) - 1])
}

const MINUTE_NS: i64 = 60_000_000_000;

/// Per-minute totals for the `minutes` minutes ending at `now_ns`, oldest first.
pub fn minute_totals(
    facts: &[UsageFact],
    filter: &UsageFilter,
    costs: &mut CostCalculator<'_>,
    now_ns: i64,
    minutes: usize,
) -> Vec<UsageTotals> {
    let mut buckets = vec![UsageTotals::default(); minutes];
    let start = now_ns - MINUTE_NS * minutes as i64;
    for fact in facts.iter().filter(|fact| filter.matches(fact)) {
        let Some(at) = fact.at_ns.filter(|at| (start..now_ns).contains(at)) else {
            continue;
        };
        let index = ((at - start) / MINUTE_NS) as usize;
        let cost = costs.cost(fact);
        buckets[index.min(minutes - 1)].add(fact, cost);
    }
    buckets
}

/// Totals of individual requests in `[from_ns, until_ns)`.
pub fn totals_between(
    facts: &[UsageFact],
    filter: &UsageFilter,
    costs: &mut CostCalculator<'_>,
    from_ns: i64,
    until_ns: i64,
) -> UsageTotals {
    let mut totals = UsageTotals::default();
    for fact in facts.iter().filter(|fact| filter.matches(fact)) {
        if fact
            .at_ns
            .is_some_and(|at| (from_ns..until_ns).contains(&at))
        {
            let cost = costs.cost(fact);
            totals.add(fact, cost);
        }
    }
    totals
}

/// Cost so far and where the current pace leads.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Projection {
    pub today: f64,
    /// Cost per hour over the recent window.
    pub burn_per_hour: f64,
    /// Today's cost if the recent pace continues until local midnight.
    pub end_of_day: f64,
    pub month: f64,
    /// Month-to-date cost per elapsed day, carried through the month.
    pub end_of_month: f64,
}

#[allow(clippy::too_many_arguments)]
pub fn projection(
    facts: &[UsageFact],
    filter: &UsageFilter,
    costs: &mut CostCalculator<'_>,
    today: LocalDay,
    (day_start_ns, day_end_ns): (i64, i64),
    now_ns: i64,
    window_minutes: i64,
) -> Projection {
    let today_cost = totals_between(facts, filter, costs, day_start_ns, day_end_ns).cost;
    let recent = totals_between(
        facts,
        filter,
        costs,
        now_ns - MINUTE_NS * window_minutes,
        now_ns + 1,
    );
    let burn_per_hour = recent.cost * 60.0 / window_minutes as f64;
    let hours_left = (day_end_ns - now_ns).max(0) as f64 / (MINUTE_NS * 60) as f64;
    let month_start = today.month_start();
    let mut month = 0.0;
    for fact in facts.iter().filter(|fact| filter.matches(fact)) {
        if fact.day >= month_start && fact.day <= today {
            month += costs.cost(fact).usd;
        }
    }
    let day_length = (day_end_ns - day_start_ns).max(1) as f64;
    let elapsed_days = today.days_since(month_start) as f64
        + ((now_ns - day_start_ns).max(0) as f64 / day_length).min(1.0);
    let month_days = month_start
        .add_days(32)
        .month_start()
        .days_since(month_start) as f64;
    Projection {
        today: today_cost,
        burn_per_hour,
        end_of_day: today_cost + burn_per_hour * hours_left,
        month,
        end_of_month: if elapsed_days > 0.0 {
            month / elapsed_days * month_days
        } else {
            month
        },
    }
}

/// Usage a subscription window has consumed, scaled to its quota percentage.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuotaEfficiency {
    pub used_percent: f64,
    pub totals: UsageTotals,
}

impl QuotaEfficiency {
    /// Too little of the quota is used to scale it meaningfully.
    pub fn is_reliable(&self) -> bool {
        self.used_percent >= 1.0 && self.totals.total_tokens() > 0
    }

    pub fn tokens_per_percent(&self) -> f64 {
        self.totals.total_tokens() as f64 / self.used_percent
    }

    pub fn cost_per_percent(&self) -> f64 {
        self.totals.cost / self.used_percent
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::telemetry::TokenUsage;
    use crate::model::telemetry::pricing::UsagePricing;
    use crate::model::usage_stats::CostMode;

    fn fact(day: &str, model: &str, project: &str, cost: f64, ttft: Option<u64>) -> UsageFact {
        UsageFact {
            day: LocalDay::parse(day).unwrap(),
            at_ns: Some(0),
            provider: "openai".into(),
            profile: None,
            model: model.into(),
            project_id: Some(project.into()),
            thread_id: None,
            role: None,
            purpose: None,
            requests: 1,
            failed: 0,
            tokens: TokenUsage {
                input: 100,
                output: 50,
                cache_read: 0,
                cache_write: 0,
            },
            reasoning: 0,
            recorded_cost: cost,
            unpriced: TokenUsage::default(),
            ttft_sum_ms: ttft.unwrap_or_default(),
            ttft_count: u64::from(ttft.is_some()),
            duration_sum_ms: 2_000,
            request: None,
        }
    }

    #[test]
    fn daily_totals_fill_days_without_usage() {
        let pricing = UsagePricing::default();
        let mut costs = CostCalculator::new(&pricing, CostMode::Recorded);
        let facts = [
            fact("2026-10-01", "a", "p", 1.0, None),
            fact("2026-10-03", "a", "p", 2.0, None),
        ];
        let days = daily_totals(
            &facts,
            &UsageFilter::default(),
            &mut costs,
            LocalDay::parse("2026-10-01").unwrap(),
            LocalDay::parse("2026-10-04").unwrap(),
        );
        let costs: Vec<_> = days.iter().map(|(_, totals)| totals.cost).collect();
        assert_eq!(costs, [1.0, 0.0, 2.0, 0.0]);
    }

    #[test]
    fn shares_keep_the_largest_and_fold_the_rest() {
        let pricing = UsagePricing::default();
        let mut costs = CostCalculator::new(&pricing, CostMode::Recorded);
        let facts = [
            fact("2026-10-01", "a", "p", 6.0, None),
            fact("2026-10-01", "b", "p", 3.0, None),
            fact("2026-10-01", "c", "p", 0.5, None),
            fact("2026-10-01", "d", "p", 0.5, None),
        ];
        let shares = top_shares(
            &facts,
            &UsageFilter::default(),
            UsageDimension::Model,
            &mut costs,
            2,
        );
        let summary: Vec<_> = shares
            .iter()
            .map(|share| (share.key.clone(), share.fraction))
            .collect();
        assert_eq!(
            summary,
            [
                (Some("a".into()), 0.6),
                (Some("b".into()), 0.3),
                (None, 0.1)
            ]
        );
    }

    #[test]
    fn stable_order_ignores_filters_so_colors_follow_entities() {
        let pricing = UsagePricing::default();
        let mut costs = CostCalculator::new(&pricing, CostMode::Recorded);
        let facts = [
            fact("2026-10-01", "cheap", "p", 1.0, None),
            fact("2026-10-01", "pricey", "q", 5.0, None),
        ];
        assert_eq!(
            stable_order(&facts, UsageDimension::Model, &mut costs),
            ["pricey", "cheap"]
        );
    }

    #[test]
    fn matrix_places_costs_by_row_and_column() {
        let pricing = UsagePricing::default();
        let mut costs = CostCalculator::new(&pricing, CostMode::Recorded);
        let facts = [
            fact("2026-10-01", "m1", "p1", 4.0, None),
            fact("2026-10-01", "m2", "p1", 1.0, None),
            fact("2026-10-01", "m1", "p2", 2.0, None),
        ];
        let grid = matrix(
            &facts,
            &UsageFilter::default(),
            UsageDimension::Project,
            UsageDimension::Model,
            &mut costs,
            8,
        );
        assert_eq!(grid.rows, ["p1", "p2"]);
        assert_eq!(grid.columns, ["m1", "m2"]);
        let cells: Vec<Vec<f64>> = grid
            .cells
            .iter()
            .map(|row| row.iter().map(|totals| totals.cost).collect())
            .collect();
        assert_eq!(cells, [vec![4.0, 1.0], vec![2.0, 0.0]]);
    }

    #[test]
    fn latency_uses_single_requests_and_nearest_rank_percentiles() {
        let mut rolled_up = fact("2026-10-01", "a", "p", 0.0, Some(900));
        rolled_up.at_ns = None;
        let facts = [
            fact("2026-10-01", "a", "p", 0.0, Some(100)),
            fact("2026-10-01", "a", "p", 0.0, None),
            rolled_up,
        ];
        let samples = latency_samples(&facts, &UsageFilter::default());
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].tokens_per_second, Some(25.0));
        assert_eq!(percentile([5.0, 1.0, 3.0, 2.0, 4.0], 0.95), Some(5.0));
        assert_eq!(percentile([5.0, 1.0, 3.0, 2.0, 4.0], 0.5), Some(3.0));
        assert_eq!(percentile([], 0.5), None);
    }

    #[test]
    fn minute_buckets_and_projection_follow_the_recent_pace() {
        let pricing = UsagePricing::default();
        let mut costs = CostCalculator::new(&pricing, CostMode::Recorded);
        let minute = 60_000_000_000_i64;
        let day_start = 0;
        let now = 12 * 60 * minute;
        let at = |minutes_ago: i64, cost: f64| {
            let mut fact = fact("2026-10-14", "a", "p", cost, None);
            fact.at_ns = Some(now - minutes_ago * minute);
            fact
        };
        let mut earlier = fact("2026-10-01", "a", "p", 4.0, None);
        earlier.at_ns = None;
        let facts = [earlier, at(600, 2.0), at(30, 0.5), at(1, 0.5)];

        let minutes = minute_totals(&facts, &UsageFilter::default(), &mut costs, now, 60);
        let busy: Vec<_> = minutes
            .iter()
            .enumerate()
            .filter(|(_, totals)| totals.requests > 0)
            .map(|(index, _)| index)
            .collect();
        assert_eq!(busy, [30, 59]);

        let projection = projection(
            &facts,
            &UsageFilter::default(),
            &mut costs,
            LocalDay::parse("2026-10-14").unwrap(),
            (day_start, 24 * 60 * minute),
            now,
            60,
        );
        assert_eq!(projection.today, 3.0);
        assert_eq!(projection.burn_per_hour, 1.0);
        assert_eq!(projection.end_of_day, 15.0);
        assert_eq!(projection.month, 7.0);
        // 13.5 elapsed days of a 31-day month.
        assert!((projection.end_of_month - 7.0 / 13.5 * 31.0).abs() < 1e-9);
    }
}
