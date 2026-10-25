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
    #[error("編集対象の文字列が見つかりません: {path}{}", edit_label(.edit))]
    EditTargetNotFound {
        /// 置換対象が見つからなかったファイルのパス。
        path: String,
        /// `edits` 形式で失敗した要素の添字（0 始まり）。単一置換では `None`。
        edit: Option<usize>,
    },
    /// `replace_all` なしの edit で、置換対象文字列がファイル内の複数箇所に一致した。
    #[error(
        "編集対象の文字列が {count} 箇所に一致し一意に定まりません: {path}{} ({} 行目)。前後の行を old_string に含めて一意にするか、すべて置換する場合は replace_all: true を指定してください",
        edit_label(.edit),
        line_list(.lines, .count)
    )]
    EditTargetNotUnique {
        /// 対象のファイルのパス。
        path: String,
        /// `edits` 形式で失敗した要素の添字（0 始まり）。単一置換では `None`。
        edit: Option<usize>,
        /// 一致した箇所の数。
        count: usize,
        /// 先頭から最大 10 箇所の一致の開始行（1 始まり）。
        lines: Vec<usize>,
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
        /// 拒否理由。監査用のため、エージェント向けガイダンスは含めない。
        reason: String,
        /// エージェントへ返すテキストだけに付ける補足。
        guidance: Option<&'static str>,
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

impl ToolError {
    /// Render agent-facing text without changing Display or audit/event reasons.
    pub fn agent_message(&self) -> String {
        match self {
            Self::ExecutionDenied {
                guidance: Some(guidance),
                ..
            } => {
                format!("{self}\n{guidance}")
            }
            _ => self.to_string(),
        }
    }
}

/// `edits` 形式の要素を示す接尾辞。単一置換では空文字列。
fn edit_label(edit: &Option<usize>) -> String {
    edit.map(|index| format!(" (edits[{index}])"))
        .unwrap_or_default()
}

/// 一致行の一覧。省略した一致があれば末尾に `…` を付ける。
fn line_list(lines: &[usize], count: &usize) -> String {
    let mut list = lines
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    if *count > lines.len() {
        list.push_str(", …");
    }
    list
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
                    edit: None,
                },
                "編集対象の文字列が見つかりません: a.txt",
            ),
            (
                ToolError::EditTargetNotFound {
                    path: "a.txt".to_string(),
                    edit: Some(2),
                },
                "編集対象の文字列が見つかりません: a.txt (edits[2])",
            ),
            (
                ToolError::EditTargetNotUnique {
                    path: "a.txt".to_string(),
                    edit: None,
                    count: 2,
                    lines: vec![3, 9],
                },
                "編集対象の文字列が 2 箇所に一致し一意に定まりません: a.txt (3, 9 行目)。前後の行を old_string に含めて一意にするか、すべて置換する場合は replace_all: true を指定してください",
            ),
            (
                ToolError::EditTargetNotUnique {
                    path: "a.txt".to_string(),
                    edit: Some(1),
                    count: 12,
                    lines: vec![1, 1, 2],
                },
                "編集対象の文字列が 12 箇所に一致し一意に定まりません: a.txt (edits[1]) (1, 1, 2, … 行目)。前後の行を old_string に含めて一意にするか、すべて置換する場合は replace_all: true を指定してください",
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
                    guidance: None,
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

        for (error, expected) in cases {
            assert_eq!(error.to_string(), expected);
        }
    }
}
