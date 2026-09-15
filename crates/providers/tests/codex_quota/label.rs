use providers::provider::codex::quota::QuotaWindow;
use std::time::Duration;

fn window(seconds: u64) -> QuotaWindow {
    QuotaWindow {
        used_percent: 0.0,
        remaining_percent: 100.0,
        window_duration: Duration::from_secs(seconds),
        resets_at: "2026-09-13T12:00:00Z".parse().expect("timestamp"),
    }
}

#[test]
fn duration_label_maps_window_length_to_compact_unit() {
    for (seconds, expected) in [
        (18000, "5h"),
        (604800, "7d"),
        (3600, "1h"),
        (86400, "1d"),
        (1209600, "14d"),
        (1500, "25m"),
        (90, "90s"),
    ] {
        assert_eq!(window(seconds).duration_label(), expected);
    }
}
