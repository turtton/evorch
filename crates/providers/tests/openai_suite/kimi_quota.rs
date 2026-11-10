use providers::{
    ProviderAuth,
    provider::{
        codex::quota::QuotaError,
        kimi_quota::{KimiQuotaClient, KimiQuotaSnapshot},
    },
};
use serde_json::json;
use std::time::Duration;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

async fn fetch_fixture(body: serde_json::Value) -> Result<KimiQuotaSnapshot, QuotaError> {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/usages"))
        .and(header("Authorization", "Bearer fixture-key"))
        .and(header("Accept", "application/json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(1)
        .mount(&server)
        .await;
    let client = KimiQuotaClient::new(&server.uri(), Duration::from_secs(2)).unwrap();
    client.fetch_quota(&ProviderAuth::new("fixture-key")).await
}

fn assert_windows(snapshot: &KimiQuotaSnapshot, expected: &[(&str, f64, f64)]) {
    assert!(!snapshot.stale);
    assert!(snapshot.last_error.is_none());
    assert_eq!(snapshot.windows.len(), expected.len());
    for (window, &(label, used, remaining)) in snapshot.windows.iter().zip(expected) {
        assert_eq!(window.label, label);
        assert!(
            (window.used_percent - used).abs() < 1e-9,
            "{label}: expected {used}% used, got {}",
            window.used_percent
        );
        assert!(
            (window.remaining_percent - remaining).abs() < 1e-9,
            "{label}: expected {remaining}% remaining, got {}",
            window.remaining_percent
        );
    }
}

#[tokio::test]
async fn named_windows_preserve_monthly_limits_and_use_configured_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/coding/v1/usages"))
        .and(header("Authorization", "Bearer fixture-key"))
        .and(header("Accept", "application/json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "usages": {
                "limit_5h": {"used_ratio": 0.3, "reset_time": "2026-10-01T12:00:00Z"},
                "limit_7d": {"used_ratio": "0.2"},
                "limit_month_total": {"used_ratio": 0.4},
                "limit_month_code": {"used_ratio": 0.25}
            },
            "usage": {"limit":"100", "used":"99"}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let client = KimiQuotaClient::new(
        &format!("{}/coding/v1/", server.uri()),
        Duration::from_secs(2),
    )
    .unwrap();
    let snapshot = client
        .fetch_quota(&ProviderAuth::new("fixture-key"))
        .await
        .unwrap();
    assert_eq!(
        snapshot
            .windows
            .iter()
            .map(|window| (window.label.as_str(), window.remaining_percent))
            .collect::<Vec<_>>(),
        [
            ("5h", 99.7),
            ("wk", 99.8),
            ("month", 99.6),
            ("code month", 99.75)
        ]
    );
    assert!(snapshot.windows[0].resets_at.is_some());
    assert!(snapshot.windows[1].resets_at.is_none());
}

#[tokio::test]
async fn named_windows_use_percentage_values_without_ratio_conversion() {
    for (used_ratio, used_percent, remaining_percent) in [
        (json!(0), 0.0, 100.0),
        (json!(100), 100.0, 0.0),
        (json!(42), 42.0, 58.0),
        (json!(0.5), 0.5, 99.5),
        (json!("0.3"), 0.3, 99.7),
        (json!(1.0), 1.0, 99.0),
        (json!(1), 1.0, 99.0),
        (json!("1.0"), 1.0, 99.0),
        (json!("42"), 42.0, 58.0),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/usages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "usages": {"limit_5h": {"used_ratio": used_ratio}}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = KimiQuotaClient::new(&server.uri(), Duration::from_secs(2)).unwrap();
        let snapshot = client
            .fetch_quota(&ProviderAuth::new("fixture-key"))
            .await
            .unwrap();
        assert_eq!(snapshot.windows.len(), 1);
        let window = &snapshot.windows[0];
        assert_eq!(
            (window.used_percent, window.remaining_percent),
            (used_percent, remaining_percent),
            "used_ratio: {used_ratio}"
        );
    }
}

#[tokio::test]
async fn real_mixed_response_corrects_zero_windows_using_same_second_legacy_resets() {
    let snapshot = fetch_fixture(json!({
        "usage": {
            "limit": "100",
            "used": "88",
            "remaining": "12",
            "resetTime": "2026-10-06T13:43:44.982282Z"
        },
        "limits": [{
            "window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"},
            "detail": {
                "limit": "100",
                "used": "59",
                "remaining": "41",
                "resetTime": "2026-10-03T16:43:44.982282Z"
            }
        }],
        "usages": {
            "limit_5h": {"used_ratio": 0, "reset_time": "2026-10-03T16:43:44Z"},
            "limit_7d": {"used_ratio": 0, "reset_time": "2026-10-06T13:43:44Z"}
        }
    }))
    .await
    .unwrap();
    // Exactly these two windows: no monthly window may be inferred from legacy usage.
    assert_windows(&snapshot, &[("5h", 59.0, 41.0), ("wk", 88.0, 12.0)]);
    for (window, reset) in snapshot
        .windows
        .iter()
        .zip(["2026-10-03T16:43:44.982282Z", "2026-10-06T13:43:44.982282Z"])
    {
        assert_eq!(window.resets_at, Some(reset.parse().unwrap()));
    }
}

#[tokio::test]
async fn matching_named_and_legacy_zero_usage_stays_zero() {
    let snapshot = fetch_fixture(json!({
        "usages": {
            "limit_5h": {"used_ratio": 0, "reset_time": "2026-10-03T16:43:44Z"},
            "limit_7d": {"used_ratio": 0, "reset_time": "2026-10-06T13:43:44Z"}
        },
        "limits": [{
            "window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"},
            "detail": {"limit": "100", "used": "0", "remaining": "100",
                "resetTime": "2026-10-03T16:43:44.982282Z"}
        }],
        "usage": {"limit": "100", "used": "0", "remaining": "100",
            "resetTime": "2026-10-06T13:43:44.982282Z"}
    }))
    .await
    .unwrap();
    assert_windows(&snapshot, &[("5h", 0.0, 100.0), ("wk", 0.0, 100.0)]);
    assert_eq!(
        snapshot.windows[0].resets_at,
        Some("2026-10-03T16:43:44Z".parse().unwrap())
    );
}

#[tokio::test]
async fn named_only_windows_can_all_have_zero_usage() {
    let snapshot = fetch_fixture(json!({
        "usages": {
            "limit_5h": {"used_ratio": 0},
            "limit_7d": {"used_ratio": 0},
            "limit_month_total": {"used_ratio": 0},
            "limit_month_code": {"used_ratio": 0}
        }
    }))
    .await
    .unwrap();
    assert_windows(
        &snapshot,
        &[
            ("5h", 0.0, 100.0),
            ("wk", 0.0, 100.0),
            ("month", 0.0, 100.0),
            ("code month", 0.0, 100.0),
        ],
    );
}

#[tokio::test]
async fn legacy_nonzero_does_not_override_named_zero_with_different_resets() {
    let snapshot = fetch_fixture(json!({
        "usages": {
            "limit_5h": {"used_ratio": 0, "reset_time": "2026-10-03T16:43:44Z"},
            "limit_7d": {"used_ratio": 0, "reset_time": "2026-10-06T13:43:44Z"}
        },
        "limits": [{
            "window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"},
            "detail": {"limit": 100, "used": 59,
                "resetTime": "2026-10-03T16:43:45Z"}
        }],
        "usage": {"limit": 100, "used": 88, "resetTime": "2026-10-06T13:43:45Z"}
    }))
    .await
    .unwrap();
    assert_windows(&snapshot, &[("5h", 0.0, 100.0), ("wk", 0.0, 100.0)]);
}

#[tokio::test]
async fn legacy_nonzero_does_not_override_named_zero_when_either_reset_is_missing() {
    for (named_reset, legacy_reset) in [(false, true), (true, false), (false, false)] {
        let mut body = json!({
            "usages": {
                "limit_5h": {"used_ratio": 0, "reset_time": "2026-10-03T16:43:44Z"},
                "limit_7d": {"used_ratio": 0, "reset_time": "2026-10-06T13:43:44Z"}
            },
            "limits": [{
                "window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"},
                "detail": {"limit": 100, "used": 59,
                    "resetTime": "2026-10-03T16:43:44Z"}
            }],
            "usage": {"limit": 100, "used": 88, "resetTime": "2026-10-06T13:43:44Z"}
        });
        if !named_reset {
            for key in ["limit_5h", "limit_7d"] {
                body["usages"][key]
                    .as_object_mut()
                    .unwrap()
                    .remove("reset_time");
            }
        }
        if !legacy_reset {
            body["limits"][0]["detail"]
                .as_object_mut()
                .unwrap()
                .remove("resetTime");
            body["usage"].as_object_mut().unwrap().remove("resetTime");
        }
        let snapshot = fetch_fixture(body).await.unwrap();
        assert_windows(&snapshot, &[("5h", 0.0, 100.0), ("wk", 0.0, 100.0)]);
    }
}

#[tokio::test]
async fn correcting_five_hour_zero_preserves_named_monthly_usage() {
    let snapshot = fetch_fixture(json!({
        "usages": {
            "limit_5h": {"used_ratio": 0, "reset_time": "2026-10-03T16:43:44Z"},
            "limit_month_total": {"used_ratio": 42},
            "limit_month_code": {"used_ratio": 0.25}
        },
        "limits": [{
            "window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"},
            "detail": {"limit": 100, "used": 59,
                "resetTime": "2026-10-03T16:43:44.982282Z"}
        }]
    }))
    .await
    .unwrap();
    assert_windows(
        &snapshot,
        &[
            ("5h", 59.0, 41.0),
            ("month", 42.0, 58.0),
            ("code month", 0.25, 99.75),
        ],
    );
}

#[tokio::test]
async fn nonzero_named_usage_wins_over_larger_legacy_usage_with_matching_resets() {
    let snapshot = fetch_fixture(json!({
        "usages": {
            "limit_5h": {"used_ratio": 0.2, "reset_time": "2026-10-03T16:43:44Z"},
            "limit_7d": {"used_ratio": 0.1, "reset_time": "2026-10-06T13:43:44Z"}
        },
        "limits": [{
            "window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"},
            "detail": {"limit": 100, "used": 90,
                "resetTime": "2026-10-03T16:43:44.982282Z"}
        }],
        "usage": {"limit": 100, "used": 99, "resetTime": "2026-10-06T13:43:44.982282Z"}
    }))
    .await
    .unwrap();
    assert_windows(&snapshot, &[("5h", 0.2, 99.8), ("wk", 0.1, 99.9)]);
}

#[tokio::test]
async fn empty_or_all_missing_named_windows_fall_back_to_legacy() {
    for usages in [
        json!({}),
        json!(null),
        json!({"unrecognized_window": {"used_ratio": 0}}),
        json!({"limit_5h": null, "limit_7d": null,
            "limit_month_total": null, "limit_month_code": null}),
    ] {
        let snapshot = fetch_fixture(json!({
            "usages": usages,
            "limits": [{
                "window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"},
                "detail": {"limit": "100", "remaining": "41"}
            }],
            "usage": {"limit": "100", "used": "88", "remaining": "12"}
        }))
        .await
        .unwrap();
        assert_windows(&snapshot, &[("5h", 59.0, 41.0), ("wk", 88.0, 12.0)]);
    }
}

fn missing_or_invalid_named_entries() -> [serde_json::Value; 8] {
    [
        json!({}),
        json!({"used_ratio": null}),
        json!({"used_ratio": "NaN"}),
        json!({"used_ratio": "inf"}),
        json!({"used_ratio": "invalid"}),
        json!({"used_ratio": -0.1}),
        json!({"used_ratio": true}),
        json!({"used_ratio": []}),
    ]
}

#[tokio::test]
async fn missing_or_invalid_named_ratios_use_valid_legacy_candidates_for_the_same_period() {
    for entry in missing_or_invalid_named_entries() {
        let snapshot = fetch_fixture(json!({
            "usages": {"limit_5h": entry, "limit_7d": entry},
            "limits": [
                {"window": {"duration": "5", "timeUnit": "TIME_UNIT_HOUR"},
                    "detail": {"limit": "100", "used": "59"}},
                {"window": {"duration": "7", "timeUnit": "TIME_UNIT_DAY"},
                    "detail": {"limit": "100", "remaining": "12"}}
            ]
        }))
        .await
        .unwrap();
        assert_windows(&snapshot, &[("5h", 59.0, 41.0), ("wk", 88.0, 12.0)]);
    }
}

#[tokio::test]
async fn missing_or_invalid_weekly_ratios_can_use_reset_matched_legacy_usage() {
    for mut entry in missing_or_invalid_named_entries() {
        entry["reset_time"] = json!("2026-10-06T13:43:44Z");
        let snapshot = fetch_fixture(json!({
            "usages": {"limit_7d": entry},
            "usage": {"limit": 100, "used": 88, "resetTime": "2026-10-06T13:43:44.982282Z"}
        }))
        .await
        .unwrap();
        assert_windows(&snapshot, &[("wk", 88.0, 12.0)]);
    }
}

#[tokio::test]
async fn missing_or_invalid_named_ratios_without_fallback_are_protocol_errors() {
    for entry in missing_or_invalid_named_entries() {
        for key in [
            "limit_5h",
            "limit_7d",
            "limit_month_total",
            "limit_month_code",
        ] {
            let result = fetch_fixture(json!({"usages": {key: entry}})).await;
            assert!(
                matches!(result, Err(QuotaError::Protocol(_))),
                "{key}: {entry}: {result:?}"
            );
        }
    }
}

#[tokio::test]
async fn invalid_weekly_ratio_cannot_use_unmatched_or_ambiguous_legacy_usage() {
    let reset = "2026-10-06T13:43:44Z";
    for (named_reset, legacy_reset, other_reset) in [
        (Some(reset), Some("2026-10-06T13:43:45Z"), None),
        (None, Some(reset), None),
        (Some(reset), None, None),
        (Some(reset), Some(reset), Some(reset)),
    ] {
        let body = json!({
            "usages": {
                "limit_5h": {"used_ratio": 0.25, "reset_time": other_reset},
                "limit_7d": {"reset_time": named_reset}
            },
            "usage": {"limit": 100, "used": 88, "resetTime": legacy_reset}
        });
        let result = fetch_fixture(body.clone()).await;
        // A valid 5h window must not hide the invalid weekly window or invent its quota.
        assert!(
            matches!(result, Err(QuotaError::Protocol(_))),
            "{body}: {result:?}"
        );
    }
}

#[tokio::test]
async fn invalid_unselected_legacy_candidates_do_not_reject_valid_named_usage() {
    for detail in [
        json!({"limit": 0, "used": 0}),
        json!({"limit": 100, "used": 59, "remaining": 42}),
        json!({"limit": 100, "used": "NaN"}),
        json!({"limit": 100, "used": 59, "resetTime": "invalid"}),
    ] {
        for used in [0.0, 42.0] {
            let snapshot = fetch_fixture(json!({
                "usages": {"limit_5h": {"used_ratio": used,
                    "reset_time": "2026-10-03T16:43:44Z"}},
                "limits": [{"window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"},
                    "detail": detail}]
            }))
            .await
            .unwrap();
            assert_windows(&snapshot, &[("5h", used, 100.0 - used)]);
        }
    }
}

#[tokio::test]
async fn unknown_legacy_periods_are_not_inferred_from_names_or_monthly_durations() {
    let snapshot = fetch_fixture(json!({
        "usages": {
            "limit_5h": {"used_ratio": 0, "reset_time": "2026-10-03T16:43:44Z"},
            "limit_month_total": {"used_ratio": 0, "reset_time": "2026-10-03T16:43:44Z"},
            "limit_month_code": {"used_ratio": 0, "reset_time": "2026-10-03T16:43:44Z"}
        },
        "usage": {"limit": 100, "used": 88, "resetTime": "2026-10-03T16:43:44Z"},
        "limits": [
            {"name": "5h", "window": {"duration": 300, "timeUnit": "TIME_UNIT_UNKNOWN"},
                "detail": {"limit": 100, "used": 59, "resetTime": "2026-10-03T16:43:44Z"}},
            {"name": "month", "window": {"duration": 30, "timeUnit": "TIME_UNIT_DAY"},
                "detail": {"limit": 100, "used": 75, "resetTime": "2026-10-03T16:43:44Z"}},
            {"name": "code month", "window": {"duration": 31, "timeUnit": "TIME_UNIT_DAY"},
                "detail": {"limit": 100, "used": 90, "resetTime": "2026-10-03T16:43:44Z"}}
        ]
    }))
    .await
    .unwrap();
    assert_windows(
        &snapshot,
        &[
            ("5h", 0.0, 100.0),
            ("month", 0.0, 100.0),
            ("code month", 0.0, 100.0),
        ],
    );
}

#[tokio::test]
async fn ambiguous_legacy_usage_reset_does_not_correct_weekly_zero() {
    let snapshot = fetch_fixture(json!({
        "usages": {
            "limit_5h": {"used_ratio": 0, "reset_time": "2026-10-03T16:43:44Z"},
            "limit_7d": {"used_ratio": 0, "reset_time": "2026-10-03T16:43:44Z"}
        },
        "usage": {"limit": 100, "used": 88, "resetTime": "2026-10-03T16:43:44.982282Z"}
    }))
    .await
    .unwrap();
    assert_windows(&snapshot, &[("5h", 0.0, 100.0), ("wk", 0.0, 100.0)]);
}

#[tokio::test]
async fn legacy_monthly_duration_cannot_replace_an_invalid_named_monthly_ratio() {
    let result = fetch_fixture(json!({
        "usages": {"limit_month_total": {"reset_time": "2026-10-03T16:43:44Z"}},
        "limits": [{
            "window": {"duration": 30, "timeUnit": "TIME_UNIT_DAY"},
            "detail": {"limit": 100, "used": 75, "resetTime": "2026-10-03T16:43:44Z"}
        }]
    }))
    .await;
    assert!(matches!(result, Err(QuotaError::Protocol(_))), "{result:?}");
}

#[tokio::test]
async fn conflicting_legacy_candidates_are_rejected_regardless_of_order() {
    let reset = "2026-10-03T16:43:44Z";
    for (first_used, second_used, second_reset) in [
        (59, 60, Some(reset)),
        (59, 59, Some("2026-10-03T16:43:45Z")),
        (59, 59, None),
        // These both clamp to 100%, but must still be recognized as conflicting.
        (120, 130, Some(reset)),
    ] {
        let limits = vec![
            json!({"window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"},
                "detail": {"limit": 100, "used": first_used, "resetTime": reset}}),
            json!({"window": {"duration": 5, "timeUnit": "TIME_UNIT_HOUR"},
                "detail": {"limit": 100, "used": second_used, "resetTime": second_reset}}),
        ];
        for limits in [limits.clone(), limits.into_iter().rev().collect()] {
            for usages in [
                json!(null),
                json!({"limit_5h": {"used_ratio": 0, "reset_time": reset}}),
                json!({"limit_5h": {}}),
            ] {
                let body = json!({"usages": usages, "limits": limits});
                let result = fetch_fixture(body.clone()).await;
                assert!(
                    matches!(
                        result,
                        Err(QuotaError::Protocol("conflicting Kimi quota windows"))
                    ),
                    "{body}: {result:?}"
                );
            }
        }
    }
}

#[tokio::test]
async fn conflicting_weekly_limit_and_top_level_usage_are_rejected() {
    let reset = "2026-10-06T13:43:44Z";
    for usages in [
        json!(null),
        json!({"limit_7d": {"used_ratio": 0, "reset_time": reset}}),
        json!({"limit_7d": {"reset_time": reset}}),
    ] {
        let result = fetch_fixture(json!({
            "usages": usages,
            "limits": [{
                "window": {"duration": 7, "timeUnit": "TIME_UNIT_DAY"},
                "detail": {"limit": 100, "used": 59, "resetTime": reset}
            }],
            "usage": {"limit": "100", "used": "88", "remaining": "12",
                "resetTime": "2026-10-06T13:43:44.982282Z"}
        }))
        .await;
        assert!(
            matches!(
                result,
                Err(QuotaError::Protocol("conflicting Kimi quota windows"))
            ),
            "{usages}: {result:?}"
        );
    }
}

#[tokio::test]
async fn nonzero_named_usage_wins_over_conflicting_legacy_candidates() {
    let reset = "2026-10-03T16:43:44Z";
    let limits = vec![
        json!({"window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"},
            "detail": {"limit": 100, "used": 59, "resetTime": reset}}),
        json!({"window": {"duration": 5, "timeUnit": "TIME_UNIT_HOUR"},
            "detail": {"limit": 100, "used": 60, "resetTime": reset}}),
    ];
    for limits in [limits.clone(), limits.into_iter().rev().collect()] {
        let snapshot = fetch_fixture(json!({
            "usages": {"limit_5h": {"used_ratio": 0.2, "reset_time": reset}},
            "limits": limits
        }))
        .await
        .unwrap();
        assert_windows(&snapshot, &[("5h", 0.2, 99.8)]);
        assert_eq!(snapshot.windows[0].resets_at, Some(reset.parse().unwrap()));
    }
}

#[tokio::test]
async fn equivalent_legacy_candidates_are_deduplicated_using_utc_seconds() {
    let snapshot = fetch_fixture(json!({
        "usages": {"limit_5h": {"used_ratio": 0, "reset_time": "2026-10-03T16:43:44Z"}},
        "limits": [
            {"window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"},
                "detail": {"limit": 100, "used": 59, "resetTime": "2026-10-03T16:43:44.982282Z"}},
            {"window": {"duration": 5, "timeUnit": "TIME_UNIT_HOUR"},
                "detail": {"limit": 200, "used": 118, "resetTime": "2026-10-04T01:43:44+09:00"}}
        ]
    }))
    .await
    .unwrap();
    assert_windows(&snapshot, &[("5h", 59.0, 41.0)]);
}

#[tokio::test]
async fn legacy_plan_parses_string_counts_and_remaining_with_nanosecond_reset() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "usage": {"limit":"200", "remaining":"150", "resetTime":"2026-10-01T12:00:00.123456789Z"},
        "limits": [{"window":{"duration":300,"timeUnit":"TIME_UNIT_MINUTE"}, "detail":{"limit":"100", "used":"90"}}]
    }))).mount(&server).await;
    let client = KimiQuotaClient::new(&server.uri(), Duration::from_secs(2)).unwrap();
    let snapshot = client
        .fetch_quota(&ProviderAuth::new("fixture-key"))
        .await
        .unwrap();
    assert_eq!(snapshot.windows[0].label, "5h");
    assert_eq!(snapshot.windows[0].remaining_percent, 10.0);
    assert_eq!(snapshot.windows[1].label, "wk");
    assert_eq!(snapshot.windows[1].remaining_percent, 75.0);
    assert!(snapshot.windows[1].resets_at.is_some());
}

#[tokio::test]
async fn malformed_or_absent_usage_never_fabricates_full_remaining_quota() {
    for body in [
        json!({}),
        json!({"usages":{"limit_5h":{"used_ratio":"NaN"}}}),
        json!({"usage":{"limit":"0","used":"0"}}),
        json!({"usage":{"limit":"100"}}),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let client = KimiQuotaClient::new(&server.uri(), Duration::from_secs(2)).unwrap();
        assert!(matches!(
            client.fetch_quota(&ProviderAuth::new("fixture-key")).await,
            Err(QuotaError::Protocol(_))
        ));
    }
}

#[tokio::test]
async fn authentication_failures_and_redirects_are_sanitized_and_not_followed() {
    let destination = MockServer::start().await;
    for status in [401, 403, 302, 500] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(status)
                    .insert_header("Location", destination.uri())
                    .set_body_string("fixture-key secret echoed by upstream"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = KimiQuotaClient::new(&server.uri(), Duration::from_secs(2)).unwrap();
        let error = client
            .fetch_quota(&ProviderAuth::new("fixture-key"))
            .await
            .unwrap_err();
        assert!(!error.to_string().contains("fixture-key"));
        assert!(if status == 401 || status == 403 {
            matches!(error, QuotaError::ReauthenticationRequired)
        } else {
            matches!(error, QuotaError::HttpStatus(code) if code == status)
        });
    }
    assert!(destination.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn missing_credentials_never_send_a_request() {
    let server = MockServer::start().await;
    let client = KimiQuotaClient::new(&server.uri(), Duration::from_secs(2)).unwrap();
    assert!(matches!(
        client.fetch_quota(&ProviderAuth::new(" ")).await,
        Err(QuotaError::Credentials)
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
}
