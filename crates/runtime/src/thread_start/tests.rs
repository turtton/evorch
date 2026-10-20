use std::path::{Path, PathBuf};
use std::process::Command;

use super::*;

const AUTHOR: [&str; 6] = [
    "-c",
    "user.name=evorch-test",
    "-c",
    "user.email=test@evorch.invalid",
    "-c",
    "commit.gpgsign=false",
];

fn git_command(dir: &Path) -> Command {
    let mut command = Command::new("git");
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("GIT_") {
            command.env_remove(name);
        }
    }
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(AUTHOR)
        .current_dir(dir);
    command
}

pub(crate) fn git(dir: &Path, arguments: &[&str]) -> Vec<u8> {
    let output = git_command(dir).args(arguments).output().unwrap();
    assert!(
        output.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn git_with_input(dir: &Path, arguments: &[&str], input: &[u8]) {
    use std::io::Write;
    let mut child = git_command(dir)
        .args(arguments)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    assert!(child.wait().unwrap().success(), "git {arguments:?}");
}

struct Repo {
    dir: tempfile::TempDir,
    global_config: PathBuf,
}

impl Repo {
    /// A repository with one commit, and a global config whose excludes file hides `*.secret-cache`.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        std::fs::write(root.join("tracked.txt"), "one\n").unwrap();
        std::fs::write(root.join("removed.txt"), "bye\n").unwrap();
        std::fs::write(root.join("image.bin"), [0_u8, 1, 2, 255]).unwrap();
        std::fs::write(root.join(".gitignore"), "ignored.log\n").unwrap();
        std::fs::create_dir_all(root.join("sub/dir")).unwrap();
        std::fs::write(root.join("sub/dir/inner.txt"), "inner\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--quiet", "-m", "initial"]);
        let excludes = dir.path().join("global-excludes");
        std::fs::write(&excludes, "*.secret-cache\n").unwrap();
        let global_config = dir.path().join("global-config");
        std::fs::write(
            &global_config,
            format!("[core]\n\texcludesFile = {}\n", excludes.display()),
        )
        .unwrap();
        Self { dir, global_config }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().join("repo")
    }

    fn capture(&self, project_root: &Path) -> ThreadStartCapture {
        capture_with(
            project_root,
            &Git {
                deadline: Instant::now() + CAPTURE_DEADLINE,
                global_config: Some(&self.global_config),
            },
        )
    }

    fn snapshot(&self) -> ThreadStartSnapshot {
        match self.capture(&self.root()) {
            ThreadStartCapture::Replayable(snapshot) => snapshot,
            other => panic!("expected a replayable capture, got {other:?}"),
        }
    }

    fn head(&self) -> String {
        String::from_utf8(git(&self.root(), &["rev-parse", "HEAD"]))
            .unwrap()
            .trim()
            .to_owned()
    }
}

fn untracked_paths(snapshot: &ThreadStartSnapshot) -> Vec<&str> {
    let mut paths: Vec<_> = snapshot
        .untracked
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    paths.sort_unstable();
    paths
}

#[test]
fn clean_repository_records_only_head() {
    // Given: a repository without changes.
    let repo = Repo::new();

    // When: its start state is captured.
    let snapshot = repo.snapshot();

    // Then: only the commit is recorded.
    assert_eq!(snapshot.head_commit, repo.head());
    assert_eq!(snapshot.head_ref.as_deref(), Some("refs/heads/main"));
    assert_eq!(snapshot.repo_prefix, "");
    assert!(snapshot.diff.is_empty());
    assert!(snapshot.untracked.is_empty());
}

#[test]
fn checked_out_branch_is_recorded_and_detached_head_records_none() {
    // Given: a repository on a feature branch.
    let repo = Repo::new();
    git(&repo.root(), &["checkout", "--quiet", "-b", "feature/x"]);

    // When: the start state is captured on the branch and again with HEAD detached.
    let on_branch = repo.snapshot();
    git(&repo.root(), &["checkout", "--quiet", "--detach"]);
    let detached = repo.snapshot();

    // Then: the branch is named only while HEAD points at it.
    assert_eq!(on_branch.head_ref.as_deref(), Some("refs/heads/feature/x"));
    assert_eq!(detached.head_ref, None);
    assert_eq!(detached.head_commit, on_branch.head_commit);
}

#[test]
fn diff_rebuilds_modified_staged_deleted_and_binary_changes() {
    // Given: an unstaged edit, a staged new file, a deletion and a binary change.
    let repo = Repo::new();
    let root = repo.root();
    std::fs::write(root.join("tracked.txt"), "one\ntwo\n").unwrap();
    std::fs::write(root.join("staged.txt"), "staged\n").unwrap();
    git(&root, &["add", "staged.txt"]);
    std::fs::remove_file(root.join("removed.txt")).unwrap();
    std::fs::write(root.join("image.bin"), [9_u8, 0, 8, 0, 7]).unwrap();

    // When: the start state is captured and its diff applied to a clean checkout of HEAD.
    let snapshot = repo.snapshot();
    let replay = repo.dir.path().join("replay");
    git(
        repo.dir.path(),
        &["clone", "--quiet", root.to_str().unwrap(), "replay"],
    );
    git_with_input(&replay, &["apply", "--binary", "-"], &snapshot.diff);

    // Then: the checkout matches the captured working tree.
    assert!(String::from_utf8_lossy(&snapshot.diff).contains("GIT binary patch"));
    assert_eq!(
        std::fs::read_to_string(replay.join("tracked.txt")).unwrap(),
        "one\ntwo\n"
    );
    assert_eq!(
        std::fs::read_to_string(replay.join("staged.txt")).unwrap(),
        "staged\n"
    );
    assert!(!replay.join("removed.txt").exists());
    assert_eq!(
        std::fs::read(replay.join("image.bin")).unwrap(),
        [9, 0, 8, 0, 7]
    );
}

#[cfg(unix)]
#[test]
fn untracked_files_keep_bytes_mode_and_symlinks_but_not_ignored_files() {
    use std::os::unix::fs::PermissionsExt;
    // Given: untracked plain, executable, binary and symlink entries, plus files
    // ignored by .gitignore and by the global excludes file.
    let repo = Repo::new();
    let root = repo.root();
    std::fs::write(root.join("notes.txt"), "draft\n").unwrap();
    std::fs::write(root.join("run.sh"), "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(root.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(root.join("sub/blob"), [0_u8, 200, 13]).unwrap();
    std::os::unix::fs::symlink("tracked.txt", root.join("link")).unwrap();
    std::fs::write(root.join("ignored.log"), "noise").unwrap();
    std::fs::write(root.join("token.secret-cache"), "noise").unwrap();

    // When: the start state is captured.
    let snapshot = repo.snapshot();

    // Then: every untracked, non-ignored entry is recorded as it is on disk.
    let entry = |path: &str| {
        snapshot
            .untracked
            .iter()
            .find(|entry| entry.path == path)
            .unwrap()
            .clone()
    };
    assert_eq!(
        untracked_paths(&snapshot),
        ["link", "notes.txt", "run.sh", "sub/blob"]
    );
    assert_eq!(
        entry("notes.txt"),
        UntrackedEntry {
            path: "notes.txt".into(),
            kind: UntrackedKind::File,
            executable: false,
            content: b"draft\n".to_vec(),
        }
    );
    assert!(entry("run.sh").executable);
    assert_eq!(entry("sub/blob").content, [0, 200, 13]);
    assert_eq!(
        (entry("link").kind, entry("link").content),
        (UntrackedKind::Symlink, b"tracked.txt".to_vec())
    );
}

#[test]
fn subdirectory_project_records_its_prefix_and_the_whole_repository() {
    // Given: a project rooted in a subdirectory, with changes inside and outside it.
    let repo = Repo::new();
    let root = repo.root();
    std::fs::write(root.join("tracked.txt"), "outside\n").unwrap();
    std::fs::write(root.join("sub/dir/inner.txt"), "inside\n").unwrap();
    std::fs::write(root.join("top-level.txt"), "new").unwrap();
    std::fs::write(root.join("sub/dir/local.txt"), "new").unwrap();

    // When: the start state is captured from the subdirectory.
    let ThreadStartCapture::Replayable(snapshot) = repo.capture(&root.join("sub/dir")) else {
        panic!("expected a replayable capture");
    };

    // Then: the prefix names the subdirectory and every change in the repository is kept.
    assert_eq!(snapshot.repo_prefix, "sub/dir/");
    let diff = String::from_utf8_lossy(&snapshot.diff);
    assert!(diff.contains("a/tracked.txt") && diff.contains("a/sub/dir/inner.txt"));
    assert_eq!(
        untracked_paths(&snapshot),
        ["sub/dir/local.txt", "top-level.txt"]
    );
}

#[test]
fn nested_repositories_are_skipped() {
    // Given: an untracked directory that is a repository of its own.
    let repo = Repo::new();
    let nested = repo.root().join("vendor/lib");
    std::fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init", "--quiet"]);
    std::fs::write(nested.join("file.txt"), "nested").unwrap();

    // When: the start state is captured.
    let snapshot = repo.snapshot();

    // Then: nothing of the nested repository is recorded.
    assert!(snapshot.untracked.is_empty());
}

#[test]
fn directory_outside_git_is_not_replayable() {
    // Given: a directory that no repository contains.
    let dir = tempfile::tempdir().unwrap();

    // When: its start state is captured.
    let capture = capture(dir.path(), Instant::now() + CAPTURE_DEADLINE);

    // Then: it is recorded as not a git repository.
    assert_eq!(
        capture,
        ThreadStartCapture::NotReplayable(NotReplayableReason::NotGitRepository)
    );
}

#[test]
fn unborn_head_is_not_replayable() {
    // Given: a repository without any commit.
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "--quiet"]);
    std::fs::write(dir.path().join("file.txt"), "content").unwrap();

    // When: its start state is captured.
    let capture = capture(dir.path(), Instant::now() + CAPTURE_DEADLINE);

    // Then: it is recorded as having no commit.
    assert_eq!(
        capture,
        ThreadStartCapture::NotReplayable(NotReplayableReason::NoCommit)
    );
}

#[test]
fn content_over_the_limit_is_not_replayable() {
    // Given: a diff and an untracked file that together exceed the limit.
    let repo = Repo::new();
    let root = repo.root();
    std::fs::write(root.join("tracked.txt"), "x".repeat(1024)).unwrap();
    std::fs::write(root.join("huge.txt"), vec![b'a'; MAX_THREAD_START_BYTES]).unwrap();

    // When: its start state is captured.
    let capture = repo.capture(&root);

    // Then: it is recorded as too large, without content.
    assert_eq!(
        capture,
        ThreadStartCapture::NotReplayable(NotReplayableReason::TooLarge)
    );
}

#[test]
fn expired_deadline_is_not_replayable() {
    // Given: a repository and a deadline that has already passed.
    let repo = Repo::new();

    // When: its start state is captured.
    let capture = capture_with(
        &repo.root(),
        &Git {
            deadline: Instant::now(),
            global_config: Some(&repo.global_config),
        },
    );

    // Then: it is recorded as timed out.
    assert_eq!(
        capture,
        ThreadStartCapture::NotReplayable(NotReplayableReason::TimedOut)
    );
}

#[test]
fn capture_never_rewrites_the_index() {
    // Given: an unchanged tracked file whose stat data is stale, so a refreshing
    // `git diff` would rewrite the index.
    let repo = Repo::new();
    let root = repo.root();
    std::fs::File::options()
        .write(true)
        .open(root.join("tracked.txt"))
        .unwrap()
        .set_modified(std::time::SystemTime::now() + Duration::from_secs(60))
        .unwrap();
    let index = root.join(".git/index");
    let before = (
        std::fs::read(&index).unwrap(),
        std::fs::metadata(&index).unwrap().modified().unwrap(),
    );

    // When: its start state is captured.
    let _ = repo.snapshot();

    // Then: the index is byte-for-byte unchanged, with the same mtime, and no lock is left.
    let after = (
        std::fs::read(&index).unwrap(),
        std::fs::metadata(&index).unwrap().modified().unwrap(),
    );
    assert_eq!(before, after);
    assert!(!root.join(".git/index.lock").exists());
}
