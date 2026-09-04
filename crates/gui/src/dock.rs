//! `egui_dock` のレイアウトと永続化用 Workspace の相互変換。

use std::collections::BTreeMap;

use egui_dock::{DockState, Node, NodeIndex, NodePath, Split as DockSplit, Surface, SurfaceIndex};
use thiserror::Error;
use workspace_ui::{
    LayoutNode, Panel, PanelId, Split, SplitDirection, Tabs, Window, WindowRect, Workspace,
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
