//! The Live view of the Usage tab (ADR 0045 E4), drawn as the legacy Live tab draws it:
//! the recent pace, today's and this month's forecast, and the usage inside each
//! quota window.
//!
//! The core answers with minute buckets and the totals of today and the month, as of
//! one moment; the pace over the chosen window (15 minutes, an hour, three hours) and
//! the forecasts are computed here from them, as the legacy tab computes them from its
//! own data.
use egui::{Ui, vec2};
use egui_plot::{Bar, BarChart, Legend, Plot};
use workbench_protocol::namespaces::diagnostics::{
    LIVE_USAGE_MINUTES, LiveUsage, ProviderQuota, QuotaProvider, QuotaState, QuotaWindowUsage,
    UsageTotals,
};

use super::usage::{add, cells};
use crate::format::compact_count as compact_tokens;
use crate::model::local_day::LocalDay;
use crate::model::telemetry::pricing::PARTIAL_COST_HINT;
use crate::panes::usage_widgets::{
    CELL_LAYERS, SERIES, Tile, UsageCells, section, segment, stacked_bars, tiles,
};
use crate::theme::text::muted;

/// The recent windows to choose from, in minutes.
pub(super) const WINDOWS: [usize; 3] = [15, 60, LIVE_USAGE_MINUTES];

/// The window the view starts with, as the legacy tab does.
pub(super) const DEFAULT_WINDOW: usize = 60;

const HOUR_MS: f64 = 3_600_000.0;
const DAY_MS_FALLBACK: f64 = 86_400_000.0;

/// What the recent pace and the day and month so far add up to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Forecast {
    /// The recent window's totals.
    pub recent: UsageCells,
    pub burn_per_hour: f64,
    pub today: f64,
    /// Some of today's requests have no known price or reported no usage.
    pub today_lower_bound: bool,
    /// Today's cost if the recent pace continued until the local midnight.
    pub end_of_day: f64,
    pub month: f64,
    /// Some of the month's requests have no known price or reported no usage.
    pub month_lower_bound: bool,
    /// The month's cost per elapsed day, carried through the month.
    pub end_of_month: f64,
}

impl Forecast {
    /// The day's forecast is built from today's cost and the recent pace.
    pub fn end_of_day_lower_bound(&self) -> bool {
        self.today_lower_bound || self.recent.lower_bound
    }
}

/// The last `window` minute buckets, summed.
fn recent_totals(live: &LiveUsage, window: usize) -> UsageTotals {
    let window = window.min(live.minutes.len());
    let mut sum = UsageTotals::default();
    for bucket in &live.minutes[live.minutes.len() - window..] {
        add(&mut sum, bucket);
    }
    sum
}

pub(super) fn forecast(live: &LiveUsage, window: usize) -> Forecast {
    let recent = recent_totals(live, window);
    let recent_cells = cells(&recent);
    let burn_per_hour = recent_cells.cost * 60.0 / window.max(1) as f64;
    let today_cells = cells(&live.today);
    let month_cells = cells(&live.month);
    let (today, month) = (today_cells.cost, month_cells.cost);
    let day_length = live.day_end_ms.saturating_sub(live.day_start_ms) as f64;
    let day_length = if day_length > 0.0 {
        day_length
    } else {
        DAY_MS_FALLBACK
    };
    let hours_left = live.day_end_ms.saturating_sub(live.as_of_ms) as f64 / HOUR_MS;
    let elapsed_in_day =
        (live.as_of_ms.saturating_sub(live.day_start_ms) as f64 / day_length).min(1.0);
    let (elapsed_days, month_days) = match LocalDay::parse(&live.local_day) {
        Some(today) => {
            let month_start = today.month_start();
            let month_days = month_start
                .add_days(32)
                .month_start()
                .days_since(month_start);
            (
                today.days_since(month_start) as f64 + elapsed_in_day,
                month_days as f64,
            )
        }
        None => (0.0, 0.0),
    };
    Forecast {
        recent: recent_cells,
        burn_per_hour,
        today,
        today_lower_bound: today_cells.lower_bound,
        end_of_day: today + burn_per_hour * hours_left,
        month,
        month_lower_bound: month_cells.lower_bound,
        end_of_month: if elapsed_days > 0.0 {
            month / elapsed_days * month_days
        } else {
            month
        },
    }
}

fn usd(cost: f64) -> String {
    crate::format::usd(cost)
}

fn window_label(minutes: usize) -> String {
    if minutes >= 60 {
        format!("{}h", minutes / 60)
    } else {
        format!("{minutes}m")
    }
}

pub(super) fn render(ui: &mut Ui, live: &LiveUsage, window: &mut usize, quotas: &[ProviderQuota]) {
    ui.horizontal(|ui| {
        ui.label(muted("Window"));
        for minutes in WINDOWS {
            if segment(ui, *window == minutes, &window_label(minutes)) {
                *window = minutes;
            }
        }
    });
    let window = *window;
    let forecast = forecast(live, window);
    tile_row(ui, &forecast, window);
    let recent: Vec<UsageCells> = live.minutes[live.minutes.len() - window..]
        .iter()
        .map(cells)
        .collect();
    section(
        ui,
        "Requests per minute",
        "Provider attempts in each minute of the window, oldest on the left",
    );
    request_bars(ui, &recent, window);
    section(
        ui,
        "Tokens per minute",
        "Input split by cache use, plus output, per minute",
    );
    token_bars(ui, &recent, window);
    if !live.quota_windows.is_empty() {
        section(
            ui,
            "Subscription windows",
            "What each profile used inside its current quota window",
        );
        quota_table(ui, live, quotas);
    }
}

fn tile_row(ui: &mut Ui, forecast: &Forecast, window: usize) {
    tiles(ui, 170.0, tiles_of(forecast, window));
}

fn tiles_of(forecast: &Forecast, window: usize) -> Vec<Tile> {
    let recent = &forecast.recent;
    let star = |lower: bool| if lower { "*" } else { "" };
    let hint = |lower: bool| lower.then(|| PARTIAL_COST_HINT.to_owned());
    let pace_lower = forecast.recent.lower_bound;
    vec![
        Tile {
            title: "Burn rate".to_owned(),
            value: format!("${:.2}/h{}", forecast.burn_per_hour, star(pace_lower)),
            detail: format!(
                "{} tok/min",
                compact_tokens((recent.total_tokens() as f64 / window as f64) as u64)
            ),
            sparkline: None,
            hint: hint(pace_lower),
        },
        Tile {
            title: "In window".to_owned(),
            value: format!("{}{}", usd(recent.cost), star(recent.lower_bound)),
            detail: format!(
                "{} requests · {} tok",
                recent.requests,
                compact_tokens(recent.total_tokens())
            ),
            sparkline: None,
            hint: hint(recent.lower_bound),
        },
        Tile {
            title: "Today".to_owned(),
            value: format!(
                "{}{}",
                usd(forecast.today),
                star(forecast.today_lower_bound)
            ),
            detail: format!(
                "≈ {}{} by midnight at this pace",
                usd(forecast.end_of_day),
                star(forecast.end_of_day_lower_bound())
            ),
            sparkline: None,
            hint: hint(forecast.end_of_day_lower_bound()),
        },
        Tile {
            title: "This month".to_owned(),
            value: format!(
                "{}{}",
                usd(forecast.month),
                star(forecast.month_lower_bound)
            ),
            detail: format!(
                "≈ {}{} by month end",
                usd(forecast.end_of_month),
                star(forecast.month_lower_bound)
            ),
            sparkline: None,
            hint: hint(forecast.month_lower_bound),
        },
    ]
}

fn minute_plot(id: &str, window: usize) -> Plot<'static> {
    let window = window as i64;
    Plot::new(id)
        .height(150.0)
        .allow_zoom(false)
        .allow_drag(false)
        .allow_scroll(false)
        .allow_boxed_zoom(false)
        .allow_double_click_reset(false)
        .show_grid([false, true])
        .include_x(-0.5)
        .include_x(window as f64 - 0.5)
        .x_axis_formatter(move |mark, _| {
            let ago = window - mark.value.round() as i64;
            if mark.value.fract().abs() > 1e-6 || ago < 0 {
                String::new()
            } else if ago == 0 {
                "now".into()
            } else {
                format!("-{ago}m")
            }
        })
}

fn request_bars(ui: &mut Ui, minutes: &[UsageCells], window: usize) {
    let bars = minutes
        .iter()
        .enumerate()
        .map(|(index, cells)| Bar::new(index as f64, cells.requests as f64).width(0.7))
        .collect();
    let chart = BarChart::new("Requests", bars).color(SERIES[0]);
    minute_plot("usage-live-requests", window)
        .include_y(1.0)
        .y_axis_formatter(|mark, _| format!("{:.0}", mark.value))
        .show(ui, |plot| plot.bar_chart(chart));
}

fn token_bars(ui: &mut Ui, minutes: &[UsageCells], window: usize) {
    let window_len = window as i64;
    let charts = stacked_bars(minutes, &CELL_LAYERS, move |index| {
        format!("-{}m", window_len - index)
    });
    minute_plot("usage-live-tokens", window)
        .legend(Legend::default())
        .y_axis_formatter(|mark, _| compact_tokens(mark.value.max(0.0) as u64))
        .show(ui, |plot| {
            for chart in charts {
                plot.bar_chart(chart);
            }
        });
}

/// Scale only a matching Codex window. Below 1% the ratio is not meaningful,
/// as in the legacy Live view. Never combine a new reset period with old totals.
fn efficiency(
    usage: &QuotaWindowUsage,
    as_of_ms: u64,
    quotas: &[ProviderQuota],
) -> Option<(f64, f64)> {
    let quota = quotas.iter().find(|q| {
        q.profile == usage.profile
            && q.provider == QuotaProvider::Codex
            && matches!(q.state, QuotaState::Ready | QuotaState::Stale)
    })?;
    let window = quota.windows.iter().find(|w| w.id == usage.window)?;
    let used = 10_000_u16.checked_sub(window.remaining_basis_points?)?;
    let end = window.resets_at_ms?;
    let start = end.checked_sub(window.duration_seconds?.checked_mul(1000)?)?;
    let totals = cells(&usage.totals);
    if used < 100 || totals.total_tokens() == 0 || !(start..end).contains(&as_of_ms) {
        return None;
    }
    let percent = f64::from(used) / 100.0;
    Some((
        totals.total_tokens() as f64 / percent,
        totals.cost / percent,
    ))
}

fn quota_table(ui: &mut Ui, live: &LiveUsage, quotas: &[ProviderQuota]) {
    egui::Grid::new("usage-quota-windows")
        .striped(true)
        .spacing(vec2(16.0, 4.0))
        .show(ui, |ui| {
            for title in [
                "Subscription",
                "Window",
                "Tokens",
                "Cost",
                "Tokens / 1%",
                "Cost / 1%",
            ] {
                ui.label(muted(title));
            }
            ui.end_row();
            for window in &live.quota_windows {
                let used = cells(&window.totals);
                ui.label(&window.profile);
                ui.label(&window.window);
                ui.label(compact_tokens(used.total_tokens()));
                ui.label(used.cost_label());
                if let Some((tokens, cost)) = efficiency(window, live.as_of_ms, quotas) {
                    ui.label(compact_tokens(tokens as u64));
                    let response = ui.label(format!(
                        "{}{}",
                        usd(cost),
                        if used.lower_bound { "*" } else { "" }
                    ));
                    if used.lower_bound {
                        response.on_hover_text(PARTIAL_COST_HINT);
                    }
                } else {
                    ui.label("—");
                    ui.label("—");
                }
                ui.end_row();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE_MS: u64 = 60_000;

    fn spent(requests: u64, micro: u64) -> UsageTotals {
        UsageTotals {
            requests,
            input_tokens: 1_000 * requests,
            recorded_cost_micro_usd: micro,
            ..Default::default()
        }
    }

    /// 2026-02-10, noon on a 24 hour day, 2 dollars spent in the last 15 minutes and
    /// 1 dollar in the 45 minutes before that.
    fn live() -> LiveUsage {
        let day_start_ms = 1_770_681_600_000;
        let mut minutes = vec![UsageTotals::default(); LIVE_USAGE_MINUTES];
        minutes[LIVE_USAGE_MINUTES - 1] = spent(2, 2_000_000);
        minutes[LIVE_USAGE_MINUTES - 20] = spent(1, 1_000_000);
        LiveUsage {
            as_of_ms: day_start_ms + 12 * 60 * MINUTE_MS,
            minutes,
            local_day: "2026-02-10".into(),
            day_start_ms,
            day_end_ms: day_start_ms + 24 * 60 * MINUTE_MS,
            today: spent(3, 3_000_000),
            month: spent(3, 30_000_000),
            quota_windows: Vec::new(),
        }
    }

    // Given: spending that is 2 dollars in the last 15 minutes and 3 over the last hour
    // When: the forecast is made for each window
    // Then: the burn rate scales to an hour, and the day and the month are carried on at that pace
    #[test]
    fn the_pace_of_the_window_is_carried_to_midnight_and_the_month_end() {
        let live = live();
        let quarter = forecast(&live, 15);
        assert_eq!(quarter.recent.cost, 2.0);
        assert_eq!(quarter.recent.requests, 2);
        assert_eq!(quarter.burn_per_hour, 8.0);
        assert_eq!(quarter.today, 3.0);
        assert_eq!(quarter.end_of_day, 3.0 + 8.0 * 12.0);

        let hour = forecast(&live, 60);
        assert_eq!(hour.recent.cost, 3.0);
        assert_eq!(hour.burn_per_hour, 3.0);
        assert_eq!(hour.end_of_day, 3.0 + 3.0 * 12.0);

        // 9 full days and half of the tenth are elapsed in a 28 day month.
        assert_eq!(hour.month, 30.0);
        assert_eq!(hour.end_of_month, 30.0 / 9.5 * 28.0);
    }

    // Given: a day without spending / When: the forecast is made
    // Then: nothing is forecast and nothing divides by zero
    #[test]
    fn an_empty_ledger_forecasts_nothing() {
        let mut empty = live();
        empty.minutes = vec![UsageTotals::default(); LIVE_USAGE_MINUTES];
        empty.today = UsageTotals::default();
        empty.month = UsageTotals::default();
        let forecast = forecast(&empty, 180);
        assert_eq!(forecast.burn_per_hour, 0.0);
        assert_eq!(forecast.end_of_day, 0.0);
        assert_eq!(forecast.end_of_month, 0.0);
    }

    fn quota_fixture() -> (LiveUsage, Vec<ProviderQuota>) {
        use workbench_protocol::namespaces::diagnostics::QuotaWindow;
        let mut live = live();
        live.quota_windows.push(QuotaWindowUsage {
            profile: "work".into(),
            window: "primary".into(),
            totals: UsageTotals {
                input_tokens: 100_000,
                recorded_cost_micro_usd: 2_500_000,
                ..Default::default()
            },
        });
        let quotas = vec![ProviderQuota {
            profile: "work".into(),
            provider: QuotaProvider::Codex,
            state: QuotaState::Ready,
            fetched_at_ms: Some(live.as_of_ms),
            windows: vec![QuotaWindow {
                id: "primary".into(),
                label: "5h".into(),
                remaining_basis_points: Some(7500),
                resets_at_ms: Some(live.as_of_ms + 60 * MINUTE_MS),
                duration_seconds: Some(18000),
                usage_label: None,
            }],
            issue: None,
        }];
        (live, quotas)
    }

    #[test]
    fn quota_efficiency_scales_tokens_and_recorded_cost_at_one_percent_or_more() {
        let (mut live, mut quotas) = quota_fixture();
        let usage = &live.quota_windows[0];
        assert_eq!(
            efficiency(usage, live.as_of_ms, &quotas),
            Some((4000.0, 0.1))
        );
        for remaining in [Some(10_000), Some(9901), None] {
            quotas[0].windows[0].remaining_basis_points = remaining;
            assert_eq!(efficiency(usage, live.as_of_ms, &quotas), None);
        }
        quotas[0].windows[0].remaining_basis_points = Some(9900);
        assert_eq!(
            efficiency(usage, live.as_of_ms, &quotas),
            Some((100_000.0, 2.5))
        );
        quotas[0].windows[0].remaining_basis_points = Some(0);
        assert_eq!(
            efficiency(usage, live.as_of_ms, &quotas),
            Some((1000.0, 0.025))
        );
        live.quota_windows[0].totals.input_tokens = 0;
        assert_eq!(
            efficiency(&live.quota_windows[0], live.as_of_ms, &quotas),
            None
        );
    }

    #[test]
    fn efficiency_never_mixes_profiles_windows_or_reset_periods() {
        let (live, quotas) = quota_fixture();
        let usage = &live.quota_windows[0];
        for (profile, window, provider, state) in [
            (
                "personal",
                "primary",
                QuotaProvider::Codex,
                QuotaState::Ready,
            ),
            ("work", "secondary", QuotaProvider::Codex, QuotaState::Ready),
            ("work", "primary", QuotaProvider::Claude, QuotaState::Ready),
            ("work", "primary", QuotaProvider::Codex, QuotaState::Loading),
        ] {
            let mut wrong = quotas.clone();
            wrong[0].profile = profile.into();
            wrong[0].windows[0].id = window.into();
            wrong[0].provider = provider;
            wrong[0].state = state;
            assert_eq!(efficiency(usage, live.as_of_ms, &wrong), None);
        }
        let end = quotas[0].windows[0].resets_at_ms.unwrap();
        assert_eq!(efficiency(usage, end, &quotas), None);
        assert_eq!(efficiency(usage, end - 18000 * 1000 - 1, &quotas), None);
        let mut stale = quotas;
        stale[0].state = QuotaState::Stale;
        assert_eq!(
            efficiency(usage, live.as_of_ms, &stale),
            Some((4000.0, 0.1))
        );
    }

    #[test]
    fn efficiency_table_marks_lower_bound_cost_with_the_existing_hint() {
        use egui_kittest::{Harness, kittest::Queryable};
        let (mut live, quotas) = quota_fixture();
        live.quota_windows[0].totals.unpriced_tokens = 100;
        let mut screen = Harness::builder()
            .with_size(egui::vec2(1000.0, 220.0))
            .build_ui_state(
                |ui, (live, quotas)| {
                    ui.ctx().global_style_mut(|style| {
                        style.interaction.tooltip_delay = 0.0;
                        style.interaction.show_tooltips_only_when_still = false;
                    });
                    quota_table(ui, live, quotas);
                },
                (live, quotas),
            );
        screen.run_steps(2);
        screen.get_by_label("Tokens / 1%");
        screen.get_by_label("Cost / 1%");
        screen.get_by_label("4.0K");
        screen.get_by_label("$0.10*").hover();
        screen.run_steps(2);
        screen.get_by_label(PARTIAL_COST_HINT);
        screen.state_mut().1[0].windows[0].remaining_basis_points = None;
        screen.run_steps(2);
        assert!(screen.query_by_label("4.0K").is_none());
        assert!(screen.query_by_label("$0.10*").is_none());
    }

    fn tile<'a>(tiles: &'a [Tile], title: &str) -> &'a Tile {
        tiles.iter().find(|tile| tile.title == title).unwrap()
    }

    // Given: unpriced tokens two hours back, also counted in today and the month
    // When: the forecast is made for the hour and for the three hours
    // Then: the hour's pace is exact while today, the month and the day's forecast are lower
    //       bounds, and the three hours reach the unpriced tokens
    #[test]
    fn unpriced_tokens_outside_the_window_still_bound_the_day_and_the_month() {
        let mut live = live();
        live.minutes[LIVE_USAGE_MINUTES - 100].unpriced_tokens = 5;
        live.today.unpriced_tokens = 5;
        live.month.unpriced_tokens = 5;
        let hour = forecast(&live, 60);
        assert!(!hour.recent.lower_bound);
        assert!(hour.today_lower_bound && hour.month_lower_bound);
        assert!(hour.end_of_day_lower_bound());
        assert!(forecast(&live, 180).recent.lower_bound);

        let tiles = tiles_of(&hour, 60);
        assert_eq!(tile(&tiles, "Burn rate").value, "$3.00/h");
        assert_eq!(tile(&tiles, "In window").value, "$3.00");
        assert!(tile(&tiles, "Burn rate").hint.is_none());
        let today = tile(&tiles, "Today");
        assert_eq!(today.value, "$3.00*");
        assert_eq!(today.detail, "≈ $39.00* by midnight at this pace");
        assert_eq!(today.hint.as_deref(), Some(PARTIAL_COST_HINT));
        let month = tile(&tiles, "This month");
        assert_eq!(month.value, "$30.00*");
        assert_eq!(month.detail, "≈ $88.42* by month end");
        assert_eq!(month.hint.as_deref(), Some(PARTIAL_COST_HINT));
    }

    // Given: an unreported request inside the window / When: the tiles are made
    // Then: the pace, the window and the day's forecast say they are lower bounds, with a reason
    #[test]
    fn a_lower_bound_inside_the_window_marks_the_pace_and_the_forecast() {
        let mut live = live();
        live.minutes[LIVE_USAGE_MINUTES - 1].unreported_requests = 1;
        let tiles = tiles_of(&forecast(&live, 15), 15);
        assert_eq!(tile(&tiles, "Burn rate").value, "$8.00/h*");
        assert_eq!(tile(&tiles, "In window").value, "$2.00*");
        assert_eq!(
            tile(&tiles, "Today").detail,
            "≈ $99.00* by midnight at this pace"
        );
        assert_eq!(tile(&tiles, "Today").value, "$3.00");
        assert_eq!(
            tile(&tiles, "Burn rate").hint.as_deref(),
            Some(PARTIAL_COST_HINT)
        );
        assert_eq!(
            tile(&tiles, "In window").hint.as_deref(),
            Some(PARTIAL_COST_HINT)
        );
        assert!(tile(&tiles, "This month").hint.is_none());
        assert!(!tile(&tiles, "This month").value.ends_with('*'));
    }

    // Given: a month that holds unpriced tokens but a clean day / When: the tiles are made
    // Then: only the month and its forecast are lower bounds
    #[test]
    fn a_clean_day_stays_exact_when_only_the_month_has_unpriced_tokens() {
        let mut live = live();
        live.month.unpriced_tokens = 9;
        let forecast = forecast(&live, 60);
        assert!(!forecast.today_lower_bound && !forecast.end_of_day_lower_bound());
        assert!(forecast.month_lower_bound);
        let tiles = tiles_of(&forecast, 60);
        assert_eq!(tile(&tiles, "Today").value, "$3.00");
        assert!(tile(&tiles, "Today").hint.is_none());
        assert!(tile(&tiles, "This month").value.ends_with('*'));
    }
}
