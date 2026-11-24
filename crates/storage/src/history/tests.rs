use super::*;

#[test]
fn initial_receipt_removes_only_the_first_prompt_and_preserves_dedupe_identity() {
    for initial in [false, true] {
        let mut fold = Reducer {
            initial_receipt: initial,
            ..Default::default()
        };
        let first = fold.message(Role::User, vec![("task".into(), false)], true, false);
        assert_eq!(first.is_some(), !initial);
        assert!(
            fold.message(Role::User, vec![("task".into(), false)], true, false)
                .is_none()
        );
        let changed = fold
            .message(Role::User, vec![("changed".into(), false)], true, false)
            .unwrap();
        assert_eq!(changed.index, u64::from(!initial));
        let reply = fold
            .message(
                Role::Assistant,
                vec![("answer".into(), false)],
                false,
                false,
            )
            .unwrap();
        assert_eq!(reply.index, 1 + u64::from(!initial));
        assert!(
            fold.message(Role::Assistant, vec![("answer".into(), false)], false, true)
                .is_none()
        );
    }
}

#[test]
fn normalization_joins_inline_parts_but_not_deferred_references() {
    let mut fold = Reducer::default();
    let message = fold
        .message(
            Role::Assistant,
            vec![("a".into(), false), ("b".into(), true)],
            false,
            false,
        )
        .unwrap();
    assert_eq!(message.parts, vec![("ab".into(), false)]);
    assert!(
        fold.message(Role::Assistant, vec![("ab".into(), false)], false, true)
            .is_none()
    );
    assert!(
        fold.message(Role::Assistant, vec![("".into(), false)], false, false)
            .is_none()
    );
}

#[test]
fn part_boundaries_and_restart_are_explicit() {
    assert_eq!(part_progress(None, 0, 0), None);
    assert_eq!(part_progress(None, 0, 4097), None);
    assert_eq!(part_progress(None, 0, 4096), Some((1, 4096)));
    assert_eq!(part_progress(None, 1, 2), None);
    assert_eq!(part_progress(Some((1, 2)), 1, 3), None);
    assert_eq!(part_progress(Some((1, 2)), 1, 2), Some((2, 2)));
    assert_eq!(part_progress(Some((1, 2)), 0, 1), Some((1, 1)));
}
