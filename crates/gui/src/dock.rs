//! `egui_dock` のレイアウトと永続化用 Workspace の相互変換。

use std::collections::BTreeMap;

use egui_dock::{DockState, Node, NodeIndex, NodePath, Split as DockSplit, Surface, SurfaceIndex};
use thiserror::Error;
use workspace_ui::{
    LayoutNode, Panel, PanelId, PanelKind, Split, SplitDirection, Tabs, Window, WindowRect,
    Workspace,
};

/// DockState と Workspace の変換エラー。
#[derive(Debug, Error, PartialEq, Eq)]
pub enum DockConvertError {
    #[error("dock tree contains an empty node")]
    EmptyNode,
    #[error("dock tree has no main root node")]
    MissingRoot,
    #[error("dock tree contains an unsupported surface")]
    UnsupportedSurface,
    #[error("dock tree contains an unknown panel '{0}'")]
    UnknownPanel(PanelId),
}

/// Workspace を DockState に再構成します。
pub fn to_dock_state(workspace: &Workspace) -> Result<DockState<PanelId>, DockConvertError> {
    let mut dock = DockState::new(leftmost_tabs(&workspace.main.root)?);
    let root = NodePath::new(SurfaceIndex::main(), NodeIndex::root());
    populate_node(&mut dock, root, &workspace.main.root)?;
    for floating in &workspace.main.floating {
        let surface = dock.add_window(leftmost_tabs(&floating.node)?);
        let window = dock
            .get_window_state_mut(surface)
            .ok_or(DockConvertError::UnsupportedSurface)?;
        window
            .set_position(egui::pos2(floating.rect.x, floating.rect.y))
            .set_size(egui::vec2(floating.rect.width, floating.rect.height));
        populate_node(
            &mut dock,
            NodePath::new(surface, NodeIndex::root()),
            &floating.node,
        )?;
    }
    Ok(dock)
}

/// サイドバーを左側に持つ水平 split の fraction 下限。
/// 従来のスレッド行（pin/タイトル/状態/Pause）が狭いウィンドウでクリップされ、
/// ボタンが操作不能になるのを防ぐ（800px で約 235px、1280px で約 380px）。
pub const MIN_SIDEBAR_FRACTION: f32 = 0.30;

/// サイドバーパネルを左サブツリーに持つ水平 split の fraction を下限値に引き上げます。
pub fn enforce_sidebar_min_fraction(dock: &mut DockState<PanelId>, workspace: &Workspace) {
    let sidebar_ids: Vec<&PanelId> = workspace
        .panels
        .values()
        .filter(|panel| panel.kind == PanelKind::Sidebar)
        .map(|panel| &panel.id)
        .collect();
    if sidebar_ids.is_empty() {
        return;
    }
    let root = NodePath::new(SurfaceIndex::main(), NodeIndex::root());
    let mut targets = Vec::new();
    collect_sidebar_split_paths(dock, root, &sidebar_ids, &mut targets);
    for path in targets {
        if let Ok(Node::Horizontal(split)) = dock.node_mut(path) {
            split.fraction = split.fraction.max(MIN_SIDEBAR_FRACTION);
        }
    }
}

fn collect_sidebar_split_paths(
    dock: &DockState<PanelId>,
    path: NodePath,
    sidebar_ids: &[&PanelId],
    targets: &mut Vec<NodePath>,
) {
    let Ok(node) = dock.node(path) else {
        return;
    };
    if matches!(node, Node::Horizontal(_))
        && subtree_contains_sidebar(dock, child_path(path, true), sidebar_ids)
    {
        targets.push(path);
    }
    if matches!(node, Node::Horizontal(_) | Node::Vertical(_)) {
        collect_sidebar_split_paths(dock, child_path(path, true), sidebar_ids, targets);
        collect_sidebar_split_paths(dock, child_path(path, false), sidebar_ids, targets);
    }
}

fn subtree_contains_sidebar(
    dock: &DockState<PanelId>,
    path: NodePath,
    sidebar_ids: &[&PanelId],
) -> bool {
    match dock.node(path) {
        Ok(Node::Leaf(leaf)) => leaf.tabs.iter().any(|tab| sidebar_ids.contains(&tab)),
        Ok(Node::Horizontal(_)) | Ok(Node::Vertical(_)) => {
            subtree_contains_sidebar(dock, child_path(path, true), sidebar_ids)
                || subtree_contains_sidebar(dock, child_path(path, false), sidebar_ids)
        }
        _ => false,
    }
}

/// DockState から rect に依存しない Workspace を抽出します。
pub fn from_dock_state(
    dock: &DockState<PanelId>,
    panels: &BTreeMap<PanelId, Panel>,
) -> Result<Workspace, DockConvertError> {
    for (_path, panel) in dock.iter_all_tabs() {
        if !panels.contains_key(panel) {
            return Err(DockConvertError::UnknownPanel(panel.clone()));
        }
    }
    let root = extract_node(dock, NodePath::new(SurfaceIndex::main(), NodeIndex::root()))?;
    let floating = dock
        .iter_surfaces_indexed()
        .filter_map(|(surface_index, surface)| match surface {
            Surface::Window(_, state) => Some((surface_index, state)),
            Surface::Empty | Surface::Main(_) => None,
        })
        .map(|(surface_index, state)| {
            let node = extract_node(dock, NodePath::new(surface_index, NodeIndex::root()))?;
            let rect = state.rect();
            // Workspace load remains fail-closed through `validate_rect`; only an unrendered
            // floating window can expose `Rect::NOTHING` while DockState is being extracted.
            let rect = if rect.min.x.is_finite()
                && rect.min.y.is_finite()
                && rect.width().is_finite()
                && rect.height().is_finite()
                && rect.width() > 0.0
                && rect.height() > 0.0
            {
                WindowRect {
                    x: rect.min.x,
                    y: rect.min.y,
                    width: rect.width(),
                    height: rect.height(),
                }
            } else {
                WindowRect {
                    x: 0.0,
                    y: 0.0,
                    width: 1.0,
                    height: 1.0,
                }
            };
            Ok(workspace_ui::Floating { node, rect })
        })
        .collect::<Result<Vec<_>, DockConvertError>>()?;
    Ok(Workspace {
        version: workspace_ui::WORKSPACE_SCHEMA_VERSION,
        panels: panels.clone(),
        main: Window {
            root,
            floating,
            rect: None,
        },
        extra_windows: Vec::new(),
    })
}

fn leftmost_tabs(node: &LayoutNode) -> Result<Vec<PanelId>, DockConvertError> {
    match node {
        LayoutNode::Tabs(tabs) if !tabs.panels.is_empty() => Ok(tabs.panels.clone()),
        LayoutNode::Tabs(_) => Err(DockConvertError::MissingRoot),
        LayoutNode::Split(split) => leftmost_tabs(&split.first),
    }
}

fn populate_node(
    dock: &mut DockState<PanelId>,
    path: NodePath,
    node: &LayoutNode,
) -> Result<(), DockConvertError> {
    match node {
        LayoutNode::Tabs(tabs) => {
            let target = dock
                .node_mut(path)
                .map_err(|_| DockConvertError::MissingRoot)?;
            let target_tabs = target.tabs_mut().ok_or(DockConvertError::MissingRoot)?;
            if target_tabs.len() != tabs.panels.len() {
                return Err(DockConvertError::MissingRoot);
            }
            target_tabs.clone_from_slice(&tabs.panels);
            let leaf = dock
                .leaf_mut(path)
                .map_err(|_| DockConvertError::MissingRoot)?;
            leaf.set_active_tab(tabs.active)
                .map_err(|_| DockConvertError::MissingRoot)
        }
        LayoutNode::Split(split) => {
            let direction = match split.direction {
                SplitDirection::Horizontal => DockSplit::Right,
                SplitDirection::Vertical => DockSplit::Below,
            };
            let [old, new] = dock[path.surface].split(
                path.node,
                direction,
                split.fraction,
                Node::leaf_with(leftmost_tabs(&split.second)?),
            );
            populate_node(dock, NodePath::new(path.surface, old), &split.first)?;
            populate_node(dock, NodePath::new(path.surface, new), &split.second)
        }
    }
}

fn extract_node(dock: &DockState<PanelId>, path: NodePath) -> Result<LayoutNode, DockConvertError> {
    let node = dock.node(path).map_err(|_| DockConvertError::MissingRoot)?;
    match node {
        Node::Empty => Err(DockConvertError::EmptyNode),
        Node::Leaf(leaf) => Ok(LayoutNode::Tabs(Tabs {
            panels: leaf.tabs.clone(),
            active: leaf.active.0,
        })),
        Node::Horizontal(split) | Node::Vertical(split) => {
            let direction = match node {
                Node::Horizontal(_) => SplitDirection::Horizontal,
                Node::Vertical(_) => SplitDirection::Vertical,
                Node::Empty | Node::Leaf(_) => return Err(DockConvertError::EmptyNode),
            };
            Ok(LayoutNode::Split(Split {
                direction,
                fraction: split.fraction,
                first: Box::new(extract_node(dock, child_path(path, true))?),
                second: Box::new(extract_node(dock, child_path(path, false))?),
            }))
        }
    }
}

fn child_path(path: NodePath, first: bool) -> NodePath {
    let offset = if first { 1 } else { 2 };
    NodePath::new(path.surface, NodeIndex(path.node.0 * 2 + offset))
}

#[cfg(test)]
mod tests {
    use super::{MIN_SIDEBAR_FRACTION, enforce_sidebar_min_fraction, to_dock_state};
    use egui_dock::{Node, NodeIndex, NodePath, SurfaceIndex};
    use std::collections::BTreeMap;
    use workspace_ui::{
        LayoutNode, Panel, PanelId, PanelKind, Split, SplitDirection, Tabs, Window, Workspace,
    };

    fn panel(id: &str, kind: PanelKind) -> (PanelId, Panel) {
        let panel_id = PanelId::new(id);
        (
            panel_id.clone(),
            Panel {
                id: panel_id,
                kind,
                title: id.to_owned(),
                target: None,
            },
        )
    }

    fn tabs(panels: &[&str]) -> LayoutNode {
        LayoutNode::Tabs(Tabs {
            panels: panels.iter().map(|id| PanelId::new(*id)).collect(),
            active: 0,
        })
    }

    fn root_fraction(dock: &egui_dock::DockState<PanelId>) -> f32 {
        let root = NodePath::new(SurfaceIndex::main(), NodeIndex::root());
        match dock.node(root) {
            Ok(Node::Horizontal(split)) | Ok(Node::Vertical(split)) => split.fraction,
            other => panic!("expected split root, got {other:?}"),
        }
    }

    #[test]
    fn enforce_raises_sidebar_split_fraction_to_minimum() {
        // Given: a horizontal root split with the sidebar on the left below the minimum
        let panels: BTreeMap<_, _> = [
            panel("sidebar", PanelKind::Sidebar),
            panel("agent", PanelKind::Agent),
            panel("goal", PanelKind::Goal),
        ]
        .into_iter()
        .collect();
        let workspace = Workspace {
            version: 1,
            panels,
            main: Window {
                root: LayoutNode::Split(Split {
                    direction: SplitDirection::Horizontal,
                    fraction: 0.2,
                    first: Box::new(tabs(&["sidebar"])),
                    second: Box::new(LayoutNode::Split(Split {
                        direction: SplitDirection::Vertical,
                        fraction: 0.6,
                        first: Box::new(tabs(&["agent"])),
                        second: Box::new(tabs(&["goal"])),
                    })),
                }),
                floating: Vec::new(),
                rect: None,
            },
            extra_windows: Vec::new(),
        };
        let mut dock = to_dock_state(&workspace).expect("workspace conversion succeeds");

        // When: enforcing the sidebar minimum fraction
        enforce_sidebar_min_fraction(&mut dock, &workspace);

        // Then: the sidebar split is raised while the unrelated split keeps its fraction
        assert_eq!(root_fraction(&dock), MIN_SIDEBAR_FRACTION);
        let right = NodePath::new(SurfaceIndex::main(), NodeIndex(2));
        match dock.node(right) {
            Ok(Node::Vertical(split)) => assert_eq!(split.fraction, 0.6),
            other => panic!("expected vertical right split, got {other:?}"),
        }
    }

    #[test]
    fn enforce_keeps_fraction_already_above_minimum() {
        // Given: a sidebar split already above the minimum
        let panels: BTreeMap<_, _> = [
            panel("sidebar", PanelKind::Sidebar),
            panel("agent", PanelKind::Agent),
        ]
        .into_iter()
        .collect();
        let workspace = Workspace {
            version: 1,
            panels,
            main: Window {
                root: LayoutNode::Split(Split {
                    direction: SplitDirection::Horizontal,
                    fraction: 0.45,
                    first: Box::new(tabs(&["sidebar"])),
                    second: Box::new(tabs(&["agent"])),
                }),
                floating: Vec::new(),
                rect: None,
            },
            extra_windows: Vec::new(),
        };
        let mut dock = to_dock_state(&workspace).expect("workspace conversion succeeds");

        // When: enforcing the sidebar minimum fraction
        enforce_sidebar_min_fraction(&mut dock, &workspace);

        // Then: the existing fraction is preserved
        assert_eq!(root_fraction(&dock), 0.45);
    }

    #[test]
    fn enforce_is_noop_without_sidebar() {
        // Given: a horizontal split with no sidebar panel anywhere
        let panels: BTreeMap<_, _> = [
            panel("agent", PanelKind::Agent),
            panel("goal", PanelKind::Goal),
        ]
        .into_iter()
        .collect();
        let workspace = Workspace {
            version: 1,
            panels,
            main: Window {
                root: LayoutNode::Split(Split {
                    direction: SplitDirection::Horizontal,
                    fraction: 0.2,
                    first: Box::new(tabs(&["agent"])),
                    second: Box::new(tabs(&["goal"])),
                }),
                floating: Vec::new(),
                rect: None,
            },
            extra_windows: Vec::new(),
        };
        let mut dock = to_dock_state(&workspace).expect("workspace conversion succeeds");

        // When: enforcing the sidebar minimum fraction
        enforce_sidebar_min_fraction(&mut dock, &workspace);

        // Then: the fraction is untouched
        assert_eq!(root_fraction(&dock), 0.2);
    }
}
