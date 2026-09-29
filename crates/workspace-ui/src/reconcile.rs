use crate::{LayoutNode, Panel, PanelId, PanelKind, Split, SplitDirection, Tabs, Workspace};

pub(crate) fn notifications(workspace: &mut Workspace) {
    let id = PanelId::new("notifications-main");
    if find_tabs(workspace, &|tabs| tabs.panels.contains(&id)).is_some() {
        retire_agents(workspace);
        move_main_diff_to_globals(workspace);
        return;
    }
    let anchors: Vec<_> = workspace
        .panels
        .values()
        .filter(|panel| matches!(panel.kind, PanelKind::Tasks | PanelKind::Agents))
        .map(|panel| panel.id.clone())
        .collect();
    match find_tabs(workspace, &|tabs| {
        tabs.panels.iter().any(|id| anchors.contains(id))
    }) {
        Some(tabs) => tabs.panels.push(id.clone()),
        None => {
            let mut node = &mut workspace.main.root;
            loop {
                match node {
                    LayoutNode::Split(split) => node = &mut split.second,
                    LayoutNode::Tabs(tabs) => {
                        tabs.panels.push(id.clone());
                        break;
                    }
                }
            }
        }
    }
    workspace.panels.entry(id.clone()).or_insert_with(|| Panel {
        id,
        kind: PanelKind::Notifications,
        title: PanelKind::Notifications.default_title().into(),
        target: None,
    });
    retire_agents(workspace);
    move_main_diff_to_globals(workspace);
}

// Diff describes the project checkout, so main-window Diff tabs share the global
// leaf. Detached panes retain their explicit floating-window placement.
fn move_main_diff_to_globals(workspace: &mut Workspace) {
    let anchors: Vec<_> = workspace
        .panels
        .values()
        .filter(|panel| matches!(panel.kind, PanelKind::Tasks | PanelKind::Notifications))
        .map(|panel| panel.id.clone())
        .collect();
    let Some(globals) = find_node_tabs(&mut workspace.main.root, &|tabs| {
        tabs.panels.iter().any(|id| anchors.contains(id))
    }) else {
        return;
    };
    let global_ids = globals.panels.clone();
    let diff_ids: Vec<_> = workspace
        .panels
        .values()
        .filter(|panel| panel.kind == PanelKind::Diff && !global_ids.contains(&panel.id))
        .map(|panel| panel.id.clone())
        .filter(|id| {
            find_node_tabs(&mut workspace.main.root, &|tabs| tabs.panels.contains(id)).is_some()
        })
        .collect();
    if diff_ids.is_empty() {
        return;
    }
    // A global anchor remains, so removing Diff cannot empty the main tree.
    workspace.main.root = prune(workspace.main.root.clone(), &diff_ids)
        .expect("global tabs remain after moving Diff");
    if let Some(globals) = find_node_tabs(&mut workspace.main.root, &|tabs| {
        tabs.panels.iter().any(|id| anchors.contains(id))
    }) {
        globals.panels.extend(diff_ids);
    }
}

fn find_tabs<'a>(
    workspace: &'a mut Workspace,
    matches: &impl Fn(&Tabs) -> bool,
) -> Option<&'a mut Tabs> {
    std::iter::once(&mut workspace.main)
        .chain(&mut workspace.extra_windows)
        .flat_map(|window| {
            std::iter::once(&mut window.root)
                .chain(window.floating.iter_mut().map(|pane| &mut pane.node))
        })
        .find_map(|node| find_node_tabs(node, matches))
}

fn find_node_tabs<'a>(
    node: &'a mut LayoutNode,
    matches: &impl Fn(&Tabs) -> bool,
) -> Option<&'a mut Tabs> {
    match node {
        LayoutNode::Split(split) => find_node_tabs(&mut split.first, matches)
            .or_else(|| find_node_tabs(&mut split.second, matches)),
        LayoutNode::Tabs(tabs) => matches(tabs).then_some(tabs),
    }
}

// Update the former Agents-based arrangement once. Custom layouts saved after
// this change keep their chosen positions when reopened.
fn retire_agents(workspace: &mut Workspace) {
    let obsolete: Vec<_> = workspace
        .panels
        .values()
        .filter(|panel| panel.kind == PanelKind::Agents)
        .map(|panel| panel.id.clone())
        .collect();
    if obsolete.is_empty() {
        return;
    }
    let globals: Vec<_> = workspace
        .panels
        .values()
        .filter(|panel| {
            matches!(
                panel.kind,
                PanelKind::Tasks | PanelKind::Notifications | PanelKind::Memory | PanelKind::Arena
            )
        })
        .map(|panel| panel.id.clone())
        .collect();
    let removed: Vec<_> = obsolete.iter().chain(&globals).cloned().collect();
    let fallback = || {
        LayoutNode::Tabs(Tabs {
            panels: vec![PanelId::new("subagents-home")],
            active: 0,
        })
    };
    workspace.main.root = prune(workspace.main.root.clone(), &removed).unwrap_or_else(fallback);
    workspace.main.floating.retain_mut(|pane| {
        if let Some(node) = prune(pane.node.clone(), &removed) {
            pane.node = node;
            true
        } else {
            false
        }
    });
    workspace.extra_windows.retain_mut(|window| {
        window.floating.retain_mut(|pane| {
            if let Some(node) = prune(pane.node.clone(), &removed) {
                pane.node = node;
                true
            } else {
                false
            }
        });
        if let Some(node) = prune(window.root.clone(), &removed) {
            window.root = node;
            true
        } else if !window.floating.is_empty() {
            window.root = window.floating.remove(0).node;
            true
        } else {
            false
        }
    });
    for id in obsolete {
        workspace.panels.remove(&id);
    }
    let home = PanelId::new("subagents-home");
    workspace
        .panels
        .entry(home.clone())
        .or_insert_with(|| Panel {
            id: home.clone(),
            kind: PanelKind::SubagentRegion,
            title: "Subagents".into(),
            target: None,
        });
    if find_tabs(workspace, &|tabs| tabs.panels.contains(&home)).is_none()
        && !split_anchor(
            &mut workspace.main.root,
            "diff-main",
            vec![home.clone()],
            false,
        )
    {
        let root = workspace.main.root.clone();
        workspace.main.root = LayoutNode::Split(Split {
            direction: SplitDirection::Horizontal,
            fraction: 0.7,
            first: Box::new(root),
            second: Box::new(fallback()),
        });
    }
    if !globals.is_empty()
        && !split_anchor(
            &mut workspace.main.root,
            "sidebar-main",
            globals.clone(),
            true,
        )
    {
        let root = workspace.main.root.clone();
        workspace.main.root = LayoutNode::Split(Split {
            direction: SplitDirection::Horizontal,
            fraction: 0.2,
            first: Box::new(LayoutNode::Tabs(Tabs {
                panels: globals,
                active: 0,
            })),
            second: Box::new(root),
        });
    }
}

fn prune(node: LayoutNode, removed: &[PanelId]) -> Option<LayoutNode> {
    match node {
        LayoutNode::Tabs(mut tabs) => {
            let active = tabs.panels.get(tabs.active).cloned();
            tabs.panels.retain(|id| !removed.contains(id));
            tabs.active = active
                .and_then(|id| tabs.panels.iter().position(|candidate| candidate == &id))
                .unwrap_or(0);
            (!tabs.panels.is_empty()).then_some(LayoutNode::Tabs(tabs))
        }
        LayoutNode::Split(mut split) => {
            match (prune(*split.first, removed), prune(*split.second, removed)) {
                (Some(first), Some(second)) => {
                    split.first = Box::new(first);
                    split.second = Box::new(second);
                    Some(LayoutNode::Split(split))
                }
                (first, second) => first.or(second),
            }
        }
    }
}

fn split_anchor(node: &mut LayoutNode, anchor: &str, panels: Vec<PanelId>, below: bool) -> bool {
    match node {
        LayoutNode::Tabs(tabs) if tabs.panels.iter().any(|id| id.as_str() == anchor) => {
            let original = node.clone();
            let inserted = LayoutNode::Tabs(Tabs { panels, active: 0 });
            let (first, second) = if below {
                (original, inserted)
            } else {
                (inserted, original)
            };
            *node = LayoutNode::Split(Split {
                direction: SplitDirection::Vertical,
                fraction: if below { 0.65 } else { 0.5 },
                first: Box::new(first),
                second: Box::new(second),
            });
            true
        }
        LayoutNode::Tabs(_) => false,
        LayoutNode::Split(split) => {
            split_anchor(&mut split.first, anchor, panels.clone(), below)
                || split_anchor(&mut split.second, anchor, panels, below)
        }
    }
}
