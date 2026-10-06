//! AgentRun の識別子・設定、および表層用の DTO を定義します。

use std::fmt;
use std::path::PathBuf;

use event_bus::AgentRunPhase;
use serde::{Deserialize, Serialize};

/// ランタイム内の AgentRun を一意に識別する newtype。
///
/// 新規 run は ULID と同じ配置 (上位 48 bit が UNIX ミリ秒、残り 80 bit が乱数) で
/// 採番され、値の大小が作成順と一致する。[`Display`](std::fmt::Display) は
/// `run-{26 桁の小文字 Crockford base32}` を返す。旧形式の連番 `run-{n}` は
/// 時刻成分 0 の値として保持し、同じ形式で表示・解釈する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RunId(u128);

const CROCKFORD: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
const ENCODED_LEN: usize = 26;
const RANDOM_BITS: u32 = 80;

impl RunId {
    /// 旧形式の連番 ID を構築する。
    pub const fn new(id: u64) -> Self {
        Self(id as u128)
    }

    /// 指定時刻と乱数から時刻順の ID を構築する。乱数は下位 80 bit のみ使う。
    pub(crate) const fn from_parts(unix_ms: u64, random: u128) -> Self {
        let time = (unix_ms as u128 & ((1 << 48) - 1)) << RANDOM_BITS;
        Self(time | (random & ((1 << RANDOM_BITS) - 1)))
    }

    const fn is_sequential(self) -> bool {
        self.0 <= u64::MAX as u128
    }

    /// 旧形式の連番 ID であればその番号を返す。
    pub const fn sequential_value(self) -> Option<u64> {
        if self.is_sequential() {
            Some(self.0 as u64)
        } else {
            None
        }
    }

    /// 直後の ID を返す。同一ミリ秒内の単調増加に使う。
    pub(crate) const fn successor(self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_sequential() {
            return write!(f, "run-{}", self.0);
        }
        let mut encoded = [0_u8; ENCODED_LEN];
        let mut value = self.0;
        for slot in encoded.iter_mut().rev() {
            *slot = CROCKFORD[(value & 0x1f) as usize];
            value >>= 5;
        }
        f.write_str("run-")?;
        // CROCKFORD は ASCII のみ。
        f.write_str(std::str::from_utf8(&encoded).map_err(|_| fmt::Error)?)
    }
}

/// `run-` 接頭辞付きの run ID として解釈できない文字列。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid run ID: {0}")]
pub struct ParseRunIdError(String);

impl std::str::FromStr for RunId {
    type Err = ParseRunIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = || ParseRunIdError(value.to_owned());
        let body = value.strip_prefix("run-").ok_or_else(invalid)?;
        if body.len() == ENCODED_LEN {
            let mut decoded = 0_u128;
            for (index, byte) in body.bytes().enumerate() {
                let digit = CROCKFORD
                    .iter()
                    .position(|candidate| *candidate == byte.to_ascii_lowercase())
                    .ok_or_else(invalid)? as u128;
                // 26 桁 x 5 bit = 130 bit のため、先頭桁は 0..=7 に限る。
                if index == 0 && digit > 7 {
                    return Err(invalid());
                }
                decoded = (decoded << 5) | digit;
            }
            let id = Self(decoded);
            return if id.is_sequential() {
                Err(invalid())
            } else {
                Ok(id)
            };
        }
        if body.is_empty() || !body.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid());
        }
        body.parse::<u64>().map(Self::new).map_err(|_| invalid())
    }
}

impl Serialize for RunId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for RunId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = RunId;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a run ID string or a legacy numeric run ID")
            }

            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<RunId, E> {
                Ok(RunId::new(value))
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<RunId, E> {
                value.parse().map_err(E::custom)
            }
        }

        deserializer.deserialize_any(Visitor)
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

/// Internal learning stages are assigned by the runtime, never by delegate arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunPurpose {
    #[default]
    General,
    /// Runtime-issued, read-only review of a generic thread objective.
    ThreadGoalReview {
        root_run_id: RunId,
        epoch: u64,
    },
    LessonExtract {
        source_run_id: RunId,
    },
    LessonReview {
        source_run_id: RunId,
        extraction_run_id: RunId,
    },
}

/// AgentRun の実行設定。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunConfig {
    /// Explicit host /goal input, consumed before starting a chat root.
    pub initial_thread_goal: Option<(String, Vec<String>)>,
    pub budget: crate::budget_tracker::BudgetSettings,
    /// Durable task identity when this run is attached to a task.
    pub task_id: Option<String>,
    pub topology: crate::CoordinationTopology,
    pub team: Option<crate::team_context::TeamContext>,
    pub team_task: Option<crate::team::TaskSpec>,
    pub finding_store: Option<PathBuf>,
    pub team_store: Option<crate::team_context::TeamStore>,
    pub delegation_value: Option<String>,
    pub memory: Option<crate::memory::MemoryBoundary>,
    pub learning_internal: bool,
    /// Trusted runtime authority for direct conversation roots; never accepted by
    /// model-facing delegation tools. Defaults to false; category alone grants nothing.
    pub conversation: bool,
    /// Trusted runtime authority; not accepted by model-facing delegation tools.
    pub purpose: RunPurpose,
    pub ownership: Option<crate::ownership::OwnerPermit>,
    pub images: Vec<DelegateImage>,
    /// Explicit model selection for this run; absent means normal routing.
    pub model_preference: Option<crate::ModelPreference>,
    /// ユーザー入力を待ち受ける対話モードか。既定は `false` (非対話)。
    pub interactive: bool,
    /// Interactive run keeps waiting after each Stop until cancel/inbox close (chat session).
    pub keep_alive: bool,
    /// run の表示名。`None` の場合はロール名へフォールバックする。
    pub name: Option<String>,
    /// Worker 専用の実行プロファイル。システムプロンプトの category overlay と
    /// カテゴリにバインドされた論理モデルの選択に使う。
    /// `None` の場合は overlay を挿入しない。
    pub category: Option<String>,
    /// 委譲時に子 run の初期 System メッセージへ本文を注入する skill 名。既定は空。
    pub load_skills: Vec<String>,
    /// 親 workspace を共有するか、専用 git worktree を使用するか。
    pub workspace_mode: WorkspaceMode,
    /// isolated workspace の変更を統合する方法。
    pub merge_mode: MergeMode,
    /// isolated workspace で checkout する既存 branch。`None` なら run 専用の新規
    /// branch (`evorch/task/run-<id>`) を作成する。既定は `None`。worktree path は
    /// この値からは導出されず、常に run 名 (`run-<id>`) から決まる (issue #73 D2)。
    pub workspace_branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegateImage {
    pub media_type: String,
    pub data: String,
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
    /// run 開始時に決定した作業 root。cleanup 成功・handoff 切離し後は `None`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_root: Option<PathBuf>,
    /// isolated workspace の変更を統合する方法。
    pub merge_mode: MergeMode,
}

impl WorkspaceInspection {
    /// file snapshot を取る root。shared run も起動時ではなく自身の作業 root を使う。
    pub(crate) fn snapshot_root(&self) -> Option<PathBuf> {
        self.worktree_path
            .clone()
            .or_else(|| self.active_root.clone())
    }
}

/// AgentRun の要約 (一覧表示用 DTO)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentSummary {
    /// 実行 ID。
    pub run_id: RunId,
    /// 親 run の ID。ルート run では `None`。
    pub parent_run_id: Option<RunId>,
    /// 表示名。`RunConfig::name` 未指定時はロール名。
    pub name: String,
    /// ロール名識別子。
    pub role_name: String,
    /// 現在の位相。
    pub phase: AgentRunPhase,
    /// 選択済みモデル識別子。
    pub model: String,
    /// ワーカーに指定されたカテゴリ。
    pub category: Option<String>,
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
    /// 割り当て済み workspace の検査情報。常に `Some`。
    pub workspace: Option<WorkspaceInspection>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_inspection_serializes_active_root_without_changing_existing_keys() {
        let workspace = WorkspaceInspection {
            mode: WorkspaceMode::Isolated,
            branch: Some("evorch/task/run-7".into()),
            worktree_path: Some(PathBuf::from("/project/.evorch/worktrees/run-7")),
            active_root: Some(PathBuf::from("/project/.evorch/worktrees/run-7")),
            merge_mode: MergeMode::Branch,
        };

        assert_eq!(
            serde_json::to_value(workspace).expect("serialize WorkspaceInspection"),
            serde_json::json!({
                "mode": "isolated",
                "branch": "evorch/task/run-7",
                "worktree_path": "/project/.evorch/worktrees/run-7",
                "active_root": "/project/.evorch/worktrees/run-7",
                "merge_mode": "branch",
            }),
        );
    }

    #[test]
    fn workspace_inspection_omits_only_absent_active_root() {
        let workspace = WorkspaceInspection {
            mode: WorkspaceMode::Shared,
            branch: None,
            worktree_path: None,
            active_root: None,
            merge_mode: MergeMode::Branch,
        };

        assert_eq!(
            serde_json::to_value(workspace).expect("serialize WorkspaceInspection"),
            serde_json::json!({
                "mode": "shared",
                "branch": null,
                "worktree_path": null,
                "merge_mode": "branch",
            }),
        );
    }

    // Given: 数値 7 と 0 の RunId / When: Display / Then: "run-{n}" 形式 (イベント run_id と同一形式)
    #[test]
    fn run_id_displays_as_run_prefixed() {
        assert_eq!(RunId::new(7).to_string(), "run-7");
        assert_eq!(RunId::new(0).to_string(), "run-0");
    }

    // Given: 旧形式と時刻順の RunId / When: JSON 往復 / Then: 表示形式の文字列で保存し、旧数値形式も読める
    #[test]
    fn run_id_serializes_as_display_string_and_reads_legacy_numbers() {
        let timed = RunId::from_parts(1_700_000_000_000, 0xabc);
        let json = serde_json::to_value(timed).expect("serialize RunId");

        assert_eq!(json, serde_json::json!(timed.to_string()));
        assert_eq!(serde_json::from_value::<RunId>(json).unwrap(), timed);
        assert_eq!(
            serde_json::from_value::<RunId>(serde_json::json!(42)).unwrap(),
            RunId::new(42)
        );
    }

    // Given: 時刻順 ID の文字列 / When: 解釈 / Then: 26 桁 base32 のみ受理し旧連番と衝突しない
    #[test]
    fn run_id_parse_accepts_only_canonical_forms() {
        let timed = RunId::from_parts(1_700_000_000_000, u128::MAX);
        let text = timed.to_string();

        assert_eq!(text.len(), "run-".len() + 26);
        assert_eq!(text.parse::<RunId>(), Ok(timed));
        assert_eq!(
            text.to_uppercase().replace("RUN-", "run-").parse::<RunId>(),
            Ok(timed)
        );
        assert_eq!("run-7".parse::<RunId>(), Ok(RunId::new(7)));
        for invalid in [
            "7",
            "run-",
            "run-+7",
            "run-7x",
            "run-00000000000000000000000007",
            "run-8zzzzzzzzzzzzzzzzzzzzzzzzz",
        ] {
            assert!(invalid.parse::<RunId>().is_err(), "{invalid}");
        }
        assert!(RunId::new(u64::MAX) < timed);
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

    // Given: RunConfig / When: Default / Then: load_skills は空 (skill 注入なし)
    #[test]
    fn run_config_default_has_no_load_skills() {
        assert!(RunConfig::default().load_skills.is_empty());
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

    // Given: RunConfig / When: Default / Then: workspace_branch は None (専用 branch 新規作成が既定)
    #[test]
    fn run_config_default_has_no_workspace_branch() {
        assert!(RunConfig::default().workspace_branch.is_none());
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
