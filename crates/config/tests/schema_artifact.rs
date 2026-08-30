//! 公開済みの versioned JSON Schema artifact の整合を検証します。
//!
//! `Config` の `CURRENT_VERSION` と artifact ファイル名の v{n} がずれた場合や、
//! checked-in artifact が生成器出力から drift した場合にこのテストが失敗します。

use std::path::PathBuf;

use config::CURRENT_VERSION;

/// docs/config 配下の versioned schema artifact のパスを返す。
fn artifact_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/config")
        .join(format!("evorch-config-v{CURRENT_VERSION}.schema.json"))
}

// Given: CURRENT_VERSION に対応する versioned artifact / When: 存在を確認する
// Then: artifact が checked-in されている
// (version bump 時は artifact の rename と再生成が必要であることをここで検知する)
#[test]
fn versioned_artifact_exists_for_current_version() {
    let path = artifact_path();

    assert!(
        path.is_file(),
        "versioned artifact が存在しない: {} (Config CURRENT_VERSION={CURRENT_VERSION})。\
         version bump 時は新しい v{{n}} 名で artifact を追加する \
         (cargo run -p config --example dump_schema -- <path>)",
        path.display()
    );
}

// Given: checked-in 済みの artifact / When: 生成器の出力と比較する
// Then: byte-identical である (再生成 command 以外の手編集を検知する)
#[test]
fn checked_in_artifact_matches_generated_schema() {
    let path = artifact_path();
    let checked_in = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("artifact を読み取れない: {}: {e}", path.display()));

    let generated = format!("{}\n", config::json_schema());

    assert_eq!(
        checked_in,
        generated,
        "artifact が生成器出力と一致しない (drift)。\
         再生成: cargo run -p config --example dump_schema -- {}",
        path.display()
    );
}

// Given: 生成した JSON Schema / When: draft 2020-12 の meta-schema で検証する
// Then: 有効な JSON Schema として受理される
#[test]
fn generated_schema_is_valid_json_schema() {
    let value: serde_json::Value =
        serde_json::from_str(&config::json_schema()).expect("生成 schema は有効な JSON");

    jsonschema::meta::validate(&value).expect("生成 schema は meta-schema に対して有効");
}

// Given: 生成した JSON Schema / When: 必須セクションの有無を確認する
// Then: version/providers/routing/panel/diagnostics/permissions/metrics を含む
#[test]
fn generated_schema_covers_all_config_sections() {
    let schema = config::json_schema();

    for section in [
        "\"version\"",
        "\"providers\"",
        "\"routing\"",
        "\"panel\"",
        "\"diagnostics\"",
        "\"permissions\"",
        "\"metrics\"",
    ] {
        assert!(
            schema.contains(section),
            "生成 schema に {section} が含まれない"
        );
    }
}
