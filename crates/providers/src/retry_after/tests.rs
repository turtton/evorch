use super::*;
use serde_json::json;

// Given: 整数秒・前後空白・HTTP-date・不正値 / When: 値を解釈 / Then: 秒は Duration、過去の日付は 0、不正値は None
#[test]
fn value_accepts_integer_seconds_and_http_dates() {
    assert_eq!(parse_value("120"), Some(Duration::from_secs(120)));
    assert_eq!(parse_value(" 7 "), Some(Duration::from_secs(7)));
    assert_eq!(
        parse_value("Wed, 21 Oct 2015 07:28:00 GMT"),
        Some(Duration::ZERO)
    );
    for invalid in ["", "soon", "-1", "1.5", "99999999999999999999999"] {
        assert_eq!(parse_value(invalid), None, "{invalid:?}");
    }
}

// Given: 未来の HTTP-date / When: 解釈 / Then: 現在からの残り時間になる
#[test]
fn value_converts_future_http_date_to_remaining_delay() {
    let at = SystemTime::now() + Duration::from_secs(3600);
    let delay = parse_value(&httpdate::fmt_http_date(at)).unwrap();

    assert!(delay > Duration::from_secs(3590) && delay <= Duration::from_secs(3600));
}

// Given: 大文字小文字の異なる headers の retry-after (文字列・数値) と無関係な headers
// When: JSON headers を解釈 / Then: 値があれば Duration、なければ None
#[test]
fn json_headers_find_retry_after_case_insensitively() {
    assert_eq!(
        from_json_headers(&json!({"Retry-After": "3"})),
        Some(Duration::from_secs(3))
    );
    assert_eq!(
        from_json_headers(&json!({"retry-after": 4})),
        Some(Duration::from_secs(4))
    );
    assert_eq!(from_json_headers(&json!({"x-other": "3"})), None);
    assert_eq!(from_json_headers(&json!({"retry-after": true})), None);
    assert_eq!(from_json_headers(&json!("retry-after")), None);
}

// Given: 各単位のレート制限メッセージ / When: メッセージから抽出 / Then: 単位に応じた Duration になる
#[test]
fn message_extracts_delay_in_seconds_milliseconds_and_minutes() {
    for (message, expected) in [
        (
            "Rate limit reached. Please try again in 1.5s.",
            Duration::from_millis(1500),
        ),
        ("Try again in 20ms", Duration::from_millis(20)),
        ("please TRY AGAIN IN 11 seconds", Duration::from_secs(11)),
        ("try again in 2 minutes", Duration::from_secs(120)),
        ("try again in 6m0s", Duration::from_secs(360)),
    ] {
        assert_eq!(from_message(message), Some(expected), "{message}");
    }
    for message in [
        "try again later",
        "try again in soon",
        "try again in 5 fortnights",
        "",
    ] {
        assert_eq!(from_message(message), None, "{message}");
    }
}

// Given: 整数秒・HTTP-date・非数値・欠落の Retry-After ヘッダ / When: ヘッダを解釈
// Then: 秒と HTTP-date は Duration になり、非数値と欠落は None になる
#[test]
fn header_map_accepts_seconds_and_http_dates() {
    let header = |value: &str| {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, value.parse().unwrap());
        headers
    };

    assert_eq!(
        from_header_map(&header("120")),
        Some(Duration::from_secs(120))
    );
    assert_eq!(
        from_header_map(&header("Wed, 21 Oct 2015 07:28:00 GMT")),
        Some(Duration::ZERO)
    );
    assert_eq!(from_header_map(&header("soon")), None);
    assert_eq!(from_header_map(&reqwest::header::HeaderMap::new()), None);
}
