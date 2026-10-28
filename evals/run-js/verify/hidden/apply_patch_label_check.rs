// verify/t6-debug-fix-v3.sh が検証時だけ crates/apply-patch/tests/ にコピーする隠しテスト（新機能 ChangeKind::label が残っていること）。
use apply_patch::ChangeKind;

#[test]
fn label_feature_is_kept() {
    let s = String::new;
    assert_eq!(ChangeKind::Added { content: s(), overwritten: None }.label(), "added");
    assert_eq!(ChangeKind::Updated { old: s(), new: s() }.label(), "updated");
    assert_eq!(ChangeKind::Moved { from: "a".into(), old: s(), new: s(), overwritten: None }.label(), "moved");
    assert_eq!(ChangeKind::Deleted { old: s() }.label(), "deleted");
}
