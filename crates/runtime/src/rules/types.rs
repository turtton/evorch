//! プロジェクトルール処理で共有する型。

use std::path::PathBuf;

/// プロジェクトファイルを読み込める信頼状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProjectTrust {
    /// プロジェクトルールの読み込みが承認されている。
    Approved,
    /// プロジェクトルールを読み込まない。
    #[default]
    Unapproved,
}

/// ルール注入時の予算設定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RulesSettings {
    /// モデルのコンテキストウィンドウ上限トークン数。
    pub context_window_tokens: u64,
    /// 応答生成用に予約するトークン数。
    pub response_headroom_tokens: u64,
    /// 1 回に注入する最大バイト数。
    pub max_injection_bytes: u64,
}

impl From<&config::RulesConfig> for RulesSettings {
    fn from(value: &config::RulesConfig) -> Self {
        Self {
            context_window_tokens: value.context_window_tokens,
            response_headroom_tokens: value.response_headroom_tokens,
            max_injection_bytes: value.max_injection_bytes,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum RuleKind {
    AgentsMd,
    ScopedRule,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ScopedDirKind {
    Omo,
    Claude,
    Cursor,
    GithubInstructions,
}

impl ScopedDirKind {
    pub(crate) const ALL: [Self; 4] = [
        Self::Omo,
        Self::Claude,
        Self::Cursor,
        Self::GithubInstructions,
    ];

    pub(crate) const fn dir_name(self) -> &'static str {
        match self {
            Self::Omo => ".omo/rules",
            Self::Claude => ".claude/rules",
            Self::Cursor => ".cursor/rules",
            Self::GithubInstructions => ".github/instructions",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct RuleMeta {
    pub(crate) always_apply: bool,
    pub(crate) globs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuleSourceFile {
    pub(crate) canonical_path: PathBuf,
    pub(crate) rel_path: String,
    pub(crate) dir_kind: Option<ScopedDirKind>,
    pub(crate) depth: u32,
    pub(crate) kind: RuleKind,
    pub(crate) scope: RuleScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuleScope {
    Project,
    User,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedRule {
    pub(crate) source: RuleSourceFile,
    pub(crate) meta: RuleMeta,
    pub(crate) body: String,
}

/// プロジェクトルールの読み込み・解析エラー。
#[derive(Debug, thiserror::Error)]
pub enum RulesError {
    /// ファイルシステム操作に失敗した。
    #[error("rules io error: {path}", path = path.display())]
    Io {
        /// 失敗したパス。
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// frontmatter が不正だった。
    #[error("invalid rules frontmatter: {path}", path = path.display())]
    InvalidFrontmatter { path: PathBuf },
    /// glob パターンが不正だった。
    #[error("invalid rules glob: {path}", path = path.display())]
    InvalidGlob { path: PathBuf },
    /// 対象パスがプロジェクトルート外へ解決された。
    #[error("rules target escaped root: {path}", path = path.display())]
    EscapedRoot { path: PathBuf },
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: config の rules 設定 / When: runtime 設定へ変換 / Then: 全フィールドが保存される
    #[test]
    fn settings_preserve_config_values() {
        let config = config::RulesConfig {
            context_window_tokens: 10,
            response_headroom_tokens: 20,
            max_injection_bytes: 30,
        };

        let settings = RulesSettings::from(&config);

        assert_eq!(settings.context_window_tokens, 10);
        assert_eq!(settings.response_headroom_tokens, 20);
        assert_eq!(settings.max_injection_bytes, 30);
    }

    // Given: 既定の信頼状態 / When: default を生成 / Then: 未承認になる
    #[test]
    fn trust_defaults_to_unapproved() {
        assert_eq!(ProjectTrust::default(), ProjectTrust::Unapproved);
    }
}
