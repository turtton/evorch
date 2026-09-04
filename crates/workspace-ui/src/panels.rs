use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// パネルの一意識別子。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PanelId(String);

impl PanelId {
    /// 任意の文字列からパネル識別子を構築します。
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// パネル識別子の文字列表現を返します。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PanelId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Workspace が提供するパネル種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelKind {
    Agent,
    Sidebar,
    Agents,
    AgentTranscript,
    Diff,
    Goal,
    MergeApproval,
    Terminal,
    Tasks,
}

impl PanelKind {
    /// パネル種別の既定タイトルを返します。
    pub const fn default_title(self) -> &'static str {
        match self {
            Self::Agent => "Agent",
            Self::Sidebar => "Projects",
            Self::Agents => "Agents",
            Self::AgentTranscript => "Transcript",
            Self::Diff => "Diff",
            Self::Goal => "Goal",
            Self::MergeApproval => "Merge",
            Self::Terminal => "Terminal",
            Self::Tasks => "Tasks",
        }
    }
}

/// Workspace に登録されるパネル定義。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Panel {
    pub id: PanelId,
    pub kind: PanelKind,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// v0.1 の Agent・Terminal・Tasks パネルレジストリを返します。
pub fn default_panels() -> BTreeMap<PanelId, Panel> {
    [
        ("agent-main", PanelKind::Agent),
        ("terminal-main", PanelKind::Terminal),
        ("tasks-main", PanelKind::Tasks),
    ]
    .into_iter()
    .map(|(id, kind)| {
        let panel_id = PanelId::new(id);
        (
            panel_id.clone(),
            Panel {
                id: panel_id,
                kind,
                title: kind.default_title().to_owned(),
                target: None,
            },
        )
    })
    .collect()
}

/// v0.2 の三領域レイアウト用パネルレジストリを返します。
pub fn default_panels_v02() -> BTreeMap<PanelId, Panel> {
    [
        ("sidebar-main", PanelKind::Sidebar, "Projects"),
        ("agent-main", PanelKind::Agent, "Conversation"),
        ("agents-main", PanelKind::Agents, "Agents"),
        ("diff-main", PanelKind::Diff, "Diff"),
        ("terminal-main", PanelKind::Terminal, "Terminal"),
        ("goal-main", PanelKind::Goal, "Goal"),
        ("merge-main", PanelKind::MergeApproval, "Merge"),
    ]
    .into_iter()
    .map(|(id, kind, title)| {
        let panel_id = PanelId::new(id);
        (
            panel_id.clone(),
            Panel {
                id: panel_id,
                kind,
                title: title.to_owned(),
                target: None,
            },
        )
    })
    .collect()
}
