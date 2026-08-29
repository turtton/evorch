use std::collections::BTreeSet;

use crate::{LayoutError, LayoutNode, WORKSPACE_SCHEMA_VERSION, Window, WindowRect, Workspace};

/// Workspace の全 window と floating pane を横断して検証します。
///
/// # Errors
/// 最初に検出した layout 不変条件違反を返します。
pub fn validate(workspace: &Workspace) -> Result<(), LayoutError> {
    if workspace.version != WORKSPACE_SCHEMA_VERSION {
        return Err(LayoutError::UnsupportedVersion {
            found: workspace.version,
            supported: WORKSPACE_SCHEMA_VERSION,
        });
    }

    let mut placed = BTreeSet::new();
    validate_window(&workspace.main, workspace, &mut placed)?;
    for window in &workspace.extra_windows {
        validate_window(window, workspace, &mut placed)?;
    }
    Ok(())
}

fn validate_window(
    window: &Window,
    workspace: &Workspace,
    placed: &mut BTreeSet<crate::PanelId>,
) -> Result<(), LayoutError> {
    validate_node(&window.root, workspace, placed)?;
    if let Some(rect) = window.rect {
        validate_rect(rect)?;
    }
    for floating in &window.floating {
        validate_rect(floating.rect)?;
        validate_node(&floating.node, workspace, placed)?;
    }
    Ok(())
}

fn validate_node(
    node: &LayoutNode,
    workspace: &Workspace,
    placed: &mut BTreeSet<crate::PanelId>,
) -> Result<(), LayoutError> {
    match node {
        LayoutNode::Split(split) => {
            if !split.fraction.is_finite() || !(0.0 < split.fraction && split.fraction < 1.0) {
                return Err(LayoutError::InvalidFraction {
                    fraction: split.fraction,
                });
            }
            validate_node(&split.first, workspace, placed)?;
            validate_node(&split.second, workspace, placed)
        }
        LayoutNode::Tabs(tabs) => {
            if tabs.panels.is_empty() {
                return Err(LayoutError::EmptyTabs);
            }
            if tabs.active >= tabs.panels.len() {
                return Err(LayoutError::ActiveTabOutOfBounds {
                    active: tabs.active,
                    len: tabs.panels.len(),
                });
            }
            for panel_id in &tabs.panels {
                if !workspace.panels.contains_key(panel_id) {
                    return Err(LayoutError::UnknownPanel {
                        panel_id: panel_id.to_string(),
                    });
                }
                if !placed.insert(panel_id.clone()) {
                    return Err(LayoutError::DuplicatePanel {
                        panel_id: panel_id.to_string(),
                    });
                }
            }
            Ok(())
        }
    }
}

fn validate_rect(rect: WindowRect) -> Result<(), LayoutError> {
    if !rect.width.is_finite()
        || !rect.height.is_finite()
        || rect.width <= 0.0
        || rect.height <= 0.0
    {
        return Err(LayoutError::InvalidRect {
            width: rect.width,
            height: rect.height,
        });
    }
    Ok(())
}
