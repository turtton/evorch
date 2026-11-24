//! Where the subagent transcript panes go in a dock, shared by the embedded workbench and
//! the core client: a running subagent gets a split of its own below the last subagent
//! leaf, the splits share the column equally, a finished one is parked as a tab of the
//! subagents' leaf, and none of this moves the focus. Generic over the tab type; it knows
//! only `egui_dock`, so each caller says which of its tabs are subagent tabs.
use egui_dock::{DockState, Node, NodeIndex, NodePath, SurfaceIndex, Tree};

/// The leaves of `tree` holding a tab `is_subagent` accepts, top to bottom.
pub fn subagent_leaves<T>(tree: &Tree<T>, is_subagent: impl Fn(&T) -> bool) -> Vec<NodeIndex> {
    let mut pending = vec![NodeIndex::root()];
    let mut leaves = Vec::new();
    while let Some(node) = pending.pop() {
        match tree.iter().nth(node.0) {
            Some(Node::Horizontal(_) | Node::Vertical(_)) => {
                pending.push(node.right());
                pending.push(node.left());
            }
            Some(Node::Leaf(leaf)) => {
                if leaf.tabs.iter().any(&is_subagent) {
                    leaves.push(node);
                }
            }
            Some(Node::Empty) | None => {}
        }
    }
    leaves
}

/// Give `tab` its own leaf below `bottom`, the last subagent leaf, or to the right of
/// everything when there is none yet.
pub fn dock_below<T>(tree: &mut Tree<T>, bottom: Option<NodeIndex>, tab: T) {
    if tree.is_empty() {
        *tree = Tree::new(vec![tab]);
    } else if let Some(node) = bottom {
        tree.split_below(node, 0.5, vec![tab]);
    } else {
        tree.split_right(NodeIndex::root(), 0.7, vec![tab]);
    }
}

/// Share the chain of splits that ends in the last of `leaves` equally between them,
/// leaving every other split as it is.
pub fn equalize_subagent_fractions<T>(tree: &mut Tree<T>, leaves: &[NodeIndex]) {
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

/// The tabs of the focused leaf, to find it again after the tree is reindexed.
pub fn focus_snapshot<T: Clone>(dock: &DockState<T>) -> Option<Vec<T>> {
    dock.focused_leaf()
        .or_else(|| {
            dock.main_surface().focused_leaf().map(|node| NodePath {
                surface: SurfaceIndex::main(),
                node,
            })
        })
        .and_then(|path| dock.leaf(path).ok())
        .map(|leaf| leaf.tabs.clone())
}

/// Focus the leaf `focus` was taken of again. A leaf that is gone, or that only a parked
/// tab of it is left in, stays unfocused rather than handing the focus to another pane.
pub fn restore_focus<T: PartialEq>(
    dock: &mut DockState<T>,
    focus: Option<Vec<T>>,
    is_parked: impl Fn(&T) -> bool,
) {
    let path = focus.and_then(|tabs| {
        tabs.iter()
            .filter(|tab| !is_parked(tab))
            .find_map(|tab| dock.find_tab(tab))
    });
    match path {
        Some(path) => dock.set_focused_node_and_surface(path.node_path()),
        None => dock.set_focused_node_and_surface(NodePath {
            surface: SurfaceIndex::main(),
            node: NodeIndex(usize::MAX),
        }),
    }
}

/// Put `parked`, in order and out of the dock, into the leaf of `target`: after the tab
/// `after` accepts when the leaf has one, else at its end. `target` is brought to the
/// front of its leaf.
pub fn splice_parked<T: PartialEq>(
    dock: &mut DockState<T>,
    target: &T,
    parked: Vec<T>,
    after: impl Fn(&T) -> bool,
) {
    let Some(path) = dock.find_tab(target) else {
        return;
    };
    let Ok(leaf) = dock.leaf_mut(path.node_path()) else {
        return;
    };
    leaf.active = path.tab;
    let index = leaf
        .tabs
        .iter()
        .position(after)
        .map_or(leaf.tabs.len(), |index| index + 1);
    if leaf.active.0 >= index {
        leaf.active.0 += parked.len();
    }
    leaf.tabs.splice(index..index, parked);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree_with_column() -> (Tree<&'static str>, NodeIndex) {
        let mut tree = Tree::new(vec!["main"]);
        let [_, column] = tree.split_right(NodeIndex::root(), 0.7, vec!["home", "other"]);
        (tree, column)
    }

    // Given: a column holding the subagents' home, and two subagents docked below it
    // Then: the leaves are listed top to bottom, each new one below the last
    #[test]
    fn subagents_are_docked_below_the_last_subagent_leaf() {
        let (mut tree, column) = tree_with_column();
        let is_subagent = |tab: &&str| *tab != "main" && *tab != "other";
        assert_eq!(subagent_leaves(&tree, is_subagent), [column]);
        dock_below(&mut tree, Some(column), "run-a");
        let leaves = subagent_leaves(&tree, is_subagent);
        dock_below(&mut tree, leaves.last().copied(), "run-b");
        let leaves = subagent_leaves(&tree, is_subagent);
        let tabs: Vec<_> = leaves
            .iter()
            .map(|node| tree[*node].tabs().unwrap().to_vec())
            .collect();
        assert_eq!(tabs, [vec!["home", "other"], vec!["run-a"], vec!["run-b"]]);
        equalize_subagent_fractions(&mut tree, &leaves);
        let Node::Vertical(top) = &tree[column] else {
            panic!("the column was split");
        };
        assert!((top.fraction - 1.0 / 3.0).abs() < f32::EPSILON);
    }

    // Given: no subagent leaf
    // Then: the first subagent opens to the right of everything
    #[test]
    fn the_first_subagent_without_a_leaf_opens_on_the_right() {
        let mut tree = Tree::new(vec!["main"]);
        dock_below(&mut tree, None, "run-a");
        let Node::Horizontal(split) = &tree[NodeIndex::root()] else {
            panic!("split right");
        };
        assert!((split.fraction - 0.7).abs() < f32::EPSILON);
        assert_eq!(tree[NodeIndex::root().right()].tabs(), Some(&["run-a"][..]));
    }

    // Given: a leaf of home, other and main, with other in front
    // When: two parked tabs are spliced in after main
    // Then: they follow main in order, and home comes to the front
    #[test]
    fn parked_tabs_follow_the_anchor_and_bring_the_target_forward() {
        let mut dock = DockState::new(vec!["home", "main", "other"]);
        let path = dock.find_tab(&"other").unwrap();
        dock.set_active_tab(path).unwrap();
        splice_parked(&mut dock, &"home", vec!["p1", "p2"], |tab| *tab == "main");
        let path = dock.find_tab(&"home").unwrap();
        let leaf = dock.leaf(path.node_path()).unwrap();
        assert_eq!(leaf.tabs, ["home", "main", "p1", "p2", "other"]);
        assert_eq!(leaf.tabs[leaf.active.0], "home");

        let mut dock = DockState::new(vec!["other", "home"]);
        splice_parked(&mut dock, &"home", vec!["p1"], |tab| *tab == "main");
        let path = dock.find_tab(&"home").unwrap();
        let leaf = dock.leaf(path.node_path()).unwrap();
        assert_eq!(leaf.tabs, ["other", "home", "p1"]);
        assert_eq!(leaf.tabs[leaf.active.0], "home");
    }

    // Given: the focus on the main leaf, then on a leaf whose tab is parked away
    // When: a subagent is docked and the focus restored
    // Then: the main leaf keeps the focus; the parked tab's leaf hands it to no one
    #[test]
    fn docking_keeps_the_focus_and_a_parked_leaf_gives_it_up() {
        let mut dock = DockState::new(vec!["main"]);
        let main = dock.find_tab(&"main").unwrap().node_path();
        dock.set_focused_node_and_surface(main);
        let focus = focus_snapshot(&dock);
        dock_below(dock.main_surface_mut(), None, "run-a");
        restore_focus(&mut dock, focus, |_| false);
        let focused = dock.focused_leaf().unwrap();
        assert_eq!(dock.leaf(focused).unwrap().tabs, ["main"]);

        let run = dock.find_tab(&"run-a").unwrap().node_path();
        dock.set_focused_node_and_surface(run);
        let focus = focus_snapshot(&dock);
        restore_focus(&mut dock, focus, |tab| *tab == "run-a");
        assert!(
            dock.focused_leaf()
                .and_then(|path| dock.leaf(path).ok())
                .is_none()
        );
    }
}
