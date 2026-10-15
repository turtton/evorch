//! Shared, egui-independent display formatting for numbers, durations and
//! timestamps. Panes and view models use these helpers so the same quantity
//! reads the same way everywhere in the workbench.

use chrono::{DateTime, Utc};
use std::time::Duration;

/// Count with K/M/B suffixes: `950`, `12.4K`, `1.23M`, `4.56B`.
pub fn compact_count(value: u64) -> String {
    let value = value as f64;
    if value >= 1e9 {
        format!("{:.2}B", value / 1e9)
    } else if value >= 1e6 {
        format!("{:.2}M", value / 1e6)
    } else if value >= 1e3 {
        format!("{:.1}K", value / 1e3)
    } else {
        format!("{value:.0}")
    }
}

/// Latency such as TTFT: milliseconds below one second, seconds from there
/// (`840ms`, `1.2s`) so four-digit millisecond counts stay readable.
pub fn latency(ms: f64) -> String {
    if ms.round() < 1_000.0 {
        format!("{ms:.0}ms")
    } else {
        format!("{:.1}s", ms / 1_000.0)
    }
}

/// Human duration with at most three units, dropping seconds once the value
/// reaches an hour and omitting zero units: `45s`, `12m 34s`, `5h 30m`,
/// `1d 5h 30m`, `2d`.
pub fn duration(value: Duration) -> String {
    let total = value.as_secs();
    let (days, hours, minutes, seconds) = (
        total / 86_400,
        total % 86_400 / 3_600,
        total % 3_600 / 60,
        total % 60,
    );
    let units: &[(u64, &str)] = if total >= 3_600 {
        &[(days, "d"), (hours, "h"), (minutes, "m")]
    } else {
        &[(minutes, "m"), (seconds, "s")]
    };
    let parts: Vec<String> = units
        .iter()
        .filter(|(amount, _)| *amount > 0)
        .map(|(amount, unit)| format!("{amount}{unit}"))
        .collect();
    if parts.is_empty() {
        "0s".into()
    } else {
        parts.join(" ")
    }
}

/// Time left until `target`, e.g. `in 1d 5h 30m`; `now` once it has passed.
pub fn time_until(target: DateTime<Utc>, now: DateTime<Utc>) -> String {
    match (target - now).to_std() {
        Ok(left) if left.as_secs() > 0 => format!("in {}", duration(left)),
        _ => "now".into(),
    }
}

/// Elapsed time since an event, e.g. `5m 12s ago`; `just now` below a second.
pub fn time_ago(age: Duration) -> String {
    if age.as_secs() == 0 {
        "just now".into()
    } else {
        format!("{} ago", duration(age))
    }
}

/// Absolute UTC timestamp for tooltips next to a relative time.
pub fn utc_timestamp(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%d %H:%M UTC").to_string()
}

/// USD amount; sub-cent values keep four decimals so they don't read as zero.
pub fn usd(cost: f64) -> String {
    if cost > 0.0 && cost < 0.01 {
        format!("${cost:.4}")
    } else {
        format!("${cost:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_keep_the_most_significant_units() {
        let cases = [
            (0, "0s"),
            (45, "45s"),
            (60, "1m"),
            (754, "12m 34s"),
            (3_600, "1h"),
            (3_600 * 5 + 30 * 60 + 59, "5h 30m"),
            (86_400 + 5 * 3_600 + 30 * 60, "1d 5h 30m"),
            (2 * 86_400, "2d"),
            (86_400 + 30 * 60, "1d 30m"),
        ];
        for (seconds, expected) in cases {
            assert_eq!(
                duration(Duration::from_secs(seconds)),
                expected,
                "{seconds}s"
            );
        }
    }

    #[test]
    fn relative_times_name_direction_and_clamp_at_zero() {
        let now = DateTime::<Utc>::from_timestamp(1_760_000_000, 0).unwrap();
        let later = now + chrono::Duration::seconds(86_400 + 5 * 3_600 + 30 * 60);
        assert_eq!(time_until(later, now), "in 1d 5h 30m");
        assert_eq!(time_until(now, later), "now");
        assert_eq!(time_ago(Duration::from_secs(3_742)), "1h 2m ago");
        assert_eq!(time_ago(Duration::from_millis(400)), "just now");
    }

    #[test]
    fn counts_and_money_stay_compact() {
        assert_eq!(compact_count(950), "950");
        assert_eq!(compact_count(12_400), "12.4K");
        assert_eq!(compact_count(1_234_567), "1.23M");
        assert_eq!(usd(0.0042), "$0.0042");
        assert_eq!(usd(1.5), "$1.50");
        assert_eq!(latency(840.0), "840ms");
        assert_eq!(latency(1_234.0), "1.2s");
    }
}
