use crate::{LayoutNode, Panel, PanelId, PanelKind, Tabs, Workspace};

pub(crate) fn notifications(workspace: &mut Workspace) {
    let id = PanelId::new("notifications-main");
    if find_tabs(workspace, &|tabs| tabs.panels.contains(&id)).is_some() {
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
