//! git_diff ツールの統合テスト。
//!
//! コミット不要の実リポジトリ（`git init` → `git add` → 作業ツリー変更）を使い、
//! インデックスと作業ツリーの差分取得、`path` による絞り込み、クリーンな木、
//! リポジトリ外のエラー経路を検証する。

use std::path::Path;
use std::sync::Arc;

use sandbox::DirectSandbox;
use tools::{GitDiff, Tool, ToolError};

fn git_diff() -> GitDiff {
    GitDiff::new(Arc::new(DirectSandbox::new_unchecked()))
}

/// 一時ディレクトリをカレントにして git サブコマンドを実行する。
///
/// テストフィクスチャ用のため、ユーザーの git 設定を読まないよう
/// `GIT_CONFIG_GLOBAL` / `GIT_CONFIG_SYSTEM` を無効化する。
fn run_git(dir: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} が失敗しました: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// コミットを含まない空の git リポジトリを作成する。
fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    run_git(dir.path(), &["init"]);
    dir
}

// Given: original をステージ済みで作業ツリーを modified に変更したリポジトリ / When: 引数 cwd のみで git_diff を実行 / Then: 削除行と追加行を含む差分が正常終了で返る
#[tokio::test]
async fn git_diff_returns_worktree_diff() {
    // Given: original をステージし、作業ツリーの内容を modified へ変更したリポジトリ
    let repo = init_repo();
    let file = repo.path().join("sample.txt");
    std::fs::write(&file, "original\n").unwrap();
    run_git(repo.path(), &["add", "sample.txt"]);
    std::fs::write(&file, "modified\n").unwrap();

    // When: cwd だけを指定して git_diff を実行
    let result = git_diff()
        .execute(serde_json::json!({ "cwd": repo.path().to_string_lossy() }))
        .await
        .unwrap();

    // Then: 正常終了であり、差分に元の行と変更後の行が両方含まれる
    assert!(!result.is_error);
    assert!(
        result.content.contains("-original"),
        "削除行が含まれない: {}",
        result.content
    );
    assert!(
        result.content.contains("+modified"),
        "追加行が含まれない: {}",
        result.content
    );
}

// Given: 2 ファイルをステージして両方変更したリポジトリ / When: path に片方のファイル名を指定して git_diff を実行 / Then: 指定ファイルの差分のみが返る
#[tokio::test]
async fn git_diff_scopes_to_path_argument() {
    // Given: alpha.txt と beta.txt をステージし、両方の作業ツリーを変更したリポジトリ
    let repo = init_repo();
    let alpha = repo.path().join("alpha.txt");
    let beta = repo.path().join("beta.txt");
    std::fs::write(&alpha, "alpha original\n").unwrap();
    std::fs::write(&beta, "beta original\n").unwrap();
    run_git(repo.path(), &["add", "alpha.txt", "beta.txt"]);
    std::fs::write(&alpha, "alpha modified\n").unwrap();
    std::fs::write(&beta, "beta modified\n").unwrap();

    // When: path に alpha.txt を指定して git_diff を実行
    let result = git_diff()
        .execute(serde_json::json!({
            "cwd": repo.path().to_string_lossy(),
            "path": "alpha.txt",
        }))
        .await
        .unwrap();

    // Then: 正常終了であり、alpha.txt の差分のみが含まれ beta の内容は含まれない
    assert!(!result.is_error);
    assert!(
        result.content.contains("alpha modified"),
        "指定ファイルの差分が含まれない: {}",
        result.content
    );
    assert!(
        !result.content.contains("beta"),
        "指定外ファイルの内容が含まれる: {}",
        result.content
    );
}

// Given: 何もステージも変更もしていないリポジトリ / When: git_diff を実行 / Then: 空本文で正常終了する
#[tokio::test]
async fn git_diff_clean_tree_is_empty_success() {
    // Given: git init 直後で何もないリポジトリ
    let repo = init_repo();

    // When: cwd を指定して git_diff を実行
    let result = git_diff()
        .execute(serde_json::json!({ "cwd": repo.path().to_string_lossy() }))
        .await
        .unwrap();

    // Then: 正常終了であり、本文は空文字列である
    assert!(!result.is_error);
    assert_eq!(result.content, "");
}

// Given: git リポジトリではない一時ディレクトリ / When: git_diff を実行 / Then: NotAGitRepository エラーで失敗する
#[tokio::test]
async fn git_diff_outside_repo_is_not_a_git_repository() {
    // Given: git init していない一時ディレクトリ
    let dir = tempfile::tempdir().unwrap();

    // When: cwd にそのディレクトリを指定して git_diff を実行
    let error = git_diff()
        .execute(serde_json::json!({ "cwd": dir.path().to_string_lossy() }))
        .await
        .unwrap_err();

    // Then: NotAGitRepository が cwd 付きで返る
    assert_eq!(
        error,
        ToolError::NotAGitRepository {
            path: dir.path().to_string_lossy().into_owned()
        }
    );
}
