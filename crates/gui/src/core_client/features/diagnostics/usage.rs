//! The Overview and Breakdown of the Usage tab, from the core's usage summary.
//!
//! The summary lists days with usage (oldest first) and models; the legacy Usage tab
//! reads every request instead. What it groups by that the summary does not carry
//! (project, thread, role, purpose) is not offered.
use std::collections::BTreeMap;

use egui::Ui;
use workbench_protocol::namespaces::diagnostics::{UsageSummary, UsageTotals};

use crate::model::local_day::LocalDay;
use crate::panes::usage_widgets::{
    self, ShareRow, SortColumn, TILE_WIDTH, TableRow, Tile, UsageCells, activity_grid,
    breakdown_table, daily_cost_chart, daily_token_chart, entity_color, other_color,
    overview_tiles_over, section, tiles,
};
use crate::theme::text::muted;

pub(super) fn cells(totals: &UsageTotals) -> UsageCells {
    #[allow(clippy::cast_precision_loss)]
    let cost = totals.recorded_cost_micro_usd as f64 / 1_000_000.0;
    UsageCells {
        requests: totals.requests,
        failed: totals.failed,
        input: totals.input_tokens,
        output: totals.output_tokens,
        cache_read: totals.cache_read_tokens,
        cache_write: totals.cache_write_tokens,
        reasoning: totals.reasoning_tokens,
        cost,
        lower_bound: totals.unpriced_tokens > 0 || totals.unreported_requests > 0,
        ttft_ms: None,
    }
}

pub(super) fn add(into: &mut UsageTotals, other: &UsageTotals) {
    into.requests += other.requests;
    into.failed += other.failed;
    into.input_tokens += other.input_tokens;
    into.output_tokens += other.output_tokens;
    into.cache_read_tokens += other.cache_read_tokens;
    into.cache_write_tokens += other.cache_write_tokens;
    into.reasoning_tokens += other.reasoning_tokens;
    into.recorded_cost_micro_usd += other.recorded_cost_micro_usd;
    into.unpriced_tokens += other.unpriced_tokens;
    into.unreported_requests += other.unreported_requests;
}

fn subtract(from: &UsageTotals, part: &UsageTotals) -> UsageTotals {
    UsageTotals {
        requests: from.requests.saturating_sub(part.requests),
        failed: from.failed.saturating_sub(part.failed),
        input_tokens: from.input_tokens.saturating_sub(part.input_tokens),
        output_tokens: from.output_tokens.saturating_sub(part.output_tokens),
        cache_read_tokens: from
            .cache_read_tokens
            .saturating_sub(part.cache_read_tokens),
        cache_write_tokens: from
            .cache_write_tokens
            .saturating_sub(part.cache_write_tokens),
        reasoning_tokens: from.reasoning_tokens.saturating_sub(part.reasoning_tokens),
        recorded_cost_micro_usd: from
            .recorded_cost_micro_usd
            .saturating_sub(part.recorded_cost_micro_usd),
        unpriced_tokens: from.unpriced_tokens.saturating_sub(part.unpriced_tokens),
        unreported_requests: from
            .unreported_requests
            .saturating_sub(part.unreported_requests),
    }
}

fn total_tokens(totals: &UsageTotals) -> u64 {
    totals.input_tokens + totals.output_tokens
}

fn cost_order(left: (&str, &UsageTotals), right: (&str, &UsageTotals)) -> std::cmp::Ordering {
    right
        .1
        .recorded_cost_micro_usd
        .cmp(&left.1.recorded_cost_micro_usd)
        .then_with(|| total_tokens(right.1).cmp(&total_tokens(left.1)))
        .then_with(|| left.0.cmp(right.0))
}

fn by_cost(left: &(String, UsageTotals), right: &(String, UsageTotals)) -> std::cmp::Ordering {
    cost_order((&left.0, &left.1), (&right.0, &right.1))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Group {
    #[default]
    Day,
    Week,
    Month,
    Provider,
    Model,
}

impl Group {
    pub(super) const ALL: [Self; 5] = [
        Self::Day,
        Self::Week,
        Self::Month,
        Self::Provider,
        Self::Model,
    ];

    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Day => "Day",
            Self::Week => "Week",
            Self::Month => "Month",
            Self::Provider => "Provider",
            Self::Model => "Model",
        }
    }

    const fn is_time(self) -> bool {
        matches!(self, Self::Day | Self::Week | Self::Month)
    }
}

pub(super) struct UsageData {
    pub summary: UsageSummary,
    listed: Vec<(LocalDay, UsageTotals)>,
    first: Option<LocalDay>,
    days: Vec<(LocalDay, UsageCells)>,
    range_days: usize,
}

impl UsageData {
    pub(super) fn new(summary: UsageSummary) -> Self {
        let listed: Vec<(LocalDay, UsageTotals)> = summary
            .days
            .iter()
            .filter_map(|day| Some((LocalDay::parse(&day.day)?, day.totals.clone())))
            .collect();
        let filled = fill_days(&summary, &listed);
        let range_days = match (
            LocalDay::parse(&summary.from_day),
            LocalDay::parse(&summary.to_day),
        ) {
            (Some(from), Some(to)) if to >= from => {
                usize::try_from(to.days_since(from) + 1).unwrap_or(filled.len())
            }
            _ => filled.len(),
        };
        Self {
            range_days,
            first: filled.first().map(|(day, _)| *day),
            days: filled
                .iter()
                .map(|(day, totals)| (*day, cells(totals)))
                .collect(),
            listed,
            summary,
        }
    }

    fn overview_tiles(&self) -> Vec<Tile> {
        overview_tiles_over(&cells(&self.summary.totals), &self.days, self.range_days)
    }

    pub(super) fn rows(&self, group: Group) -> Vec<TableRow> {
        if group.is_time() {
            let mut groups: BTreeMap<String, UsageTotals> = BTreeMap::new();
            for (day, totals) in &self.listed {
                let key = match group {
                    Group::Week => format!("Week of {}", day.week_start()),
                    Group::Month => day.month_label(),
                    _ => day.to_string(),
                };
                add(groups.entry(key).or_default(), totals);
            }
            return groups
                .into_iter()
                .rev()
                .map(|(label, totals)| TableRow {
                    label,
                    cells: cells(&totals),
                    children: Vec::new(),
                })
                .collect();
        }
        let mut children: BTreeMap<&str, Vec<(String, UsageTotals)>> = BTreeMap::new();
        if group == Group::Provider {
            for model in &self.summary.models {
                children
                    .entry(&model.provider)
                    .or_default()
                    .push((model.model.clone(), model.totals.clone()));
            }
        }
        self.grouped(group)
            .into_iter()
            .map(|(label, totals)| {
                let mut models = children.remove(label.as_str()).unwrap_or_default();
                models.sort_by(by_cost);
                TableRow {
                    cells: cells(&totals),
                    children: models
                        .iter()
                        .map(|(model, totals)| (model.clone(), cells(totals)))
                        .collect(),
                    label,
                }
            })
            .collect()
    }

    /// The five largest groups of `group` (Model or Provider) by cost, or by tokens when
    /// nothing is priced, and the rest folded into "Other" with the models the summary
    /// left out.
    pub(super) fn shares(&self, group: Group) -> Vec<ShareRow> {
        const LIMIT: usize = 5;
        let total = &self.summary.totals;
        let priced = total.recorded_cost_micro_usd > 0;
        #[allow(clippy::cast_precision_loss)]
        let weight = |totals: &UsageTotals| {
            if priced {
                totals.recorded_cost_micro_usd as f64
            } else {
                total_tokens(totals) as f64
            }
        };
        let whole = weight(total);
        let fraction = |totals: &UsageTotals| {
            if whole > 0.0 {
                weight(totals) / whole
            } else {
                0.0
            }
        };
        let mut ranked = self.grouped(group);
        // Colours follow the cost order, as the legacy tab's stable order does.
        let order: Vec<String> = ranked.iter().map(|(key, _)| key.clone()).collect();
        ranked.sort_by(|left, right| weight(&right.1).total_cmp(&weight(&left.1)));
        let folded = ranked.len() > LIMIT || self.summary.models_omitted > 0;
        let mut shown = UsageTotals::default();
        let mut rows = Vec::new();
        for (key, totals) in ranked.into_iter().take(LIMIT) {
            add(&mut shown, &totals);
            rows.push(ShareRow {
                color: entity_color(&order, &key),
                label: key,
                fraction: fraction(&totals),
                cells: cells(&totals),
            });
        }
        if folded {
            let rest = subtract(total, &shown);
            rows.push(ShareRow {
                label: "Other".to_owned(),
                color: other_color(),
                fraction: fraction(&rest),
                cells: cells(&rest),
            });
        }
        rows
    }

    fn grouped(&self, group: Group) -> Vec<(String, UsageTotals)> {
        let mut groups: BTreeMap<String, UsageTotals> = BTreeMap::new();
        for model in &self.summary.models {
            let key = if group == Group::Provider {
                &model.provider
            } else {
                &model.model
            };
            add(groups.entry(key.clone()).or_default(), &model.totals);
        }
        let mut groups: Vec<_> = groups.into_iter().collect();
        groups.sort_by(by_cost);
        groups
    }
}

/// Every day from the start of the range (or the oldest listed day when older ones
/// were left out) to its end, a day without usage counting zero.
fn fill_days(
    summary: &UsageSummary,
    listed: &[(LocalDay, UsageTotals)],
) -> Vec<(LocalDay, UsageTotals)> {
    let oldest = listed.first().map(|(day, _)| *day);
    let from = if summary.days_omitted > 0 {
        oldest
    } else {
        LocalDay::parse(&summary.from_day).or(oldest)
    };
    let to = LocalDay::parse(&summary.to_day).or_else(|| listed.last().map(|(day, _)| *day));
    let (Some(from), Some(to)) = (from, to) else {
        return listed.to_vec();
    };
    let mut listed = listed.iter().peekable();
    let mut days = Vec::new();
    let mut day = from;
    while day <= to {
        while listed.next_if(|(listed, _)| *listed < day).is_some() {}
        let totals = listed
            .next_if(|(listed, _)| *listed == day)
            .map(|(_, totals)| totals.clone())
            .unwrap_or_default();
        days.push((day, totals));
        day = day.add_days(1);
    }
    days
}

pub(super) fn render_overview(ui: &mut Ui, data: &UsageData, distribution: &mut Group) {
    tiles(ui, TILE_WIDTH, data.overview_tiles());
    if data.summary.days_omitted > 0 {
        ui.label(muted(format!(
            "{} older days are not shown.",
            data.summary.days_omitted
        )));
    }
    if let Some(first) = data.first {
        section(
            ui,
            "Daily tokens",
            "Input split by cache use, plus output; one bar per day",
        );
        daily_token_chart(ui, "core-usage-daily-tokens", first, &data.days);
        section(ui, "Daily cost", "Estimated cost per day in USD");
        daily_cost_chart(ui, "core-usage-daily-cost", first, &data.days);
        section(
            ui,
            "Activity",
            "Total tokens per day; darker cells used less",
        );
        activity_grid(ui, "core-usage-activity", &data.days);
    }
    section(
        ui,
        "Distribution",
        "Share of cost (or tokens when nothing is priced); the top five plus Other",
    );
    ui.horizontal(|ui| {
        ui.label(muted("By"));
        egui::ComboBox::from_id_salt("usage-distribution")
            .selected_text(distribution.label())
            .show_ui(ui, |ui| {
                for candidate in [Group::Model, Group::Provider] {
                    ui.selectable_value(distribution, candidate, candidate.label());
                }
            });
    });
    usage_widgets::share_list(
        ui,
        ("core-usage-shares", distribution.label()),
        &data.shares(*distribution),
    );
    models_note(ui, data);
}

pub(super) struct BreakdownControls<'a> {
    pub group: &'a mut Group,
    pub show_models: &'a mut bool,
    pub sort: &'a mut Option<(SortColumn, bool)>,
}

pub(super) fn render_breakdown(ui: &mut Ui, data: &UsageData, controls: BreakdownControls<'_>) {
    ui.horizontal_wrapped(|ui| {
        ui.label(muted("Group by"));
        egui::ComboBox::from_id_salt("usage-dimension")
            .selected_text(controls.group.label())
            .show_ui(ui, |ui| {
                for group in Group::ALL {
                    ui.selectable_value(&mut *controls.group, group, group.label());
                }
            });
        if *controls.group == Group::Provider {
            ui.checkbox(&mut *controls.show_models, "Model breakdown");
        }
    });
    ui.separator();
    let group = *controls.group;
    let rows = data.rows(group);
    egui::ScrollArea::horizontal()
        .id_salt("core-usage-breakdown-scroll")
        .show(ui, |ui| {
            breakdown_table(
                ui,
                "core-usage-breakdown",
                group.label(),
                &rows,
                &cells(&data.summary.totals),
                controls.sort,
                group == Group::Provider && *controls.show_models,
                false,
            );
        });
    if group.is_time() {
        if data.summary.days_omitted > 0 {
            ui.label(muted(format!(
                "{} older days are not shown.",
                data.summary.days_omitted
            )));
        }
    } else {
        models_note(ui, data);
    }
}

fn models_note(ui: &mut Ui, data: &UsageData) {
    if data.summary.models_omitted > 0 {
        ui.label(muted(format!(
            "{} less used models are not shown.",
            data.summary.models_omitted
        )));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workbench_protocol::namespaces::diagnostics::{UsageDay, UsageModel, UsageRange};

    fn totals(requests: u64, input: u64, micro_usd: u64) -> UsageTotals {
        UsageTotals {
            requests,
            input_tokens: input,
            output_tokens: 10,
            recorded_cost_micro_usd: micro_usd,
            ..Default::default()
        }
    }

    fn day(label: &str, totals: UsageTotals) -> UsageDay {
        UsageDay {
            day: label.into(),
            totals,
        }
    }

    fn model(provider: &str, name: &str, totals: UsageTotals) -> UsageModel {
        UsageModel {
            provider: provider.into(),
            model: name.into(),
            totals,
        }
    }

    fn summary(from: &str, to: &str, days: Vec<UsageDay>, models: Vec<UsageModel>) -> UsageSummary {
        let mut total = UsageTotals::default();
        for day in &days {
            add(&mut total, &day.totals);
        }
        UsageSummary {
            range: UsageRange::Last7Days,
            from_day: from.into(),
            to_day: to.into(),
            totals: total,
            days,
            days_omitted: 0,
            models,
            models_omitted: 0,
        }
    }

    fn day_labels(data: &UsageData) -> Vec<(String, u64)> {
        data.days
            .iter()
            .map(|(day, cells)| (day.to_string(), cells.requests))
            .collect()
    }

    #[test]
    fn days_without_usage_fill_the_range_and_omitted_days_start_it_at_the_oldest_listed() {
        let days = vec![
            day("2026-10-15", totals(2, 100, 0)),
            day("2026-10-17", totals(3, 100, 0)),
        ];
        let data = UsageData::new(summary("2026-10-14", "2026-10-18", days.clone(), vec![]));
        assert_eq!(
            day_labels(&data),
            [
                ("2026-10-14".into(), 0),
                ("2026-10-15".into(), 2),
                ("2026-10-16".into(), 0),
                ("2026-10-17".into(), 3),
                ("2026-10-18".into(), 0),
            ]
        );

        let mut cut = summary("2026-01-01", "2026-10-18", days, vec![]);
        cut.days_omitted = 40;
        assert_eq!(
            day_labels(&UsageData::new(cut))[0],
            ("2026-10-15".into(), 2)
        );

        let empty = UsageData::new(summary("2026-10-18", "2026-10-18", vec![], vec![]));
        assert_eq!(day_labels(&empty), [("2026-10-18".into(), 0)]);
    }

    #[test]
    fn the_cost_per_day_spans_the_whole_range_when_older_days_are_omitted() {
        let mut cut = summary(
            "2026-10-09",
            "2026-10-18",
            vec![day("2026-10-17", totals(1, 100, 2_000_000))],
            vec![],
        );
        cut.totals = totals(5, 500, 10_000_000);
        cut.days_omitted = 4;
        let data = UsageData::new(cut);
        assert_eq!(data.days.len(), 2);
        assert_eq!(data.overview_tiles()[0].detail, "$1.00 / day");
    }

    #[test]
    fn micro_dollars_read_as_the_legacy_costs_and_unknown_prices_make_a_lower_bound() {
        assert_eq!(cells(&totals(1, 0, 1_500_000)).cost_label(), "$1.50");
        assert_eq!(cells(&totals(1, 0, 4_200)).cost_label(), "$0.0042");
        let unpriced = UsageTotals {
            unpriced_tokens: 5,
            ..totals(1, 0, 1_500_000)
        };
        assert_eq!(cells(&unpriced).cost_label(), "$1.50*");
        let unreported = UsageTotals {
            unreported_requests: 1,
            ..totals(1, 0, 0)
        };
        assert_eq!(cells(&unreported).cost_label(), "$0.00*");

        let cache = UsageTotals {
            cache_read_tokens: 30,
            cache_write_tokens: 20,
            ..totals(1, 100, 0)
        };
        assert_eq!(cells(&cache).uncached_input(), 50);
        assert_eq!(cells(&totals(1, 0, 0)).cache_hit_rate(), None);
    }

    #[test]
    fn weeks_and_months_group_the_listed_days_newest_first() {
        let data = UsageData::new(summary(
            "2026-09-28",
            "2026-10-05",
            vec![
                day("2026-09-28", totals(1, 10, 0)),
                day("2026-10-04", totals(2, 10, 0)),
                day("2026-10-05", totals(4, 10, 0)),
            ],
            vec![],
        ));
        let rows = |group| {
            data.rows(group)
                .into_iter()
                .map(|row| (row.label, row.cells.requests))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            rows(Group::Day),
            [
                ("2026-10-05".into(), 4),
                ("2026-10-04".into(), 2),
                ("2026-09-28".into(), 1),
            ]
        );
        assert_eq!(
            rows(Group::Week),
            [
                ("Week of 2026-10-05".into(), 4),
                ("Week of 2026-09-28".into(), 3),
            ]
        );
        assert_eq!(
            rows(Group::Month),
            [("2026-10".into(), 6), ("2026-09".into(), 1)]
        );
    }

    #[test]
    fn providers_sum_their_models_most_expensive_first_and_models_sum_across_providers() {
        let data = UsageData::new(summary(
            "2026-10-14",
            "2026-10-14",
            vec![],
            vec![
                model("anthropic", "opus", totals(1, 10, 3_000_000)),
                model("openrouter", "opus", totals(2, 10, 500_000)),
                model("openrouter", "gpt", totals(4, 10, 1_000_000)),
            ],
        ));
        let providers = data.rows(Group::Provider);
        let view: Vec<_> = providers
            .iter()
            .map(|row| {
                (
                    row.label.as_str(),
                    row.cells.cost_label(),
                    row.children
                        .iter()
                        .map(|(model, _)| model.as_str())
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        assert_eq!(
            view,
            [
                ("anthropic", "$3.00".to_owned(), vec!["opus"]),
                ("openrouter", "$1.50".to_owned(), vec!["gpt", "opus"]),
            ]
        );
        let models: Vec<_> = data
            .rows(Group::Model)
            .into_iter()
            .map(|row| (row.label, row.cells.requests, row.children.len()))
            .collect();
        assert_eq!(models, [("opus".into(), 3, 0), ("gpt".into(), 4, 0)]);
    }

    fn shares_of(models: Vec<UsageModel>, omitted: u32) -> Vec<(String, String)> {
        let mut summary = summary("2026-10-14", "2026-10-14", vec![], models.clone());
        for model in &models {
            add(&mut summary.totals, &model.totals);
        }
        summary.models_omitted = omitted;
        UsageData::new(summary)
            .shares(Group::Model)
            .into_iter()
            .map(|row| (row.label, format!("{:.2}", row.fraction)))
            .collect()
    }

    #[test]
    fn the_five_largest_models_are_shown_and_the_rest_is_other() {
        let models: Vec<_> = (1..=6)
            .map(|n| model("p", &format!("m{n}"), totals(1, 10, n * 1_000_000)))
            .collect();
        let shares = shares_of(models, 0);
        let labels: Vec<_> = shares.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(labels, ["m6", "m5", "m4", "m3", "m2", "Other"]);
        assert_eq!(shares[0].1, format!("{:.2}", 6.0 / 21.0));
        assert_eq!(shares[5].1, format!("{:.2}", 1.0 / 21.0));
    }

    #[test]
    fn without_prices_shares_follow_tokens_and_omitted_models_fold_into_other() {
        let shares = shares_of(
            vec![
                model("p", "small", totals(1, 10, 0)),
                model("p", "large", totals(1, 90, 0)),
            ],
            2,
        );
        assert_eq!(
            shares,
            [
                ("large".to_owned(), "0.83".to_owned()),
                ("small".to_owned(), "0.17".to_owned()),
                ("Other".to_owned(), "0.00".to_owned()),
            ]
        );
    }

    #[test]
    fn a_group_keeps_its_colour_by_cost_order() {
        let data = UsageData::new(summary(
            "2026-10-14",
            "2026-10-14",
            vec![],
            vec![
                model("p", "cheap", totals(1, 10, 100)),
                model("p", "dear", totals(1, 10, 900)),
            ],
        ));
        let colours: Vec<_> = data
            .shares(Group::Model)
            .into_iter()
            .map(|row| (row.label, row.color))
            .collect();
        assert_eq!(
            colours,
            [
                ("dear".to_owned(), usage_widgets::SERIES[0]),
                ("cheap".to_owned(), usage_widgets::SERIES[1]),
            ]
        );
    }
}
