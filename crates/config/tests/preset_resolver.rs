//! プリセットストアと prompt_sources の解決に関する統合テスト。

use std::path::PathBuf;

use config::presets::PresetStore;
use config::{Config, ConfigError, resolve_prompt_sources};

fn presets_dir(tmp: &tempfile::TempDir) -> PathBuf {
    tmp.path().join("presets")
}

fn write_override(tmp: &tempfile::TempDir, name: &str, content: &[u8]) -> PathBuf {
    let path = presets_dir(tmp).join(format!("{name}.md"));
    std::fs::create_dir_all(path.parent().expect("親ディレクトリを持つパス"))
        .expect("プリセットディレクトリを作成できる");
    std::fs::write(&path, content).expect("オーバーライドファイルを書き込める");
    path
}

fn config_referencing(preset: &str) -> Config {
    let doc = format!("[agents.worker]\npreset = \"{preset}\"\n");
    toml::from_str(&doc).expect("参照する agents 設定をパースできる")
}

// Given: 同梱プリセット名 / When: ユーザー上書きなしで解決する
// Then: 同梱本文が返り、上書き無しのディレクトリ指定でも同じ結果になる
#[test]
fn bundled_preset_resolves_when_no_override() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");

    let resolved = PresetStore::resolve("role-worker", None).expect("同梱プリセットを解決できる");
    let fallback = PresetStore::resolve("role-worker", Some(&presets_dir(&tmp)))
        .expect("上書きが無い場合は同梱プリセットへフォールバックする");

    assert!(!resolved.is_empty());
    assert_eq!(resolved, fallback);
}

// Given: 同梱プリセットと同名のユーザー上書きファイル / When: 解決する
// Then: 上書き本文が同梱本文に優先する
#[test]
fn user_override_wins_over_bundled() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    write_override(&tmp, "role-worker", b"USER-OVERRIDE-BODY\n");

    let resolved =
        PresetStore::resolve("role-worker", Some(&presets_dir(&tmp))).expect("上書きを解決できる");

    assert_eq!(resolved, "USER-OVERRIDE-BODY\n");
}

// Given: ユーザー上書きファイル / When: 解決する
// Then: 解決は上書きファイルを一切書き換えない (バイト等価)
#[test]
fn bundled_update_never_mutates_user_override_file() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let path = write_override(&tmp, "role-worker", b"user override line 1\nline 2\n");
    let before = std::fs::read(&path).expect("上書きファイルを読める");

    let resolved =
        PresetStore::resolve("role-worker", Some(&presets_dir(&tmp))).expect("上書きを解決できる");

    let after = std::fs::read(&path).expect("上書きファイルを読める");
    assert_eq!(resolved, "user override line 1\nline 2\n");
    assert_eq!(before, after, "解決はユーザーファイルを変更しない");
}

// Given: パス区切りを含むプリセット名 / When: 解決する
// Then: PresetNameInvalid で拒否される
#[test]
fn preset_name_with_path_separator_is_rejected() {
    let result = PresetStore::resolve("role-worker/../../etc", None);

    match &result {
        Err(ConfigError::PresetNameInvalid { name }) => {
            assert_eq!(name, "role-worker/../../etc");
        }
        other => panic!("PresetNameInvalid を期待した: {other:?}"),
    }
}

// Given: 親ディレクトリ参照を表すプリセット名 / When: 解決する
// Then: PresetNameInvalid で拒否される
#[test]
fn preset_name_with_dotdot_is_rejected() {
    let result = PresetStore::resolve("..", None);

    assert!(
        matches!(result, Err(ConfigError::PresetNameInvalid { .. })),
        "PresetNameInvalid を期待した: {result:?}"
    );
}

// Given: 64KiB 超の上書きファイル (本文にセンチネル文字列) / When: 解決する
// Then: PresetTooLarge で拒否され、エラーに本文が漏れない
#[test]
fn oversized_preset_is_rejected_without_content_in_error() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let sentinel = "SENTINEL-PRESET-BODY-7c3f9a";
    let mut body = String::from(sentinel);
    body.push_str(&"x".repeat(70 * 1024));
    write_override(&tmp, "role-worker", body.as_bytes());

    let error = PresetStore::resolve("role-worker", Some(&presets_dir(&tmp)))
        .expect_err("サイズ超過のプリセットは拒否される");

    match &error {
        ConfigError::PresetTooLarge { size, .. } => assert!(*size > 64 * 1024),
        other => panic!("PresetTooLarge を期待した: {other:?}"),
    }
    let message = error.to_string();
    assert!(
        !message.contains(sentinel),
        "エラーに本文が漏れた: {message}"
    );
}

// Given: UTF-8 として不正なバイト列の上書きファイル / When: 解決する
// Then: PresetNotUtf8 で拒否される
#[test]
fn non_utf8_preset_is_rejected() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    write_override(&tmp, "role-worker", b"\xff\xfe\x00invalid");

    let result = PresetStore::resolve("role-worker", Some(&presets_dir(&tmp)));

    assert!(
        matches!(result, Err(ConfigError::PresetNotUtf8 { .. })),
        "PresetNotUtf8 を期待した: {result:?}"
    );
}

// Given: 同梱にもユーザーにも存在しないプリセット名 / When: 解決する
// Then: PresetNotFound の型付きエラーになる
#[test]
fn missing_preset_is_typed_not_found_error() {
    let result = PresetStore::resolve("no-such-preset", None);

    match &result {
        Err(ConfigError::PresetNotFound { name }) => assert_eq!(name, "no-such-preset"),
        other => panic!("PresetNotFound を期待した: {other:?}"),
    }
}

// Given: 存在しないプリセットを参照する agents 設定 / When: resolve_prompt_sources を呼ぶ
// Then: 型付きエラーで失敗し、部分的な結果は返らない
#[test]
fn prompt_sources_resolve_fail_closed_on_missing_preset() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let config = config_referencing("missing-appendix");

    let result = resolve_prompt_sources(&config, Some(tmp.path()));

    match &result {
        Err(ConfigError::PresetNotFound { name }) => assert_eq!(name, "missing-appendix"),
        other => panic!("PresetNotFound を期待した: {other:?}"),
    }
}

// Given: サイズ超過の上書きファイルを参照する agents 設定 / When: resolve_prompt_sources を呼ぶ
// Then: エラーにオーバーライド本文が漏れない
#[test]
fn prompt_sources_error_never_leaks_override_body() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let sentinel = "SENTINEL-OVERRIDE-BODY-1d4e5f";
    let mut body = String::from(sentinel);
    body.push_str(&"x".repeat(70 * 1024));
    write_override(&tmp, "worker-appendix", body.as_bytes());
    let config = config_referencing("worker-appendix");

    let error = resolve_prompt_sources(&config, Some(tmp.path()))
        .expect_err("サイズ超過のプリセットは拒否される");

    let message = error.to_string();
    assert!(
        !message.contains(sentinel),
        "エラーに本文が漏れた: {message}"
    );
}
