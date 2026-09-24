//! レイヤード構成の読み込みオプションと、各層のマージ処理。
//!
//! レイヤーの優先順位 (低い順):
//! 組み込み既定値 < ユーザ層 < プロジェクト層 < 環境変数層 < CLI 上書き。

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::env;
use crate::error::ConfigError;
use crate::merge::deep_merge;
use crate::migrate;
use crate::types::Config;

/// ユーザ層のメイン設定ファイル名。
const USER_MAIN_FILE: &str = "config.toml";

/// プロジェクト層のメイン設定ファイル名。
const PROJECT_MAIN_FILE: &str = "evorch.toml";

/// ドロップイン設定ディレクトリ名。
const DROPIN_DIR: &str = "config.d";

/// [`Config::load`] の読み込みオプション。
#[derive(Debug, Clone)]
pub struct LoadOptions {
    /// プロジェクト層の基準ディレクトリ。
    ///
    /// `Some` の場合 `<dir>/evorch.toml` と `<dir>/config.d/*.toml` を読み込む。
    /// `None` の場合はプロジェクト層をスキップする。
    pub project_dir: Option<PathBuf>,

    /// ユーザ層の設定ディレクトリ (`<dir>/config.toml` と `<dir>/config.d/*.toml`)。
    ///
    /// `None` の場合は実際のユーザ設定ディレクトリ
    /// (`$XDG_CONFIG_HOME/evorch` または `~/.config/evorch`) を遅延解決する。
    /// テストから実際のホームを isolation するための上書きポイント。
    pub user_config_dir: Option<PathBuf>,

    /// CLI 上書きレイヤー (最も優先度が高い)。
    ///
    /// マージ済み設定値へ最後に深マージされる。
    pub cli_overrides: Option<toml::Value>,

    /// パスが完全一致する設定ファイルの内容を、ディスクの代わりに使用する。
    ///
    /// 保存前の実効設定シミュレーション用。値は現行バージョンの形式で渡すこと。
    /// 上書き値にはマイグレーションを適用しない。メインファイルは未作成でも使用できる。
    pub file_overrides: BTreeMap<PathBuf, toml::Value>,

    /// 環境変数レイヤーを有効にするか。
    pub read_env: bool,

    /// 注入可能な環境変数ソース。
    ///
    /// `read_env == true` かつ `Some` の場合は実際の環境変数の代わりにこのマップを
    /// 使う (テスト用)。`read_env == true` かつ `None` の場合は [`std::env::vars`]
    /// を使う。`read_env == false` の場合は参照されない。
    pub env: Option<BTreeMap<String, String>>,
}

impl Default for LoadOptions {
    /// 既定値: ディレクトリ・CLI 上書き・環境変数ソースなし、環境変数レイヤー有効。
    fn default() -> Self {
        Self {
            project_dir: None,
            user_config_dir: None,
            cli_overrides: None,
            file_overrides: BTreeMap::new(),
            read_env: true,
            env: None,
        }
    }
}

impl Config {
    /// オプションに従って各レイヤーを読み込み、マージした設定を返す。
    ///
    /// レイヤーの優先順位 (低い順):
    ///
    /// 1. 組み込み既定値 ([`Config::default`] を TOML 経由で [`toml::Value`] にした
    ///    もの。マイグレーションは通さない — 既に現行バージョンのため)。
    /// 2. ユーザ層 (`config.toml` → `config.d/*.toml` 辞書順)。
    /// 3. プロジェクト層 (`evorch.toml` → `config.d/*.toml` 辞書順)。
    /// 4. 環境変数層 (`EVORCH_` プレフィックス、[`Option::env`] が `Some` なら
    ///    注入ソースを優先)。
    /// 5. CLI 上書き。
    ///
    /// ディスクから読む各ファイルは TOML パース後、マージ前に [`crate::migrate::run`]
    /// をファイル単位で通す。`file_overrides` の値は現行形式としてそのまま使用する。
    /// 上書きのない、存在しないファイル・ディレクトリは黙ってスキップする。
    ///
    /// # Errors
    ///
    /// - 組み込み既定値の直列化・再パースに失敗した場合
    ///   ([`ConfigError::Migration`]。型定義が TOML 往復可能である限り発生しない)。
    /// - ファイル読み取りに失敗した場合 (`NotFound` 以外の I/O エラー、
    ///   [`ConfigError::Io`])。
    /// - TOML パースに失敗した場合、またはドロップインのルートがテーブルでない
    ///   場合 ([`ConfigError::Parse`])。
    /// - ファイルの `version` が現行より大きい、または整数として読み取れない
    ///   場合 ([`ConfigError::UnsupportedVersion`] / [`ConfigError::Migration`])。
    /// - 環境変数のパスが衝突する場合 ([`ConfigError::InvalidEnvValue`])。
    /// - マージ済み設定に平文 credential フィールドがある場合
    ///   ([`ConfigError::InvalidField`])。未知フィールドは警告して無視する。
    /// - マージ済みの値を [`Config`] にデシリアライズできない場合 (該当する
    ///   エラーバリアントが存在しないため、経緯を文字列に載せた
    ///   [`ConfigError::Migration`] として報告する)。
    pub fn load(opts: &LoadOptions) -> Result<Config, ConfigError> {
        Self::load_with_unknown_field_policy(opts, false)
    }

    /// 未知フィールドもエラーにする厳格な読み込み。設定の検証に使う。
    pub fn load_strict(opts: &LoadOptions) -> Result<Config, ConfigError> {
        Self::load_with_unknown_field_policy(opts, true)
    }

    fn load_with_unknown_field_policy(
        opts: &LoadOptions,
        reject_unknown_fields: bool,
    ) -> Result<Config, ConfigError> {
        let mut merged = builtin_layer()?;

        let user_dir = opts.user_config_dir.clone().or_else(user_config_dir);
        if let Some(dir) = user_dir {
            merge_dir_layer(&mut merged, &dir, USER_MAIN_FILE, &opts.file_overrides)?;
        }

        if let Some(dir) = &opts.project_dir {
            merge_dir_layer(&mut merged, dir, PROJECT_MAIN_FILE, &opts.file_overrides)?;
        }

        if opts.read_env {
            let vars: BTreeMap<String, String> = match &opts.env {
                Some(vars) => vars.clone(),
                None => std::env::vars().collect(),
            };
            merged = deep_merge(merged, env::build_layer(&vars)?);
        }

        if let Some(overrides) = &opts.cli_overrides {
            merged = deep_merge(merged, overrides.clone());
        }

        if !reject_unknown_fields {
            for path in crate::strict::remove_unknown_fields(&mut merged)? {
                tracing::warn!(field = %path, "ignoring unknown config field");
            }
        }
        crate::strict::validate_strict(&merged)?;
        merged.try_into().map_err(|err| {
            ConfigError::Migration(format!("failed to deserialize merged config: {err}"))
        })
    }
}

/// 組み込み既定値レイヤー (最も優先度が低い) を構築する。
///
/// [`Config::default`] を TOML 文字列へ直列化してから [`toml::Value`] に戻すことで、
/// ファイル読み込みと同じ表現に揃える。マイグレーションは通さない (既に現行
/// バージョンのため)。
fn builtin_layer() -> Result<toml::Value, ConfigError> {
    let serialized = toml::to_string(&Config::default()).map_err(|err| {
        ConfigError::Migration(format!("failed to serialize builtin config: {err}"))
    })?;
    toml::from_str(serialized.as_str())
        .map_err(|err| ConfigError::Migration(format!("failed to parse builtin config: {err}")))
}

/// 環境変数から既定のユーザ設定ディレクトリを解決する。
///
/// `$XDG_CONFIG_HOME` (空でない) があれば `<それ>/evorch`、なければ
/// `$HOME/.config/evorch`。どちらも解決できない場合は `None`。
/// この関数はユーザ層の読み込みを試みる時点でのみ呼ばれる (遅延解決)。
pub fn user_config_dir() -> Option<PathBuf> {
    user_config_dir_from(
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

pub(crate) fn user_config_dir_from(xdg: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    let xdg = xdg.filter(|dir| !dir.is_empty());
    if let Some(xdg) = xdg {
        return Some(PathBuf::from(xdg).join("evorch"));
    }
    let home = home.filter(|dir| !dir.is_empty())?;
    Some(PathBuf::from(home).join(".config").join("evorch"))
}

/// 1 レイヤー分のディレクトリ (メインファイル + ドロップイン) を反映する。
///
/// メインファイルを先に、`config.d/*.toml` を辞書順に (後勝ちで) 深マージする。
fn merge_dir_layer(
    merged: &mut toml::Value,
    dir: &Path,
    main_file: &str,
    file_overrides: &BTreeMap<PathBuf, toml::Value>,
) -> Result<(), ConfigError> {
    let mut paths = vec![dir.join(main_file)];
    paths.extend(collect_dropins(&dir.join(DROPIN_DIR))?);
    for path in paths {
        if let Some(value) = read_file_migrated(&path, file_overrides)? {
            *merged = deep_merge(merged.clone(), value);
        }
    }
    Ok(())
}

/// ドロップインディレクトリから `*.toml` ファイルを辞書順に収集する。
///
/// ディレクトリが存在しない場合は空のベクタを返す。
fn collect_dropins(dir: &Path) -> Result<Vec<PathBuf>, ConfigError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };

    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let is_toml = path.extension().is_some_and(|ext| ext == "toml");
        if is_toml && path.is_file() {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

/// 1 つの設定ファイルを読み、TOML パースとマイグレーションまで行う。
///
/// 上書き値がある場合は I/O とマイグレーションを省略する。
/// 上書きもファイルも存在しない場合は `Ok(None)`。ディスク上のルートがテーブルでない
/// ドキュメントも [`ConfigError::Parse`] として扱う。
fn read_file_migrated(
    path: &Path,
    file_overrides: &BTreeMap<PathBuf, toml::Value>,
) -> Result<Option<toml::Value>, ConfigError> {
    if let Some(value) = file_overrides.get(path) {
        return Ok(Some(value.clone()));
    }
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };

    let table: toml::value::Table =
        toml::from_str(content.as_str()).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
    migrate::run(toml::Value::Table(table)).map(Some)
}

#[cfg(test)]
mod tests {
    use super::{read_file_migrated, user_config_dir_from};

    #[test]
    fn file_override_skips_migration_even_for_missing_file() {
        // Given: a current-format override omits the optional version field.
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("evorch.toml");
        let candidate: toml::Value =
            toml::from_str("[metrics]\nenabled = false\n").expect("current-format candidate");
        let overrides = [(path.clone(), candidate.clone())].into();
        // When: resolving that exact path.
        let loaded = read_file_migrated(&path, &overrides).expect("override");
        // Then: unlike migration, the override path does not insert a version field.
        assert_eq!(loaded, Some(candidate));
        assert!(!path.exists());
    }

    #[test]
    fn file_override_requires_matching_path() {
        // Given: an override targets a different directory's identically named file.
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("evorch.toml");
        let other = temp.path().join("other/evorch.toml");
        let candidate = toml::from_str("[metrics]\nenabled = false\n").expect("candidate");
        let overrides = [(other, candidate)].into();
        // When: resolving a missing file not in the map.
        let loaded = read_file_migrated(&path, &overrides).expect("missing file");
        // Then: a matching filename alone cannot replace a different path.
        assert_eq!(loaded, None);
    }

    #[test]
    fn user_config_dir_from_prefers_non_empty_xdg_config_home() {
        // Given: XDG_CONFIG_HOME is set to a non-empty path
        // When: the user config directory is resolved
        let result = user_config_dir_from(Some("/x"), Some("/h"));

        // Then: the XDG path is used as the base directory
        assert_eq!(result, Some(std::path::PathBuf::from("/x/evorch")));
    }

    #[test]
    fn user_config_dir_from_falls_back_for_empty_xdg_config_home() {
        // Given: XDG_CONFIG_HOME is empty and HOME is set
        // When: the user config directory is resolved
        let result = user_config_dir_from(Some(""), Some("/h"));

        // Then: the HOME path is used as the base directory
        assert_eq!(result, Some(std::path::PathBuf::from("/h/.config/evorch")));
    }

    #[test]
    fn user_config_dir_from_uses_home_when_xdg_config_home_is_unset() {
        // Given: XDG_CONFIG_HOME is unset and HOME is set
        // When: the user config directory is resolved
        let result = user_config_dir_from(None, Some("/h"));

        // Then: the HOME path is used as the base directory
        assert_eq!(result, Some(std::path::PathBuf::from("/h/.config/evorch")));
    }

    #[test]
    fn user_config_dir_from_returns_none_without_config_base_directories() {
        // Given: both XDG_CONFIG_HOME and HOME are unset or empty
        // When: the user config directory is resolved
        let result = user_config_dir_from(Some(""), None);

        // Then: no user config directory can be resolved
        assert_eq!(result, None);
    }
}
