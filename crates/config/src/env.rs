//! 環境変数から設定オーバーライドレイヤーを構築する。
//!
//! `EVORCH_` プレフィックスを持つ環境変数を入れ子の TOML テーブルへ変換する。
//! v0.1 では `.` や `-` を含むキー (論理モデル名・プロファイル名) は
//! 環境変数経由では表現できない。

use std::collections::BTreeMap;

use crate::error::ConfigError;

/// 環境変数オーバーライドの対象を示すプレフィックス。
const ENV_PREFIX: &str = "EVORCH_";

/// 環境変数のマップから設定レイヤーを構築する。
///
/// 変換規則:
///
/// - `EVORCH_` プレフィックスを持つ変数のみを扱い、プレフィックスを取り除く。
/// - 残りを小文字化し、`__` を入れ子パスの区切りとして扱う。
/// - 値はまず TOML リテラルとしてパースし、失敗した場合は生の文字列として扱う
///   (例: `true` は boolean、`hello` は `"hello"`)。
///
/// # Errors
///
/// - パス区切りによって空のセグメントが生じる場合
///   (例: `EVORCH_A__`)。
/// - 既存のスカラー値を入れ子の途中ノードとして横断しようとする場合
///   (例: `EVORCH_PANEL=x` と `EVORCH_PANEL__WIDTH=1` の併用)。エラーには
///   処理中の変数名と値を報告する。
pub(crate) fn build_layer(vars: &BTreeMap<String, String>) -> Result<toml::Value, ConfigError> {
    let mut root = toml::value::Table::new();
    for (key, raw) in vars {
        let Some(rest) = key.strip_prefix(ENV_PREFIX) else {
            continue;
        };
        let segments: Vec<String> = rest.to_lowercase().split("__").map(String::from).collect();
        if segments.iter().any(String::is_empty) {
            return Err(ConfigError::InvalidEnvValue {
                key: key.clone(),
                value: raw.clone(),
            });
        }
        let value = parse_value(raw);
        insert_nested(&mut root, &segments, value, key, raw)?;
    }
    Ok(toml::Value::Table(root))
}

/// 環境変数の値を TOML リテラルとして解釈し、失敗時は生の文字列として扱う。
fn parse_value(raw: &str) -> toml::Value {
    match raw.parse::<toml::Value>() {
        Ok(value) => value,
        Err(_) => toml::Value::String(raw.to_owned()),
    }
}

/// 1 つの変数の値を、パスセグメントに従って入れ木へ挿入する。
///
/// 衝突の扱い:
/// - 途中ノードとして既存のスカラーを横断しようとする場合 → エラー。
/// - リーフ位置に既存のテーブルがある場合 (より長いパスが先行していた等) → エラー。
/// - リーフ位置に既存のスカラーがある場合 (大文字小文字の畳み込み重複等) → 上書き。
fn insert_nested(
    root: &mut toml::value::Table,
    segments: &[String],
    value: toml::Value,
    key: &str,
    raw: &str,
) -> Result<(), ConfigError> {
    let mut cursor = root;
    for segment in &segments[..segments.len() - 1] {
        let entry = cursor
            .entry(segment.clone())
            .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));
        match entry {
            toml::Value::Table(inner) => cursor = inner,
            _ => {
                return Err(ConfigError::InvalidEnvValue {
                    key: key.to_string(),
                    value: raw.to_string(),
                });
            }
        }
    }

    let leaf = segments
        .last()
        .expect("セグメントは空でないため 1 つ以上存在する");
    if cursor.get(leaf.as_str()).is_some_and(toml::Value::is_table) {
        return Err(ConfigError::InvalidEnvValue {
            key: key.to_string(),
            value: raw.to_string(),
        });
    }
    cursor.insert(leaf.clone(), value);
    Ok(())
}
