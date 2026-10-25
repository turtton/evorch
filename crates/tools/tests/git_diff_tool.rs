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
/// `GIT_*` を取り除き、`GIT_CONFIG_GLOBAL` / `GIT_CONFIG_SYSTEM` を無効化する。
fn run_git(dir: &Path, args: &[&str]) {
    let mut command = std::process::Command::new("git");
    // Hooks export repository-local Git variables; fixtures must never use that repository.
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("GIT_") {
            command.env_remove(name);
        }
    }
    let output = command
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

#[test]
fn git_fixture_ignores_inherited_repository_env() {
    const CHILD_MARKER: &str = "EVORCH_GIT_FIXTURE_ISOLATION_CHILD";
    if std::env::var_os(CHILD_MARKER).is_some() {
        let fixture = init_repo();
        assert!(fixture.path().join(".git").is_dir());
        run_git(fixture.path(), &["config", "user.name", "Fixture Identity"]);
        return;
    }

    let host = init_repo();
    run_git(host.path(), &["config", "user.name", "Host Identity"]);
    let config = host.path().join(".git/config");
    let original = std::fs::read(&config).unwrap();
    // A child process keeps the hook-like environment out of parallel tests.
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "git_fixture_ignores_inherited_repository_env"])
        .env(CHILD_MARKER, "1")
        .env("GIT_DIR", host.path().join(".git"))
        .env("GIT_COMMON_DIR", host.path().join(".git"))
        .env("GIT_WORK_TREE", host.path())
        .env("GIT_INDEX_FILE", host.path().join(".git/index"))
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "user.name")
        .env("GIT_CONFIG_VALUE_0", "Inherited Identity")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "fixture isolation failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(config).unwrap(), original);
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

/// External diff programs that report how git started them (needs `/proc`).
#[cfg(target_os = "linux")]
mod external_diff {
    use std::path::{Path, PathBuf};

    use tools::Tool;

    use super::{git_diff, init_repo, run_git};

    /// Wait until `path` holds a complete line, written by the external diff fixture.
    async fn wait_for_line(path: &Path) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Ok(text) = std::fs::read_to_string(path)
                && text.ends_with('\n')
            {
                return text;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{} was not written",
                path.display()
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// Wait until `pid` no longer runs; a zombie waiting for its reaper counts as stopped.
    async fn assert_stops(pid: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let running = std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
                stat.rsplit_once(')')
                    .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
            });
            if !running {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "process {pid} still runs after git_diff ended"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// A repository with one modified file whose diff runs `body` as the
    /// external diff program. The program writes into the returned directory.
    fn repository_with_external_diff(
        body: impl FnOnce(&Path) -> String,
    ) -> (tempfile::TempDir, tempfile::TempDir) {
        use std::os::unix::fs::PermissionsExt;
        let repo = init_repo();
        let file = repo.path().join("sample.txt");
        std::fs::write(&file, "original\n").unwrap();
        run_git(repo.path(), &["add", "sample.txt"]);
        std::fs::write(&file, "modified\n").unwrap();
        let probe = tempfile::tempdir().unwrap();
        // The Nix build sandbox has no /bin/sh.
        let sh: PathBuf = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|dir| dir.join("sh"))
            .find(|sh| sh.is_file())
            .expect("sh on PATH");
        let script = probe.path().join("external-diff");
        std::fs::write(
            &script,
            format!("#!{}\n{}", sh.display(), body(probe.path())),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        run_git(
            repo.path(),
            &["config", "diff.external", script.to_str().unwrap()],
        );
        (repo, probe)
    }

    // Given: evorch holds a descriptor without O_CLOEXEC, and the repository runs
    // an external diff program that reports how it was started and then waits on
    // a background helper / When: git_diff runs and is cancelled / Then: git and
    // its diff program ran in a session of their own without that descriptor, and
    // cancelling stops the helper too, not only git.
    #[tokio::test]
    async fn git_diff_runs_detached_without_evorch_descriptors_and_stops_its_helpers() {
        use std::os::fd::AsRawFd;
        let held = tempfile::NamedTempFile::new().unwrap();
        let fd = rustix::io::fcntl_dupfd_cloexec(&held, 100).unwrap();
        rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::empty()).unwrap();
        let name = held
            .path()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let (repo, probe) = repository_with_external_diff(|out| {
            let out = out.display();
            format!(
                "cut -d' ' -f6 /proc/$$/stat > '{out}/session'\n\
                 {{ readlink /proc/$$/fd/{fd} || echo descriptor-closed; }} > '{out}/descriptor' 2>/dev/null\n\
                 sleep 60 &\n\
                 echo $! > '{out}/helper.tmp' && mv '{out}/helper.tmp' '{out}/helper.pid'\n\
                 wait\n",
                fd = fd.as_raw_fd(),
            )
        });

        let cwd = repo.path().to_string_lossy().into_owned();
        let call =
            tokio::spawn(
                async move { git_diff().execute(serde_json::json!({ "cwd": cwd })).await },
            );
        let helper = wait_for_line(&probe.path().join("helper.pid")).await;
        call.abort();
        assert!(call.await.unwrap_err().is_cancelled());

        let session = wait_for_line(&probe.path().join("session")).await;
        let own_session = rustix::process::getsid(None).unwrap().to_string();
        assert!(session.trim().parse::<u32>().is_ok(), "{session}");
        assert_ne!(
            session.trim(),
            own_session,
            "git diff shares evorch's session"
        );
        let descriptor = wait_for_line(&probe.path().join("descriptor")).await;
        assert_eq!(descriptor, "descriptor-closed\n", "{name} leaked");
        assert_stops(helper.trim()).await;
    }

    // Given: an external diff program that leaves a helper holding git's
    // output and exits / When: git_diff runs / Then: it returns once git exits
    // instead of waiting for the helper, and the helper stops.
    #[tokio::test]
    async fn git_diff_returns_when_git_exits_even_if_a_helper_holds_its_output() {
        let (repo, probe) = repository_with_external_diff(|out| {
            let out = out.display();
            format!(
                "sleep 60 &\n\
                 echo $! > '{out}/helper.tmp' && mv '{out}/helper.tmp' '{out}/helper.pid'\n\
                 printf 'external diff ran\\n'\n"
            )
        });

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            git_diff().execute(serde_json::json!({ "cwd": repo.path().to_string_lossy() })),
        )
        .await
        .expect("git_diff must not wait for the helper's pipe")
        .unwrap();

        assert!(!result.is_error);
        assert!(
            result.content.contains("external diff ran"),
            "{}",
            result.content
        );
        let helper = wait_for_line(&probe.path().join("helper.pid")).await;
        assert_stops(helper.trim()).await;
    }
}
