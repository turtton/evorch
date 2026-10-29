// verify/t4-build-test-v3.sh が検証時だけ crates/apply-patch/tests/ にコピーする隠しテスト。
use apply_patch::{ChangeCounts, ChangeKind, apply_patch};

#[test]
fn labels_are_stable_for_all_four_kinds() {
    let s = String::new;
    assert_eq!(ChangeKind::Added { content: s(), overwritten: None }.label(), "added");
    assert_eq!(ChangeKind::Updated { old: s(), new: s() }.label(), "updated");
    assert_eq!(ChangeKind::Moved { from: "a".into(), old: s(), new: s(), overwritten: None }.label(), "moved");
    assert_eq!(ChangeKind::Deleted { old: s() }.label(), "deleted");
}

#[test]
fn counts_total_and_default() {
    let counts = ChangeCounts { added: 2, updated: 3, moved: 0, deleted: 5 };
    assert_eq!(counts.total(), 10);
    assert_eq!(ChangeCounts::default(), ChangeCounts { added: 0, updated: 0, moved: 0, deleted: 0 });
    let copy = counts;
    assert_eq!(copy, counts);
    const TOTAL: usize = ChangeCounts { added: 1, updated: 1, moved: 1, deleted: 1 }.total();
    assert_eq!(TOTAL, 4);
}

#[test]
fn applied_counts_two_adds_one_delete_and_one_move() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("old.txt"), "gone\n").unwrap();
    std::fs::write(dir.path().join("src.txt"), "x\n").unwrap();
    let patch = "*** Begin Patch\n*** Add File: a.txt\n+a\n*** Add File: b.txt\n+b\n*** Delete File: old.txt\n*** Update File: src.txt\n*** Move to: dst.txt\n@@\n-x\n+y\n*** End Patch";
    let applied = apply_patch(patch, Some(dir.path())).unwrap();
    assert_eq!(applied.counts(), ChangeCounts { added: 2, updated: 0, moved: 1, deleted: 1 });
}
