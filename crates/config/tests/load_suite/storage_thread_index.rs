//! `[storage.thread_index]`（ADR 0041 Phase B、決定 B-2）の読み込み・strict 検証・既定値。
use std::path::Path;

use config::{
    Config, DEFAULT_THREAD_INDEX_MAX_BYTES, LoadOptions, MAX_THREAD_INDEX_MAX_BYTES,
    ThreadIndexConfig,
};

fn put(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn user_options(root: &Path, text: Option<&str>) -> LoadOptions {
    if let Some(text) = text {
        put(&root.join("user/config.toml"), text);
    }
    LoadOptions {
        user_config_dir: Some(root.join("user")),
        read_env: false,
        ..Default::default()
    }
}

#[test]
fn missing_section_loads_enabled_index_with_512_mib_budget() {
    // Given: storage セクションのない user 設定
    let root = tempfile::tempdir().unwrap();
    let opts = user_options(root.path(), Some("[metrics]\nenabled = true\n"));

    // When / Then: どちらの読み込みでも既定値（有効・512 MiB）になる
    for load in [Config::load, Config::load_strict] {
        let thread_index = load(&opts).unwrap().storage.thread_index;
        assert!(thread_index.enabled);
        assert_eq!(thread_index.max_bytes, 512 * 1024 * 1024);
        assert_eq!(thread_index.max_bytes, DEFAULT_THREAD_INDEX_MAX_BYTES);
    }
}

#[test]
fn user_values_and_env_override_are_applied() {
    // Given: user 層で無効化・1 GiB、env 層で上限を上書きする設定
    let root = tempfile::tempdir().unwrap();
    put(
        &root.path().join("user/config.toml"),
        "[storage.thread_index]\nenabled = false\nmax_bytes = 1073741824\n",
    );
    let user_only = user_options(root.path(), None);
    let with_env = LoadOptions {
        read_env: true,
        env: Some(
            [(
                "EVORCH_STORAGE__THREAD_INDEX__MAX_BYTES".to_owned(),
                "268435456".to_owned(),
            )]
            .into(),
        ),
        ..user_options(root.path(), None)
    };

    // When / Then: user の値が読まれ、env は後勝ちで上書きする
    for load in [Config::load, Config::load_strict] {
        assert_eq!(
            load(&user_only).unwrap().storage.thread_index,
            ThreadIndexConfig {
                enabled: false,
                max_bytes: 1_073_741_824,
            }
        );
        assert_eq!(
            load(&with_env).unwrap().storage.thread_index,
            ThreadIndexConfig {
                enabled: false,
                max_bytes: 268_435_456,
            }
        );
    }
}

#[test]
fn unknown_keys_are_rejected_by_strict_load_and_ignored_by_tolerant_load() {
    for (text, path) in [
        (
            "[storage.thread_index]\nmax_byte = 1048576\n",
            "storage.thread_index.max_byte",
        ),
        ("[storage]\nfts_ratio = 3\n", "storage.fts_ratio"),
    ] {
        // Given: storage 配下に typo を含む user 設定
        let root = tempfile::tempdir().unwrap();
        let opts = user_options(root.path(), Some(text));

        // When: strict / 通常の読み込みを行う
        let error = Config::load_strict(&opts).unwrap_err().to_string();

        // Then: strict は完全な path で拒否し、通常読み込みは既定値で続行する
        assert!(error.contains(path), "{error}");
        assert_eq!(
            Config::load(&opts).unwrap().storage.thread_index,
            ThreadIndexConfig::default()
        );
    }
}

#[test]
fn out_of_range_or_malformed_budget_is_rejected_by_both_loads() {
    let too_large = MAX_THREAD_INDEX_MAX_BYTES + 1;
    for text in [
        "[storage.thread_index]\nmax_bytes = 0\n".to_owned(),
        "[storage.thread_index]\nmax_bytes = 1048575\n".to_owned(),
        format!("[storage.thread_index]\nmax_bytes = {too_large}\n"),
        "[storage.thread_index]\nenabled = false\nmax_bytes = 0\n".to_owned(),
        "[storage.thread_index]\nmax_bytes = -1\n".to_owned(),
        "[storage.thread_index]\nmax_bytes = \"512MiB\"\n".to_owned(),
        "[storage.thread_index]\nenabled = 1\n".to_owned(),
    ] {
        // Given: 範囲外・負・型違いの値を持つ user 設定
        let root = tempfile::tempdir().unwrap();
        let opts = user_options(root.path(), Some(&text));

        // When / Then: 既定値へ黙って置き換えず、どちらの読み込みも失敗する
        for load in [Config::load, Config::load_strict] {
            assert!(load(&opts).is_err(), "{text:?} は拒否される");
        }
    }
}

#[test]
fn range_error_names_the_full_config_path() {
    // Given: 下限未満の max_bytes
    let root = tempfile::tempdir().unwrap();
    let opts = user_options(
        root.path(),
        Some("[storage.thread_index]\nmax_bytes = 4096\n"),
    );

    // When
    let error = Config::load_strict(&opts).unwrap_err().to_string();

    // Then: 完全な config path と許容範囲を示す
    assert!(error.contains("storage.thread_index.max_bytes"), "{error}");
    assert!(error.contains("1048576..=10737418240"), "{error}");
}

#[test]
fn project_layer_cannot_change_the_storage_budget() {
    // Given: project 層だけが storage を設定している
    let root = tempfile::tempdir().unwrap();
    put(
        &root.path().join("project/.evorch/config.toml"),
        "[storage.thread_index]\nenabled = false\nmax_bytes = 1048576\n",
    );
    let opts = LoadOptions {
        project_dir: Some(root.path().join("project")),
        ..user_options(root.path(), None)
    };

    // When / Then: project 層の storage は無視され既定値になる（DB はユーザー単位で共有）
    assert_eq!(
        Config::load(&opts).unwrap().storage.thread_index,
        ThreadIndexConfig::default()
    );
}
