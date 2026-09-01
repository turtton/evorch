//! ツールの抽象と権限モデルを定義します。

use crate::error::ToolError;
use crate::result::ToolResult;

/// ツールが要求する権限の集合。
///
/// 各フラグはツールがその種類のリソースへアクセスし得ることを示し、実行環境は
/// この宣言に基づいて許可判定を行う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permissions {
    /// ファイルシステムの読み取り。
    pub fs_read: bool,
    /// ファイルシステムの書き込み。
    pub fs_write: bool,
    /// プロセスの起動。
    pub process_spawn: bool,
    /// ネットワークアクセス。
    pub network: bool,
}

impl Permissions {
    /// 読み取り専用の権限（`fs_read` のみ `true`）。
    pub const fn read_only() -> Self {
        Self {
            fs_read: true,
            fs_write: false,
            process_spawn: false,
            network: false,
        }
    }

    /// 読み書きの権限（`fs_read` と `fs_write` が `true`）。
    pub const fn read_write() -> Self {
        Self {
            fs_read: true,
            fs_write: true,
            process_spawn: false,
            network: false,
        }
    }

    /// プロセス起動を含む全権限（ローカルリソースの 3 フラグすべて `true`）。
    pub const fn process() -> Self {
        Self {
            fs_read: true,
            fs_write: true,
            process_spawn: true,
            network: false,
        }
    }

    /// ネットワークアクセスのみの権限（`network` のみ `true`）。
    pub const fn network() -> Self {
        Self {
            fs_read: false,
            fs_write: false,
            process_spawn: false,
            network: true,
        }
    }
}

/// 標準ツールの抽象。
///
/// ツールの実行は必ず ToolExecutor（wave 3 で追加）経由で行うこと。ToolExecutor
/// が引数のスキーマ検証と結果の正規化を担うため、`execute` を直接呼び出した場合の
/// 戻り値は検証前の生の内容になる。
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// ツールの一意な名前。
    fn name(&self) -> &'static str;

    /// 引数の JSON Schema。
    fn schema(&self) -> serde_json::Value;

    /// ツールが要求する権限。
    fn permissions(&self) -> Permissions;

    /// ツールを実行する。
    ///
    /// `args` は [`Tool::schema`] に適合する JSON オブジェクトを想定する。
    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: 3 つのコンストラクタ / When: 権限を生成 / Then: フラグの組が契約どおりで network はすべて false
    #[test]
    fn permissions_const_constructors() {
        assert_eq!(
            Permissions::read_only(),
            Permissions {
                fs_read: true,
                fs_write: false,
                process_spawn: false,
                network: false,
            }
        );
        assert_eq!(
            Permissions::read_write(),
            Permissions {
                fs_read: true,
                fs_write: true,
                process_spawn: false,
                network: false,
            }
        );
        assert_eq!(
            Permissions::process(),
            Permissions {
                fs_read: true,
                fs_write: true,
                process_spawn: true,
                network: false,
            }
        );
    }

    // Given: network コンストラクタ / When: 権限を生成 / Then: network のみ true で他は false
    #[test]
    fn permissions_network_constructor_sets_only_network() {
        assert_eq!(
            Permissions::network(),
            Permissions {
                fs_read: false,
                fs_write: false,
                process_spawn: false,
                network: true,
            }
        );
    }
}
