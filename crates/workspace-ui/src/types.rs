use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::LayoutError;
use crate::panels::{Panel, PanelId, default_panels};

pub const WORKSPACE_SCHEMA_VERSION: u32 = 1;

/// 二分割の方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

/// Workspace の再帰レイアウトノード。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LayoutNode {
    Split(Split),
    Tabs(Tabs),
}

/// 2 つのレイアウトノードを分割配置します。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Split {
    pub direction: SplitDirection,
    pub fraction: f32,
    pub first: Box<LayoutNode>,
    pub second: Box<LayoutNode>,
}

/// 同一領域に重ねるパネルタブ。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tabs {
    pub panels: Vec<PanelId>,
    pub active: usize,
}

/// Workspace 内でパネルを挿入する相対位置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "position", content = "panel_id", rename_all = "snake_case")]
pub enum InsertPosition {
    First,
    Last,
    Before(PanelId),
    After(PanelId),
}

/// 浮動領域または OS ウィンドウの矩形。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// メインツリーから分離した浮動ペイン。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Floating {
    pub node: LayoutNode,
    pub rect: WindowRect,
}

/// OS ウィンドウ 1 枚分のレイアウト状態。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub root: LayoutNode,
    #[serde(default)]
    pub floating: Vec<Floating>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rect: Option<WindowRect>,
}

pub type FloatingPane = Floating;
pub type WindowState = Window;

/// バージョン付き Workspace レイアウトとパネルレジストリ。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    pub version: u32,
    pub panels: BTreeMap<PanelId, Panel>,
    pub main: Window,
    #[serde(default)]
    pub extra_windows: Vec<Window>,
}

impl Workspace {
    /// Agent・Terminal・Tasks の nested split 既定レイアウトを構築します。
    pub fn default_v01() -> Self {
        let tabs = |id| {
            LayoutNode::Tabs(Tabs {
                panels: vec![PanelId::new(id)],
                active: 0,
            })
        };

        Self {
            version: WORKSPACE_SCHEMA_VERSION,
            panels: default_panels(),
            main: Window {
                root: LayoutNode::Split(Split {
                    direction: SplitDirection::Horizontal,
                    fraction: 0.2,
                    first: Box::new(tabs("tasks-main")),
                    second: Box::new(LayoutNode::Split(Split {
                        direction: SplitDirection::Vertical,
                        fraction: 0.7,
                        first: Box::new(tabs("agent-main")),
                        second: Box::new(tabs("terminal-main")),
                    })),
                }),
                floating: Vec::new(),
                rect: None,
            },
            extra_windows: Vec::new(),
        }
    }

    /// Workspace の全レイアウト不変条件を検証します。
    ///
    /// # Errors
    /// 最初に検出した不変条件違反を [`LayoutError`] として返します。
    pub fn validate(&self) -> Result<(), LayoutError> {
        crate::validate(self)
    }
}

impl Default for Workspace {
    fn default() -> Self {
        Self::default_v01()
    }
}
