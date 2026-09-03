//! otel 写像層の golden テスト。
//!
//! fixture 更新手順:
//! 1. 新しい写像ルールを semconv v1.37.0 (`event_bus::SEMCONV_PIN`) に照らして
//!    確定させる。
//! 2. `tests/otel_golden/*.json` の `event` を手で書き、`measurements` も
//!    期待値として手で書く (実装出力から生成しない: 循環検査になるため)。
//!    `measurements` の属性順序は mapping 表の順序契約 (semconv キーの後に
//!    evorch 拡張キー) に従う。
//! 3. `cargo test -p event-bus --test otel_golden` が通ることを確認する。
//!    不合が出た場合は fixture と実装のどちらが誤りかを判定し、片方だけ直す。
//! 4. fixture は 1 ファイル 1 event・`{"event": {...}, "measurements": [...]}`
//!    形式。値比較は `serde_json::Value` の完全一致 (measurements 配列と
//!    attrs 配列の順序も含む) で行う。

use event_bus::{Event, map_event, validate_metric_attributes};

fn golden_cases() -> Vec<(String, serde_json::Value)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/otel_golden");
    let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .expect("golden fixture directory exists")
        .map(|entry| entry.expect("fixture entry readable").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let name = path
                .file_name()
                .expect("fixture file name")
                .to_string_lossy()
                .into_owned();
            let content = std::fs::read_to_string(&path).expect("fixture readable");
            let value: serde_json::Value =
                serde_json::from_str(&content).expect("valid fixture JSON");
            (name, value)
        })
        .collect()
}

// Given: tests/otel_golden/*.json の全 fixture。
// When: event を serde で復元し map_event で写像する。
// Then: serde_json::Value として fixture の measurements と完全一致する
//       (配列順序を含む)。
#[test]
fn golden_fixtures_match_the_mapping_contract() {
    for (name, fixture) in golden_cases() {
        let event: Event = serde_json::from_value(fixture["event"].clone())
            .unwrap_or_else(|error| panic!("{name}: event deserialization failed: {error}"));
        let actual = serde_json::to_value(map_event(&event)).expect("measurements serialize");
        assert_eq!(actual, fixture["measurements"], "golden mismatch: {name}");
    }
}

// Given: 全 fixture の event。
// When: map_event の全 measurement を validate_metric_attributes に通す。
// Then: いずれも検査を通過する。
#[test]
fn golden_measurements_pass_attribute_validation() {
    for (name, fixture) in golden_cases() {
        let event: Event = serde_json::from_value(fixture["event"].clone())
            .unwrap_or_else(|error| panic!("{name}: event deserialization failed: {error}"));
        for measurement in map_event(&event) {
            assert_eq!(
                validate_metric_attributes(&measurement),
                Ok(()),
                "{name}: {measurement:?}"
            );
        }
    }
}
