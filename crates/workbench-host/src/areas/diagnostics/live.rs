//! The live usage window (ADR 0045 E4): the last three hours by minute, today and
//! the month, read from the ledger in one pass. Nothing is kept between two
//! reads; every read costs what the ledger of the month costs, as the usage tab's
//! own refresh does. Forecasts and the choice of a shorter window are the
//! client's.
use super::{add_daily, add_request, consistent_rows, day_label, day_number, merge};
use crate::queries::storage_error;
use storage::{
    Database,
    usage::{LocalClock, UsageDailyRow, UsageRequestRow},
};
use workbench_protocol::{
    errors::{ErrorCode, WorkbenchError},
    namespaces::diagnostics::{
        LIVE_USAGE_MINUTES, LiveUsage, MAX_QUOTA_WINDOW_USAGES, ProviderQuotas, QuotaProvider,
        QuotaState, QuotaWindowUsage, UsageTotals,
    },
};

const MINUTE_NS: i64 = 60_000_000_000;

pub(super) fn live_usage_with_quotas(
    database: &Database,
    now_ns: i64,
    quotas: &ProviderQuotas,
) -> Result<LiveUsage, WorkbenchError> {
    let ranges = quota_ranges(quotas, now_ns);
    let clock = database.usage_local_clock().map_err(storage_error)?;
    let (mut from, month_start) = read_range(&clock.today).ok_or_else(|| {
        crate::queries::error(
            ErrorCode::StorageUnavailable,
            "the ledger's day is unreadable",
        )
    })?;
    if let Some(start) = ranges.iter().map(|r| r.from_ns).min() {
        // A conservative local-day bound (all UTC offsets), then exact ns filtering.
        let first = day_label(start.div_euclid(86_400_000_000_000) - 1);
        from = from.min(first);
    }
    let (requests, daily) = consistent_rows(
        || database.usage_daily_between(&from, &clock.today),
        || database.usage_requests_in_days(&from, &clock.today),
    )?;
    let mut live = reduce_live(&requests, &daily, &clock, &month_start, now_ns);
    live.quota_windows = sum_quota_windows(&requests, &ranges);
    Ok(live)
}

struct QuotaRange {
    profile: String,
    window: String,
    from_ns: i64,
    until_ns: i64,
}
fn quota_ranges(quotas: &ProviderQuotas, now_ns: i64) -> Vec<QuotaRange> {
    quotas
        .profiles
        .iter()
        .filter(|q| {
            q.provider == QuotaProvider::Codex
                && matches!(q.state, QuotaState::Ready | QuotaState::Stale)
        })
        .flat_map(|q| {
            q.windows.iter().filter_map(move |w| {
                let end = i64::try_from(w.resets_at_ms?.checked_mul(1_000_000)?).ok()?;
                let duration =
                    i64::try_from(w.duration_seconds?.checked_mul(1_000_000_000)?).ok()?;
                if duration <= 0 {
                    return None;
                }
                let from = end.checked_sub(duration)?;
                let until = end.min(now_ns);
                (from < until).then(|| QuotaRange {
                    profile: q.profile.clone(),
                    window: w.id.clone(),
                    from_ns: from,
                    until_ns: until,
                })
            })
        })
        .take(MAX_QUOTA_WINDOW_USAGES)
        .collect()
}
fn sum_quota_windows(requests: &[UsageRequestRow], ranges: &[QuotaRange]) -> Vec<QuotaWindowUsage> {
    ranges
        .iter()
        .map(|range| {
            let mut totals = UsageTotals::default();
            for row in requests.iter().filter(|row| {
                row.record.profile.as_deref() == Some(range.profile.as_str())
                    && (range.from_ns..range.until_ns).contains(&row.record.at_ns)
            }) {
                add_request(&mut totals, row);
            }
            // Rolled-up daily rows lost profile identity: never attribute them to
            // another account, even if their provider/model happens to match.
            QuotaWindowUsage {
                profile: range.profile.clone(),
                window: range.window.clone(),
                totals,
            }
        })
        .collect()
}

/// The first day to read and the first day of the month. The window of the last
/// three hours may begin on the day before the month does, so the read starts
/// there when that is earlier.
fn read_range(today: &str) -> Option<(String, String)> {
    let month_start = format!("{}-01", today.get(..7)?);
    let yesterday = day_label(day_number(today)? - 1);
    let from = if yesterday < month_start {
        yesterday
    } else {
        month_start.clone()
    };
    Some((from, month_start))
}

fn at_ms(ns: i64) -> u64 {
    u64::try_from(ns / 1_000_000).unwrap_or(0)
}

/// The pure reduction of the ledger rows of [`read_range`] as of `now_ns`.
fn reduce_live(
    requests: &[UsageRequestRow],
    daily: &[UsageDailyRow],
    clock: &LocalClock,
    month_start: &str,
    now_ns: i64,
) -> LiveUsage {
    let window_start = now_ns - MINUTE_NS * LIVE_USAGE_MINUTES as i64;
    let mut minutes = vec![UsageTotals::default(); LIVE_USAGE_MINUTES];
    let mut today = UsageTotals::default();
    let mut month = UsageTotals::default();
    let in_month = |day: &str| day >= month_start && day <= clock.today.as_str();
    for row in requests {
        let mut one = UsageTotals::default();
        add_request(&mut one, row);
        let at = row.record.at_ns;
        if (window_start..now_ns).contains(&at) {
            let index = usize::try_from((at - window_start) / MINUTE_NS).unwrap_or(0);
            merge(&mut minutes[index.min(LIVE_USAGE_MINUTES - 1)], &one);
        }
        if in_month(&row.day) {
            merge(&mut month, &one);
        }
        if row.day == clock.today {
            merge(&mut today, &one);
        }
    }
    for row in daily {
        let mut one = UsageTotals::default();
        add_daily(&mut one, row);
        if in_month(&row.day) {
            merge(&mut month, &one);
        }
        if row.day == clock.today {
            merge(&mut today, &one);
        }
    }
    LiveUsage {
        as_of_ms: at_ms(now_ns),
        minutes,
        local_day: clock.today.clone(),
        day_start_ms: at_ms(clock.day_start_ns),
        day_end_ms: at_ms(clock.day_end_ns),
        today,
        month,
        quota_windows: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use storage::usage::{UsageRequestRecord, UsageStatus};

    const HOUR_NS: i64 = 60 * MINUTE_NS;
    /// 2026-03-01 00:00 on the ledger's clock, the first day of a month.
    const DAY_START: i64 = 1_772_323_200_000_000_000;

    fn clock(today: &str, start: i64) -> LocalClock {
        LocalClock {
            today: today.into(),
            day_start_ns: start,
            day_end_ns: start + 24 * HOUR_NS,
        }
    }

    fn request(day: &str, at_ns: i64, cost: f64) -> UsageRequestRow {
        UsageRequestRow {
            record: UsageRequestRecord {
                request_id: format!("{day}-{at_ns}"),
                at_ns,
                provider: "main".into(),
                profile: None,
                model: "model".into(),
                run_id: None,
                parent_run_id: None,
                role: None,
                purpose: None,
                status: UsageStatus::Ok,
                failure: None,
                finish_reason: None,
                input_tokens: 10,
                output_tokens: 1,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: None,
                ttft_ms: None,
                duration_ms: 1,
                cost_usd: Some(cost),
                usage_reported: true,
                service_tier: None,
            },
            day: day.into(),
            time: "00:00:00".into(),
            thread_id: None,
            project_id: None,
        }
    }

    fn daily(day: &str, cost: f64) -> UsageDailyRow {
        UsageDailyRow {
            day: day.into(),
            provider: "main".into(),
            model: "model".into(),
            request_count: 2,
            input_tokens: 100,
            cost_usd: cost,
            ..Default::default()
        }
    }

    #[test]
    fn quota_read_extends_before_the_month_in_the_database() {
        use workbench_protocol::namespaces::diagnostics::*;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.db");
        let store = storage::Storage::open(storage::StorageConfig {
            db_path: path.clone(),
            ..Default::default()
        })
        .unwrap();
        let database = Database::open_read_only(&path).unwrap();
        let clock = database.usage_local_clock().unwrap();
        let now = clock.day_start_ns + HOUR_NS;
        let mut row = request("unused", clock.day_start_ns - 35 * 24 * HOUR_NS, 2.0).record;
        row.profile = Some("work".into());
        let writer = store.handle();
        writer.record_usage_requests(vec![row]).unwrap();
        writer.flush_usage_now().unwrap();
        let quotas = ProviderQuotas {
            profiles: vec![ProviderQuota {
                profile: "work".into(),
                provider: QuotaProvider::Codex,
                state: QuotaState::Ready,
                fetched_at_ms: Some(1),
                windows: vec![QuotaWindow {
                    id: "primary".into(),
                    label: "45d".into(),
                    remaining_basis_points: Some(5000),
                    resets_at_ms: Some(at_ms(clock.day_end_ns)),
                    duration_seconds: Some(45 * 86400),
                    usage_label: None,
                }],
                issue: None,
            }],
        };
        let live = live_usage_with_quotas(&database, now, &quotas).unwrap();
        assert_eq!(live.month.requests, 0);
        assert_eq!(
            live.quota_windows[0].totals.recorded_cost_micro_usd,
            2_000_000
        );
    }

    #[test]
    fn codex_windows_keep_profiles_separate_and_include_pre_month_requests() {
        use workbench_protocol::namespaces::diagnostics::*;
        let now = DAY_START + HOUR_NS;
        let quotas = ProviderQuotas {
            profiles: vec![ProviderQuota {
                profile: "work".into(),
                provider: QuotaProvider::Codex,
                state: QuotaState::Ready,
                fetched_at_ms: Some(1),
                windows: vec![QuotaWindow {
                    id: "primary".into(),
                    label: "7d".into(),
                    remaining_basis_points: Some(5000),
                    resets_at_ms: Some(at_ms(DAY_START + 24 * HOUR_NS)),
                    duration_seconds: Some(7 * 86400),
                    usage_label: None,
                }],
                issue: None,
            }],
        };
        let ranges = quota_ranges(&quotas, now);
        assert!(ranges[0].from_ns < DAY_START);
        let mut rows = vec![
            request("2026-02-28", DAY_START - HOUR_NS, 2.0),
            request("2026-03-01", now - 1, 3.0),
            request("2026-03-01", now - 1, 99.0),
            request("2026-03-01", now, 99.0),
        ];
        for row in &mut rows {
            row.record.profile = Some("work".into());
        }
        rows[2].record.profile = Some("other".into());
        let summed = sum_quota_windows(&rows, &ranges);
        assert_eq!(summed[0].totals.requests, 2);
        assert_eq!(summed[0].totals.recorded_cost_micro_usd, 5_000_000);
        assert_eq!(summed[0].window, "primary");
        let mut quotas = quotas;
        quotas.profiles[0].provider = QuotaProvider::Claude;
        assert!(quota_ranges(&quotas, now).is_empty());
    }

    #[test]
    fn minutes_are_oldest_first_and_end_at_the_moment_of_the_read() {
        let now = DAY_START + 10 * HOUR_NS;
        let window_start = now - 3 * HOUR_NS;
        let rows = [
            request("2026-03-01", window_start - 1, 9.0),
            request("2026-03-01", window_start, 1.0),
            request("2026-03-01", window_start + MINUTE_NS - 1, 1.0),
            request("2026-03-01", now - 1, 2.0),
            request("2026-03-01", now, 9.0),
        ];
        let live = reduce_live(
            &rows,
            &[],
            &clock("2026-03-01", DAY_START),
            "2026-03-01",
            now,
        );
        assert_eq!(live.minutes.len(), LIVE_USAGE_MINUTES);
        assert_eq!(live.minutes[0].requests, 2, "the first minute, both edges");
        assert_eq!(live.minutes[0].recorded_cost_micro_usd, 2_000_000);
        assert_eq!(live.minutes[LIVE_USAGE_MINUTES - 1].requests, 1);
        let counted: u64 = live.minutes.iter().map(|m| m.requests).sum();
        assert_eq!(
            counted, 3,
            "a request before the window or at its end is outside"
        );
        assert_eq!(live.as_of_ms, u64::try_from(now / 1_000_000).unwrap());
        assert!(live.validate().is_ok());
    }

    #[test]
    fn an_empty_ledger_is_an_empty_window() {
        let live = reduce_live(
            &[],
            &[],
            &clock("2026-03-01", DAY_START),
            "2026-03-01",
            DAY_START,
        );
        assert!(live.minutes.iter().all(|m| *m == UsageTotals::default()));
        assert_eq!(live.today, UsageTotals::default());
        assert_eq!(live.month, UsageTotals::default());
        assert!(live.quota_windows.is_empty());
        assert!(live.validate().is_ok());
    }

    #[test]
    fn today_and_the_month_follow_the_ledgers_days_not_the_window() {
        // 00:30 on the first of March: the window reaches back into February.
        let now = DAY_START + 30 * MINUTE_NS;
        let rows = [
            request("2026-02-28", DAY_START - 24 * HOUR_NS, 5.0),
            request("2026-02-28", DAY_START - 10 * MINUTE_NS, 1.0),
            request("2026-03-01", DAY_START + 5 * MINUTE_NS, 2.0),
        ];
        let rolled_up = [daily("2026-02-27", 7.0), daily("2026-03-01", 3.0)];
        let live = reduce_live(
            &rows,
            &rolled_up,
            &clock("2026-03-01", DAY_START),
            "2026-03-01",
            now,
        );
        let window: u64 = live.minutes.iter().map(|m| m.requests).sum();
        assert_eq!(window, 2, "the window crosses the month boundary");
        assert_eq!(
            live.today.requests, 3,
            "one request and one rolled-up day of two"
        );
        assert_eq!(live.today.recorded_cost_micro_usd, 5_000_000);
        assert_eq!(live.month, live.today, "February is not this month");
        assert_eq!(live.local_day, "2026-03-01");
        assert_eq!(live.day_end_ms - live.day_start_ms, 24 * 3_600_000);
    }

    #[test]
    fn the_month_sums_every_day_of_it_and_yesterday_is_not_today() {
        let start = DAY_START + 14 * 24 * HOUR_NS;
        let now = start + 12 * HOUR_NS;
        let rows = [
            request("2026-03-01", DAY_START + HOUR_NS, 1.0),
            request("2026-03-14", start - HOUR_NS, 1.0),
            request("2026-03-15", start + HOUR_NS, 1.0),
        ];
        let rolled_up = [daily("2026-03-02", 4.0)];
        let live = reduce_live(
            &rows,
            &rolled_up,
            &clock("2026-03-15", start),
            "2026-03-01",
            now,
        );
        assert_eq!(live.today.requests, 1);
        assert_eq!(live.month.requests, 5);
        assert_eq!(live.month.recorded_cost_micro_usd, 7_000_000);
    }

    #[test]
    fn the_read_starts_on_the_earlier_of_yesterday_and_the_first_of_the_month() {
        let range = |today| read_range(today).unwrap();
        assert_eq!(
            range("2026-03-01"),
            ("2026-02-28".into(), "2026-03-01".into())
        );
        assert_eq!(
            range("2026-03-02"),
            ("2026-03-01".into(), "2026-03-01".into())
        );
        assert_eq!(
            range("2026-03-20"),
            ("2026-03-01".into(), "2026-03-01".into())
        );
        assert_eq!(
            range("2025-01-01"),
            ("2024-12-31".into(), "2025-01-01".into())
        );
        assert!(read_range("bad").is_none());
    }
}
