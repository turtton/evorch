//! `egui_dock` のレイアウトと永続化用 Workspace の相互変換。

pub mod subagent_layout;

use egui_dock::{DockState, Node, NodeIndex, NodePath, SurfaceIndex};

/// サイドバーを左側に持つ水平 split の fraction 下限。
/// 従来のスレッド行（pin/タイトル/状態/Pause）が狭いウィンドウでクリップされ、
/// ボタンが操作不能になるのを防ぐ（800px で約 235px、1280px で約 380px）。
pub const MIN_SIDEBAR_FRACTION: f32 = 0.30;

/// Raises to [`MIN_SIDEBAR_FRACTION`] the fraction of each horizontal split of the main
/// surface whose left subtree holds a tab `is_sidebar` accepts.
pub fn enforce_min_fraction_left_of<T>(dock: &mut DockState<T>, is_sidebar: impl Fn(&T) -> bool) {
    let root = NodePath::new(SurfaceIndex::main(), NodeIndex::root());
    let mut targets = Vec::new();
    collect_sidebar_split_paths(dock, root, &is_sidebar, &mut targets);
    for path in targets {
        if let Ok(Node::Horizontal(split)) = dock.node_mut(path) {
            split.fraction = split.fraction.max(MIN_SIDEBAR_FRACTION);
        }
    }
}

fn collect_sidebar_split_paths<T>(
    dock: &DockState<T>,
    path: NodePath,
    is_sidebar: &impl Fn(&T) -> bool,
    targets: &mut Vec<NodePath>,
) {
    let Ok(node) = dock.node(path) else {
        return;
    };
    if matches!(node, Node::Horizontal(_))
        && subtree_contains_sidebar(dock, child_path(path, true), is_sidebar)
    {
        targets.push(path);
    }
    if matches!(node, Node::Horizontal(_) | Node::Vertical(_)) {
        collect_sidebar_split_paths(dock, child_path(path, true), is_sidebar, targets);
        collect_sidebar_split_paths(dock, child_path(path, false), is_sidebar, targets);
    }
}

fn subtree_contains_sidebar<T>(
    dock: &DockState<T>,
    path: NodePath,
    is_sidebar: &impl Fn(&T) -> bool,
) -> bool {
    match dock.node(path) {
        Ok(Node::Leaf(leaf)) => leaf.tabs.iter().any(is_sidebar),
        Ok(Node::Horizontal(_)) | Ok(Node::Vertical(_)) => {
            subtree_contains_sidebar(dock, child_path(path, true), is_sidebar)
                || subtree_contains_sidebar(dock, child_path(path, false), is_sidebar)
        }
        _ => false,
    }
}

/// The path of a split's first (left or upper) or second child.
pub fn child_path(path: NodePath, first: bool) -> NodePath {
    let offset = if first { 1 } else { 2 };
    NodePath::new(path.surface, NodeIndex(path.node.0 * 2 + offset))
}
