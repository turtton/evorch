use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use gui::diff::{
    DIFF_BYTE_CAP, DiffError, DiffMode, DiffModel, DiffRequest, DiffSource, DiffState,
    FixtureDiffSource, GitCliDiffSource,
};
use tempfile::TempDir;

fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git を起動できる");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn commit(repo: &Path, message: &str) {
    git(repo, &["add", "."]);
    git(
        repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-m",
            message,
        ],
    );
}

fn initialized_repo() -> TempDir {
    let repo = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    git(repo.path(), &["init", "-b", "main"]);
    fs::write(repo.path().join("tracked.txt"), "base\n").expect("fixture を書き込める");
    commit(repo.path(), "base");
    repo
}

fn request(repo: &Path, mode: DiffMode) -> DiffRequest {
    DiffRequest {
        repo_root: repo.to_path_buf(),
        mode,
    }
}

fn poll_until_settled(model: &mut DiffModel, mode: &DiffMode) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while matches!(model.state(mode), DiffState::Loading) && Instant::now() < deadline {
        model.poll();
        thread::yield_now();
    }
}

// Given: 変更のない初期化済み Git リポジトリ
// When: working-tree diff を取得する
// Then: 空文字列が返る
#[test]
fn working_tree_diff_of_clean_repo_is_empty() {
    let repo = initialized_repo();
    let source = GitCliDiffSource;

    let text = source
        .fetch(&request(repo.path(), DiffMode::WorkingTree))
        .expect("clean repo の diff を取得できる");

    assert!(text.trim().is_empty());
}

// Given: tracked file が変更された Git リポジトリ
// When: working-tree diff を取得する
// Then: 変更内容を含む unified diff が返る
#[test]
fn working_tree_diff_shows_modified_file() {
    let repo = initialized_repo();
    fs::write(repo.path().join("tracked.txt"), "modified\n").expect("fixture を変更できる");
    let source = GitCliDiffSource;

    let text = source
        .fetch(&request(repo.path(), DiffMode::WorkingTree))
        .expect("working-tree diff を取得できる");

    assert!(text.contains("tracked.txt"));
    assert!(text.contains("+modified"));
}

// Given: main と分岐後に双方へ独立した commit があるリポジトリ
// When: task branch から main...HEAD の diff を取得する
// Then: merge base 以降の branch 側変更だけが返る
//
// 任意 base は DiffMode::Branch が unit variant となった時点で型レベルで
// 表現不可能であり、それを試す runtime test は存在し得ない (issue #65 AC11)。
#[test]
fn branch_diff_against_main_uses_merge_base() {
    let repo = initialized_repo();
    git(repo.path(), &["switch", "-c", "evorch/task/run-1"]);
    fs::write(repo.path().join("branch.txt"), "branch\n").expect("branch fixture を書ける");
    commit(repo.path(), "branch change");
    git(repo.path(), &["switch", "main"]);
    fs::write(repo.path().join("unrelated.txt"), "main\n").expect("main fixture を書ける");
    commit(repo.path(), "unrelated main change");
    git(repo.path(), &["switch", "evorch/task/run-1"]);
    let source = GitCliDiffSource;

    let text = source
        .fetch(&request(repo.path(), DiffMode::Branch))
        .expect("branch diff を取得できる");

    assert!(text.contains("branch.txt"));
    assert!(!text.contains("unrelated.txt"));
}

// Given: byte cap を超える multibyte working-tree diff を持つ Git リポジトリ
// When: model が worker result を poll する
// Then: UTF-8 境界を保った Truncated state になる
#[test]
fn oversized_diff_is_truncated_at_cap() {
    let repo = initialized_repo();
    fs::write(repo.path().join("tracked.txt"), "あ".repeat(DIFF_BYTE_CAP))
        .expect("oversized fixture を書き込める");
    let mode = DiffMode::WorkingTree;
    let mut model = DiffModel::new();

    model.request(
        Arc::new(GitCliDiffSource),
        request(repo.path(), mode.clone()),
    );
    poll_until_settled(&mut model, &mode);

    match model.state(&mode) {
        DiffState::Truncated {
            text,
            total_bytes,
            cap,
        } => {
            assert!(*total_bytes > DIFF_BYTE_CAP);
            assert_eq!(*cap, DIFF_BYTE_CAP);
            assert!(text.len() <= DIFF_BYTE_CAP);
            assert!(text.is_char_boundary(text.len()));
        }
        state => panic!("expected Truncated, got {state:?}"),
    }
}

// Given: Git リポジトリではないディレクトリ
// When: model 経由で working-tree diff を取得する
// Then: panic せず stderr を含む Error state になる
#[test]
fn git_error_is_explicit_state_not_panic() {
    let directory = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let mode = DiffMode::WorkingTree;
    let mut model = DiffModel::new();

    model.request(
        Arc::new(GitCliDiffSource),
        request(directory.path(), mode.clone()),
    );
    poll_until_settled(&mut model, &mode);

    match model.state(&mode) {
        DiffState::Error { message } => assert!(message.to_lowercase().contains("git")),
        state => panic!("expected Error, got {state:?}"),
    }
}

// Given: mode ごとに canned result を持つ fixture source
// When: working-tree と branch の result を取得する
// Then: 対応する canned state が返る
#[test]
fn fixture_source_returns_canned_states() {
    let source = FixtureDiffSource::new(
        Ok("working".to_string()),
        Err(DiffError::Io("branch unavailable".to_string())),
    );

    let working = source.fetch(&request(Path::new("."), DiffMode::WorkingTree));
    let branch = source.fetch(&request(Path::new("."), DiffMode::Branch));

    assert_eq!(working.expect("working fixture が成功する"), "working");
    assert!(matches!(branch, Err(DiffError::Io(message)) if message == "branch unavailable"));
}

// Given: ready text を返す fixture source
// When: request 直後と bounded loop 内で model を観測する
// Then: request は即座に Loading を返し、poll により Ready へ遷移する
#[test]
fn model_poll_transitions_loading_to_ready_without_blocking() {
    let source = Arc::new(FixtureDiffSource::new(
        Ok("diff text".to_string()),
        Ok(String::new()),
    ));
    let mode = DiffMode::WorkingTree;
    let mut model = DiffModel::new();

    model.request(source, request(Path::new("."), mode.clone()));

    assert!(matches!(model.state(&mode), DiffState::Loading));
    poll_until_settled(&mut model, &mode);
    assert!(matches!(model.state(&mode), DiffState::Ready { text } if text == "diff text"));
}
