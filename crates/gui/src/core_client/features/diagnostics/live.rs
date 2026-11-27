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
use workbench_protocol::namespaces::diagnostics::{LIVE_USAGE_MINUTES, LiveUsage, UsageTotals};

use super::usage::{add, cells};
use crate::format::compact_count as compact_tokens;
use crate::model::local_day::LocalDay;
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
    /// Today's cost if the recent pace continued until the local midnight.
    pub end_of_day: f64,
    pub month: f64,
    /// The month's cost per elapsed day, carried through the month.
    pub end_of_month: f64,
}

fn cost_of(totals: &UsageTotals) -> f64 {
    cells(totals).cost
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
    let today = cost_of(&live.today);
    let month = cost_of(&live.month);
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
        end_of_day: today + burn_per_hour * hours_left,
        month,
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

pub(super) fn render(ui: &mut Ui, live: &LiveUsage, window: &mut usize) {
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
        quota_table(ui, live);
    }
}

fn tile_row(ui: &mut Ui, forecast: &Forecast, window: usize) {
    let recent = &forecast.recent;
    let marker = if recent.lower_bound { "*" } else { "" };
    let tiles_to_show = vec![
        Tile {
            title: "Burn rate".to_owned(),
            value: format!("${:.2}/h", forecast.burn_per_hour),
            detail: format!(
                "{} tok/min",
                compact_tokens((recent.total_tokens() as f64 / window as f64) as u64)
            ),
            sparkline: None,
        },
        Tile {
            title: "In window".to_owned(),
            value: format!("{}{marker}", usd(recent.cost)),
            detail: format!(
                "{} requests · {} tok",
                recent.requests,
                compact_tokens(recent.total_tokens())
            ),
            sparkline: None,
        },
        Tile {
            title: "Today".to_owned(),
            value: usd(forecast.today),
            detail: format!("≈ {} by midnight at this pace", usd(forecast.end_of_day)),
            sparkline: None,
        },
        Tile {
            title: "This month".to_owned(),
            value: usd(forecast.month),
            detail: format!("≈ {} by month end", usd(forecast.end_of_month)),
            sparkline: None,
        },
    ];
    tiles(ui, 170.0, tiles_to_show);
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

fn quota_table(ui: &mut Ui, live: &LiveUsage) {
    egui::Grid::new("usage-quota-windows")
        .striped(true)
        .spacing(vec2(16.0, 4.0))
        .show(ui, |ui| {
            for title in ["Subscription", "Window", "Tokens", "Cost"] {
                ui.label(muted(title));
            }
            ui.end_row();
            for window in &live.quota_windows {
                let used = cells(&window.totals);
                ui.label(&window.profile);
                ui.label(&window.window);
                ui.label(compact_tokens(used.total_tokens()));
                ui.label(used.cost_label());
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

    // Given: unpriced tokens two hours back / When: the forecast is made
    // Then: only the windows that reach them are a lower bound
    #[test]
    fn unpriced_tokens_make_the_window_a_lower_bound() {
        let mut live = live();
        live.minutes[LIVE_USAGE_MINUTES - 100].unpriced_tokens = 5;
        assert!(!forecast(&live, 60).recent.lower_bound);
        assert!(forecast(&live, 180).recent.lower_bound);
    }
}
