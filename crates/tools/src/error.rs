//! ツール層のエラー型を定義します。

/// ツールの解決・引数検証・実行で発生しうるエラー。
///
/// [`std::error::Error`] は thiserror により自動実装される。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ToolError {
    /// 登録されていないツール名が指定された。
    #[error("未知のツールが指定されました: {name}")]
    UnknownTool {
        /// 指定されたツール名。
        name: String,
    },
    /// ツールの引数がスキーマに適合しない。
    #[error("ツールの引数が不正です: {detail}")]
    InvalidArgs {
        /// 不正の詳細。
        detail: String,
    },
    /// ツールのスキーマ定義自体が不正。
    #[error("ツール {tool_name} のスキーマが不正です: {detail}")]
    InvalidSchema {
        /// スキーマが不正なツールの名前。
        tool_name: String,
        /// 不正の詳細。
        detail: String,
    },
    /// 指定されたパスが存在しない。
    #[error("パスが見つかりません: {path}")]
    PathNotFound {
        /// 存在しないパス。
        path: String,
    },
    /// 指定されたパスがファイルではない（ディレクトリ等）。
    #[error("パスがファイルではありません: {path}")]
    NotAFile {
        /// ファイルではないパス。
        path: String,
    },
    /// edit ツールの置換対象文字列がファイル内に見つからない。
    #[error("編集対象の文字列が見つかりません: {path}")]
    EditTargetNotFound {
        /// 置換対象が見つからなかったファイルのパス。
        path: String,
    },
    /// grep ツールの検索パターンが不正な正規表現。
    #[error("検索パターンが不正です: {detail}")]
    InvalidPattern {
        /// 不正の詳細。
        detail: String,
    },
    /// コマンドの起動に失敗した。
    #[error("コマンドの起動に失敗しました: {command}: {detail}")]
    SpawnFailed {
        /// 起動に失敗したコマンド。
        command: String,
        /// 失敗の詳細。
        detail: String,
    },
    /// コマンドが制限時間内に完了しなかった。
    #[error("コマンドがタイムアウトしました: {timeout_ms}ms")]
    Timeout {
        /// 制限時間（ミリ秒）。
        timeout_ms: u64,
    },
    /// 承認方針または承認応答により実行を拒否された。
    #[error("ツールの実行が拒否されました: {tool_name}: {reason}")]
    ExecutionDenied {
        /// 拒否されたツール名。
        tool_name: String,
        /// 拒否理由。
        reason: String,
    },
    /// コマンドを実行するサンドボックスを準備できない。
    #[error("サンドボックスを利用できません: {detail}")]
    SandboxUnavailable {
        /// 利用できない理由。
        detail: String,
    },
    /// 指定されたパスが Git リポジトリの作業ツリーではない。
    #[error("Git リポジトリではありません: {path}")]
    NotAGitRepository {
        /// Git リポジトリではないパス。
        path: String,
    },
    /// その他の入出力失敗。
    #[error("入出力エラー: {detail}")]
    Io {
        /// 失敗の詳細。
        detail: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: 全バリアントの値 / When: Display / Then: 契約どおりのメッセージになる
    #[test]
    fn error_display_matches_contract() {
        let cases = [
            (
                ToolError::UnknownTool {
                    name: "foo".to_string(),
                },
                "未知のツールが指定されました: foo",
            ),
            (
                ToolError::InvalidArgs {
                    detail: "missing path".to_string(),
                },
                "ツールの引数が不正です: missing path",
            ),
            (
                ToolError::InvalidSchema {
                    tool_name: "read".to_string(),
                    detail: "bad".to_string(),
                },
                "ツール read のスキーマが不正です: bad",
            ),
            (
                ToolError::PathNotFound {
                    path: "/tmp/a".to_string(),
                },
                "パスが見つかりません: /tmp/a",
            ),
            (
                ToolError::NotAFile {
                    path: "/tmp".to_string(),
                },
                "パスがファイルではありません: /tmp",
            ),
            (
                ToolError::EditTargetNotFound {
                    path: "a.txt".to_string(),
                },
                "編集対象の文字列が見つかりません: a.txt",
            ),
            (
                ToolError::InvalidPattern {
                    detail: "((".to_string(),
                },
                "検索パターンが不正です: ((",
            ),
            (
                ToolError::SpawnFailed {
                    command: "git".to_string(),
                    detail: "no such file".to_string(),
                },
                "コマンドの起動に失敗しました: git: no such file",
            ),
            (
                ToolError::Timeout { timeout_ms: 1500 },
                "コマンドがタイムアウトしました: 1500ms",
            ),
            (
                ToolError::ExecutionDenied {
                    tool_name: "shell".to_string(),
                    reason: "方針".to_string(),
                },
                "ツールの実行が拒否されました: shell: 方針",
            ),
            (
                ToolError::SandboxUnavailable {
                    detail: "bwrap なし".to_string(),
                },
                "サンドボックスを利用できません: bwrap なし",
            ),
            (
                ToolError::NotAGitRepository {
                    path: "/tmp/x".to_string(),
                },
                "Git リポジトリではありません: /tmp/x",
            ),
            (
                ToolError::Io {
                    detail: "permission denied".to_string(),
                },
                "入出力エラー: permission denied",
            ),
        ];

        assert_eq!(cases.len(), 13);
        for (error, expected) in cases {
            assert_eq!(error.to_string(), expected);
        }
    }
}
