//! AgentRun の識別子・設定、および表層用の DTO を定義します。

use std::fmt;
use std::path::PathBuf;

use event_bus::AgentRunPhase;
use serde::{Deserialize, Serialize};

/// ランタイム内の AgentRun を一意に識別する newtype。
///
/// [`Display`](std::fmt::Display) はイベントペイロードの `run_id` 文字列と
/// 同一の `run-{n}` 形式を返す。イベントへの載せ替えはこの形式で行う。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct RunId(u64);

impl RunId {
    /// 数値から ID を構築する。
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// 内部の数値表現を返す。
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "run-{}", self.0)
    }
}

/// AgentRun が親 workspace を共有するか、専用の git worktree を使うかを示す。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceMode {
    /// 親 run と同じ workspace を使用する。
    #[default]
    Shared,
    /// run 専用の git worktree を使用する。
    Isolated,
}

/// isolated workspace の変更を統合する方法を示す。
///
/// branch が既定である。patch mode は v0.2 スコープ外の型トークン
/// (packet v02-workspace-isolation) とする。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MergeMode {
    /// 専用 branch を統合する。
    #[default]
    Branch,
}

/// AgentRun の実行設定。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunConfig {
    /// ユーザー入力を待ち受ける対話モードか。既定は `false` (非対話)。
    pub interactive: bool,
    /// run の表示名。`None` の場合はロール名へフォールバックする。
    pub name: Option<String>,
    /// run のタスクカテゴリ。システムプロンプトの category overlay 選択に使う。
    /// `None` の場合は overlay を挿入しない。
    pub category: Option<String>,
    /// 親 workspace を共有するか、専用 git worktree を使用するか。
    pub workspace_mode: WorkspaceMode,
    /// isolated workspace の変更を統合する方法。
    pub merge_mode: MergeMode,
}

/// AgentRun に割り当てられた workspace の検査用 DTO。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceInspection {
    /// 親 workspace を共有するか、専用 worktree を使うか。
    pub mode: WorkspaceMode,
    /// isolated run の merge deliverable branch。cleanup 後も保持される。
    pub branch: Option<String>,
    /// isolated worktree の path。cleanup 成功後は `None`。
    pub worktree_path: Option<PathBuf>,
    /// isolated workspace の変更を統合する方法。
    pub merge_mode: MergeMode,
}

/// AgentRun の要約 (一覧表示用 DTO)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentSummary {
    /// 実行 ID。
    pub run_id: RunId,
    /// 表示名。`RunConfig::name` 未指定時はロール名。
    pub name: String,
    /// ロール名識別子。
    pub role_name: String,
    /// 現在の位相。
    pub phase: AgentRunPhase,
    /// 選択済みモデル識別子。
    pub model: String,
}

/// 単一 AgentRun の詳細検査 (検査用 DTO)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentInspection {
    /// 実行 ID。
    pub run_id: RunId,
    /// ロール名識別子。
    pub role_name: String,
    /// 現在の位相。
    pub phase: AgentRunPhase,
    /// 保持するメッセージ数。
    pub message_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: 数値 7 と 0 の RunId / When: Display / Then: "run-{n}" 形式 (イベント run_id と同一形式)
    #[test]
    fn run_id_displays_as_run_prefixed() {
        assert_eq!(RunId::new(7).to_string(), "run-7");
        assert_eq!(RunId::new(0).to_string(), "run-0");
    }

    // Given: 数値 42 の RunId / When: JSON 化 / Then: 内部数値として serialize される
    #[test]
    fn run_id_serializes_as_number() {
        let json = serde_json::to_value(RunId::new(42)).expect("serialize RunId");

        assert_eq!(json, serde_json::json!(42));
    }

    // Given: 数値 123 の RunId / When: get / Then: 元の数値を返す
    #[test]
    fn run_id_get_returns_inner_value() {
        assert_eq!(RunId::new(123).get(), 123);
    }

    // Given: RunConfig / When: Default / Then: interactive は false (非対話が既定)
    #[test]
    fn run_config_defaults_to_non_interactive() {
        assert!(!RunConfig::default().interactive);
    }

    // Given: RunConfig / When: Default / Then: name は None (表示名未指定が既定)
    #[test]
    fn run_config_default_has_no_name() {
        assert!(RunConfig::default().name.is_none());
    }

    // Given: RunConfig / When: Default / Then: category は None (overlay 選択なし)
    #[test]
    fn run_config_default_has_no_category() {
        assert!(RunConfig::default().category.is_none());
    }

    // Given: RunConfig / When: Default / Then: workspace_mode は Shared (共有 workspace が既定)
    #[test]
    fn workspace_mode_on_config_defaults_to_shared() {
        assert_eq!(RunConfig::default().workspace_mode, WorkspaceMode::Shared);
    }

    // Given: RunConfig / When: Default / Then: merge_mode は Branch (branch merge が既定)
    #[test]
    fn merge_mode_on_config_defaults_to_branch() {
        assert_eq!(RunConfig::default().merge_mode, MergeMode::Branch);
    }

    // Given: Isolated workspace mode / When: JSON 化 / Then: lowercase の "isolated" となる
    #[test]
    fn workspace_mode_serializes_lowercase() {
        let json = serde_json::to_value(WorkspaceMode::Isolated).expect("serialize WorkspaceMode");

        assert_eq!(json, serde_json::json!("isolated"));
    }

    // Given: "shared" と "isolated" / When: WorkspaceMode として JSON 復元 / Then: 対応する既知 variant となる
    #[test]
    fn workspace_mode_deserializes_known_values() {
        assert_eq!(
            serde_json::from_value::<WorkspaceMode>(serde_json::json!("shared"))
                .expect("deserialize shared WorkspaceMode"),
            WorkspaceMode::Shared
        );
        assert_eq!(
            serde_json::from_value::<WorkspaceMode>(serde_json::json!("isolated"))
                .expect("deserialize isolated WorkspaceMode"),
            WorkspaceMode::Isolated
        );
    }

    // Given: 未知の workspace mode / When: WorkspaceMode として JSON 復元 / Then: fail-closed でエラーとなる
    #[test]
    fn workspace_mode_rejects_unknown_value() {
        assert!(serde_json::from_value::<WorkspaceMode>(serde_json::json!("hybrid")).is_err());
    }

    // Given: Branch merge mode / When: JSON 化 / Then: lowercase の "branch" となる
    #[test]
    fn merge_mode_branch_serializes_lowercase() {
        let json = serde_json::to_value(MergeMode::Branch).expect("serialize MergeMode");

        assert_eq!(json, serde_json::json!("branch"));
    }
}
