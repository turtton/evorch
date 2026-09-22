use egui_dock::{Node, NodeIndex, NodePath, SurfaceIndex, TabIndex};
use workspace_ui::{Panel, PanelId, PanelKind};

use super::WorkbenchState;
use crate::model::tasks::AgentRunSource;

const REGION_HOME: &str = "subagents-home";

impl<S: AgentRunSource> WorkbenchState<S> {
    pub fn open_subagent_pane(&mut self, run_id: &str) {
        if self.is_conversation_run(run_id) {
            return;
        }
        let id = PanelId::new(format!("agent-{run_id}"));
        if self.panels.contains_key(&id) {
            return;
        }
        let focus = self.subagent_focus_snapshot();
        self.panels.insert(
            id.clone(),
            Panel {
                id: id.clone(),
                kind: PanelKind::SubagentTranscript,
                title: format!("Transcript ({run_id})"),
                target: Some(run_id.to_owned()),
            },
        );
        let bottom = self.subagent_leaves().last().copied();
        let tree = self.dock.main_surface_mut();
        if tree.is_empty() {
            *tree = egui_dock::Tree::new(vec![id]);
        } else if let Some(node) = bottom {
            tree.split_below(node, 0.5, vec![id]);
        } else {
            tree.split_right(NodeIndex::root(), 0.7, vec![id]);
        }
        self.restore_subagent_focus(focus);
        self.equalize_subagent_panes();
    }

    pub fn park_completed_subagent(&mut self, run_id: &str) {
        if self.is_conversation_run(run_id) {
            return;
        }
        let focus = self.subagent_focus_snapshot();
        let id = PanelId::new(format!("agent-{run_id}"));
        if !self.panels.contains_key(&id) {
            self.open_subagent_pane(run_id);
        }
        let mut parked: Vec<_> = self
            .panels
            .iter()
            .filter_map(|(id, panel)| match panel.kind {
                PanelKind::ParkedAgentTranscript(order) => Some((order, id.clone())),
                _ => None,
            })
            .collect();
        parked.sort();
        if !parked.iter().any(|(_, tab)| tab == &id) {
            let order = parked.last().map_or(0, |(order, _)| order + 1);
            if let Some(panel) = self.panels.get_mut(&id) {
                panel.kind = PanelKind::ParkedAgentTranscript(order);
            }
            parked.push((order, id));
        }
        // A retained, idle region stays at the top even when later runs start below it.
        let home = PanelId::new(REGION_HOME);
        let target = if self
            .dock
            .find_tab(&home)
            .is_some_and(|path| path.surface == egui_dock::SurfaceIndex::main())
        {
            home
        } else {
            let leaves = self.subagent_leaves();
            let running = leaves.iter().find_map(|node| {
                self.dock.main_surface()[*node]
                    .tabs()?
                    .iter()
                    .find(|id| {
                        self.panels
                            .get(*id)
                            .is_some_and(|p| p.kind == PanelKind::SubagentTranscript)
                    })
                    .cloned()
            });
            match running {
                Some(id) => id,
                None => {
                    self.panels.insert(
                        home.clone(),
                        Panel {
                            id: home.clone(),
                            kind: PanelKind::SubagentRegion,
                            title: "Subagents".into(),
                            target: None,
                        },
                    );
                    if let Some(path) = self.dock.find_tab(&home) {
                        self.dock.remove_tab(path);
                    }
                    let leaves = self.subagent_leaves();
                    if let Some(node) = leaves.first()
                        && let Node::Leaf(leaf) = &mut self.dock.main_surface_mut()[*node]
                    {
                        // Anchor before removals so the last region leaf cannot be pruned.
                        leaf.tabs.insert(0, home.clone());
                        leaf.active = TabIndex(0);
                    } else {
                        let tree = self.dock.main_surface_mut();
                        if tree.is_empty() {
                            *tree = egui_dock::Tree::new(vec![home.clone()]);
                        } else {
                            tree.split_right(NodeIndex::root(), 0.7, vec![home.clone()]);
                        }
                    }
                    home
                }
            }
        };
        // Only explicitly completed tabs move. Each removal can reindex the entire tree.
        for (_, tab) in &parked {
            if let Some(path) = self.dock.find_tab(tab) {
                self.dock.remove_tab(path);
            }
        }
        if let Some(path) = self.dock.find_tab(&target)
            && let Ok(leaf) = self.dock.leaf_mut(path.node_path())
        {
            leaf.active = path.tab;
            let index = leaf
                .tabs
                .iter()
                .position(|id| id.as_str() == "agent-main")
                .map_or(leaf.tabs.len(), |index| index + 1);
            if leaf.active.0 >= index {
                leaf.active.0 += parked.len();
            }
            leaf.tabs
                .splice(index..index, parked.into_iter().map(|(_, id)| id));
        }
        self.restore_subagent_focus(focus);
        self.equalize_subagent_panes();
    }

    pub(super) fn equalize_subagent_panes(&mut self) {
        let leaves = self.subagent_leaves();
        equalize_subagent_fractions(self.dock.main_surface_mut(), &leaves);
    }

    fn subagent_leaves(&self) -> Vec<NodeIndex> {
        let tree = self.dock.main_surface();
        let mut pending = vec![NodeIndex::root()];
        let mut leaves = Vec::new();
        while let Some(node) = pending.pop() {
            match tree.iter().nth(node.0) {
                Some(Node::Horizontal(_) | Node::Vertical(_)) => {
                    pending.push(node.right());
                    pending.push(node.left());
                }
                Some(Node::Leaf(leaf)) => {
                    if leaf.tabs.iter().any(|id| {
                        id.as_str() == REGION_HOME
                            || self.panels.get(id).is_some_and(|panel| {
                                matches!(
                                    panel.kind,
                                    PanelKind::SubagentTranscript
                                        | PanelKind::ParkedAgentTranscript(_)
                                )
                            })
                    }) {
                        leaves.push(node);
                    }
                }
                Some(Node::Empty) | None => {}
            }
        }
        leaves
    }

    fn subagent_focus_snapshot(&self) -> Option<Vec<PanelId>> {
        self.dock
            .focused_leaf()
            .or_else(|| {
                self.dock
                    .main_surface()
                    .focused_leaf()
                    .map(|node| NodePath {
                        surface: SurfaceIndex::main(),
                        node,
                    })
            })
            .and_then(|path| self.dock.leaf(path).ok())
            .map(|leaf| leaf.tabs.clone())
    }

    fn restore_subagent_focus(&mut self, focus: Option<Vec<PanelId>>) {
        // Resolve saved tab identities after reindexing; a vanished leaf stays
        // unfocused rather than transferring focus to a new or parked pane.
        match focus {
            None => self.dock.set_focused_node_and_surface(NodePath {
                surface: SurfaceIndex::main(),
                node: NodeIndex(usize::MAX),
            }),
            Some(tabs) => {
                if let Some(path) =
                    tabs.iter()
                        .filter(|id| {
                            !self.panels.get(*id).is_some_and(|p| {
                                matches!(p.kind, PanelKind::ParkedAgentTranscript(_))
                            })
                        })
                        .find_map(|id| self.dock.find_tab(id))
                {
                    self.dock.set_focused_node_and_surface(path.node_path());
                } else {
                    self.dock.set_focused_node_and_surface(NodePath {
                        surface: SurfaceIndex::main(),
                        node: NodeIndex(usize::MAX),
                    });
                }
            }
        }
    }
}

fn equalize_subagent_fractions(tree: &mut egui_dock::Tree<PanelId>, leaves: &[NodeIndex]) {
    let Some((&bottom, above)) = leaves.split_last() else {
        return;
    };
    let mut subtree = bottom;
    let mut count = 1.0;
    for &leaf in above.iter().rev() {
        let Some(parent) = subtree.parent() else {
            break;
        };
        if parent.left() != leaf || parent.right() != subtree {
            break;
        }
        match &mut tree[parent] {
            Node::Vertical(split) => {
                count += 1.0;
                split.fraction = 1.0 / count;
                subtree = parent;
            }
            Node::Horizontal(_) | Node::Leaf(_) | Node::Empty => break,
        }
    }
}

#[cfg(test)]
#[path = "subagent_dock_tests.rs"]
mod tests;
