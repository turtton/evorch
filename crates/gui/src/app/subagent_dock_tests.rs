use super::*;

#[test]
fn equalize_sets_chain_fractions_for_n_panes() {
    for count in 1_u16..=6 {
        // Given: a sidebar and an unequal right-hand subagent chain.
        let mut tree = egui_dock::Tree::new(vec![PanelId::new("sidebar")]);
        let [_, first] = tree.split_right(NodeIndex::root(), 0.7, vec![PanelId::new("0")]);
        let mut leaves = vec![first];
        for index in 1..count {
            let bottom = leaves.pop().expect("last pane");
            leaves.extend(tree.split_below(bottom, 0.5, vec![PanelId::new(index.to_string())]));
        }
        // When: insertion equalizes the subagent chain.
        equalize_subagent_fractions(&mut tree, &leaves);
        // Then: fractions encode equal shares without changing sidebar width.
        let Node::Horizontal(sidebar) = &tree[NodeIndex::root()] else {
            panic!("sidebar split");
        };
        assert_eq!(sidebar.fraction, 0.7);
        let mut node = first;
        for remaining in (2..=count).rev() {
            let Node::Vertical(split) = &tree[node] else {
                panic!("subagent split");
            };
            assert!((split.fraction - 1.0 / f32::from(remaining)).abs() < f32::EPSILON);
            node = node.right();
        }
        assert_eq!(Some(&node), leaves.last());
    }
}
