//! 認証情報を解決する環境変数ソースを提供します。

use std::collections::BTreeMap;

/// 環境変数名から値を取得する抽象です。
pub trait EnvLookup: Send + Sync + std::fmt::Debug {
    /// 指定名の値を返し、未設定なら `None` を返します。
    fn var(&self, name: &str) -> Option<String>;
}

/// 現在のプロセス環境を参照する実装です。
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessEnv;

impl EnvLookup for ProcessEnv {
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

/// 決定的なテストや埋め込み用途向けのマップ実装です。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MapEnv(pub BTreeMap<String, String>);

impl MapEnv {
    /// マップから環境変数ソースを構築します。
    pub const fn new(values: BTreeMap<String, String>) -> Self {
        Self(values)
    }
}

impl<K, V> FromIterator<(K, V)> for MapEnv
where
    K: Into<String>,
    V: Into<String>,
{
    fn from_iter<T: IntoIterator<Item = (K, V)>>(iter: T) -> Self {
        Self(
            iter.into_iter()
                .map(|(key, value)| (key.into(), value.into()))
                .collect(),
        )
    }
}

impl From<BTreeMap<String, String>> for MapEnv {
    fn from(values: BTreeMap<String, String>) -> Self {
        Self::new(values)
    }
}

impl EnvLookup for MapEnv {
    fn var(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }
}
