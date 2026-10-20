use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use storage::thread_start::{ThreadStartSnapshot, UntrackedEntry, UntrackedKind};

use super::*;

pub(crate) fn git_command(dir: &Path) -> Command {
    let mut command = Command::new("git");
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("GIT_") {
            command.env_remove(name);
        }
    }
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "user.name=evorch-test",
            "-c",
            "user.email=test@evorch.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.excludesFile=/dev/null",
        ])
        .current_dir(dir);
    command
}

pub(crate) fn git(dir: &Path, arguments: &[&str]) -> String {
    let output = git_command(dir).args(arguments).output().unwrap();
    assert!(
        output.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn identity() -> CommitIdentity {
    CommitIdentity {
        name: "evorch-test".into(),
        email: "test@evorch.invalid".into(),
    }
}

/// Refs, HEAD, index bytes and every working-tree file, for detecting writes.
#[derive(Debug, PartialEq, Eq)]
struct Fingerprint {
    refs: String,
    head: String,
    index: Vec<u8>,
    files: Vec<(PathBuf, Vec<u8>)>,
}

/// A committed repository with branches `main` and `other`, checked out on `main`.
pub(crate) struct Source {
    pub dir: tempfile::TempDir,
}

impl Source {
    pub(crate) fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("source");
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        std::fs::write(root.join("tracked.txt"), "one\n").unwrap();
        std::fs::write(root.join("removed.txt"), "bye\n").unwrap();
        std::fs::write(root.join("image.bin"), [0_u8, 1, 2, 255]).unwrap();
        std::fs::write(root.join(".gitignore"), "ignored.log\n").unwrap();
        std::fs::create_dir_all(root.join("app/src")).unwrap();
        std::fs::write(root.join("app/src/lib.rs"), "fn main() {}\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--quiet", "-m", "initial"]);
        git(&root, &["branch", "other"]);
        Self { dir }
    }

    pub(crate) fn root(&self) -> PathBuf {
        self.dir.path().join("source")
    }

    pub(crate) fn head(&self) -> String {
        git(&self.root(), &["rev-parse", "HEAD"])
    }

    /// Edits, a staged addition, a deletion, a binary change and untracked entries.
    pub(crate) fn dirty(&self) {
        let root = self.root();
        std::fs::write(root.join("tracked.txt"), "one\ntwo\n").unwrap();
        std::fs::write(root.join("staged.txt"), "staged\n").unwrap();
        git(&root, &["add", "staged.txt"]);
        std::fs::remove_file(root.join("removed.txt")).unwrap();
        std::fs::write(root.join("image.bin"), [9_u8, 0, 8]).unwrap();
        std::fs::write(root.join("notes.md"), "draft\n").unwrap();
        std::fs::write(root.join("app/run.sh"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(
            root.join("app/run.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::fs::create_dir_all(root.join("new/deep")).unwrap();
        std::fs::write(root.join("new/deep/data.bin"), [7_u8, 0, 200]).unwrap();
        std::os::unix::fs::symlink("tracked.txt", root.join("link")).unwrap();
        std::fs::write(root.join("ignored.log"), "noise").unwrap();
    }

    /// The start state as the runtime records it: `git diff --binary HEAD` and untracked files.
    pub(crate) fn snapshot(&self, prefix: &str) -> ThreadStartSnapshot {
        let root = self.root();
        let diff = git_command(&root)
            .args([
                "diff",
                "--binary",
                "--no-color",
                "--no-ext-diff",
                "--no-relative",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                "HEAD",
                "--",
            ])
            .output()
            .unwrap()
            .stdout;
        let untracked = git(
            &root,
            &["ls-files", "--others", "--exclude-standard", "--full-name"],
        )
        .lines()
        .filter(|path| !path.is_empty())
        .map(|path| {
            let full = root.join(path);
            let metadata = std::fs::symlink_metadata(&full).unwrap();
            if metadata.file_type().is_symlink() {
                UntrackedEntry {
                    path: path.into(),
                    kind: UntrackedKind::Symlink,
                    executable: false,
                    content: std::fs::read_link(&full)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned()
                        .into_bytes(),
                }
            } else {
                UntrackedEntry {
                    path: path.into(),
                    kind: UntrackedKind::File,
                    executable: metadata.permissions().mode() & 0o100 != 0,
                    content: std::fs::read(&full).unwrap(),
                }
            }
        })
        .collect();
        let head_ref = git_command(&root)
            .args(["symbolic-ref", "-q", "HEAD"])
            .output()
            .unwrap();
        ThreadStartSnapshot {
            repo_prefix: prefix.into(),
            head_commit: self.head(),
            head_ref: head_ref.status.success().then(|| {
                String::from_utf8(head_ref.stdout)
                    .unwrap()
                    .trim()
                    .to_owned()
            }),
            diff,
            untracked,
        }
    }

    /// The tree of the source working tree, built in a scratch index.
    pub(crate) fn worktree_tree(&self) -> String {
        let root = self.root();
        let index = self.dir.path().join("source.index");
        std::fs::copy(root.join(".git/index"), &index).unwrap();
        let run = |arguments: &[&str]| {
            let output = git_command(&root)
                .env("GIT_INDEX_FILE", &index)
                .args(arguments)
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        run(&["add", "--all", "--", ":/"]);
        run(&["write-tree"])
    }

    fn workspace(&self) -> PathBuf {
        self.dir.path().join("run/workspace")
    }

    fn prepare(&self, snapshot: &ThreadStartSnapshot) -> Result<Workspace, PrepareError> {
        prepare(&self.root(), snapshot, &self.workspace(), &identity())
    }

    fn fingerprint(&self) -> Fingerprint {
        let root = self.root();
        let mut files = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.file_name() == Some(OsStr::new(".git")) {
                    continue;
                }
                let metadata = std::fs::symlink_metadata(&path).unwrap();
                if metadata.is_dir() {
                    stack.push(path);
                } else if metadata.is_file() {
                    files.push((path.clone(), std::fs::read(&path).unwrap()));
                }
            }
        }
        files.sort();
        Fingerprint {
            refs: git(
                &root,
                &["for-each-ref", "--format=%(refname) %(objectname)"],
            ),
            head: std::fs::read_to_string(root.join(".git/HEAD")).unwrap(),
            index: std::fs::read(root.join(".git/index")).unwrap(),
            files,
        }
    }
}

#[test]
fn prepared_start_reproduces_head_diff_and_untracked_entries_exactly() {
    // Given: a source working tree with every kind of uncommitted change.
    let source = Source::new();
    source.dirty();
    let snapshot = source.snapshot("");

    // When: the workspace is prepared from the recorded start.
    let workspace = source.prepare(&snapshot).unwrap();

    // Then: its start tree equals the source working tree, modes and links included.
    assert_eq!(workspace.start_tree, source.worktree_tree());
    let root = &workspace.toplevel;
    assert_eq!(
        std::fs::read_to_string(root.join("tracked.txt")).unwrap(),
        "one\ntwo\n"
    );
    assert!(!root.join("removed.txt").exists());
    assert!(!root.join("ignored.log").exists());
    let mode = std::fs::metadata(root.join("app/run.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o111, 0o111);
    assert_eq!(
        std::fs::read_link(root.join("link")).unwrap(),
        PathBuf::from("tracked.txt")
    );
    assert_eq!(
        git(root, &["rev-parse", "refs/evorch/eval-start"]),
        workspace.start_tree
    );
    assert_eq!(workspace.project_dir, workspace.toplevel);
}

#[test]
fn recorded_branch_is_checked_out_at_the_head_with_its_origin_upstream() {
    // Given: a start on `main`, after which the source's `main` moved on.
    let source = Source::new();
    let snapshot = source.snapshot("app/");
    let other = git(&source.root(), &["rev-parse", "other"]);
    std::fs::write(source.root().join("later.txt"), "later\n").unwrap();
    git(&source.root(), &["add", "later.txt"]);
    git(&source.root(), &["commit", "--quiet", "-m", "later"]);

    // When: the workspace is prepared.
    let workspace = source.prepare(&snapshot).unwrap();

    // Then: `main` points at the recorded head locally and in origin, which lives in the clone.
    let root = &workspace.toplevel;
    assert_eq!(git(root, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(git(root, &["rev-parse", "HEAD"]), snapshot.head_commit);
    assert_eq!(
        git(root, &["rev-parse", "--abbrev-ref", "main@{upstream}"]),
        "origin/main"
    );
    let origin = PathBuf::from(git(root, &["remote", "get-url", "origin"]));
    assert_eq!(origin, workspace.origin);
    assert!(origin.starts_with(root));
    assert_eq!(git(root, &["remote"]), "origin");
    assert_eq!(
        git(&origin, &["rev-parse", "refs/heads/main"]),
        snapshot.head_commit
    );
    assert_eq!(git(&origin, &["rev-parse", "refs/heads/other"]), other);
    assert_eq!(
        git(root, &["for-each-ref", "--format=%(refname)", "refs/heads"]),
        "refs/heads/main"
    );
    assert!(!root.join(".git/objects/info/alternates").exists());
    assert!(!origin.join("objects/info/alternates").exists());
    assert_eq!(workspace.project_dir, root.join("app/"));
    assert_eq!(git(root, &["config", "--get", "commit.gpgsign"]), "false");
    assert_eq!(git(root, &["config", "--get", "user.name"]), "evorch-test");
}

#[test]
fn detached_start_is_checked_out_detached() {
    // Given: a start recorded with HEAD detached.
    let source = Source::new();
    git(&source.root(), &["checkout", "--quiet", "--detach"]);
    let snapshot = source.snapshot("");
    assert_eq!(snapshot.head_ref, None);

    // When: the workspace is prepared.
    let workspace = source.prepare(&snapshot).unwrap();

    // Then: HEAD is detached at the head commit and no local branch remains.
    let root = &workspace.toplevel;
    let symbolic = git_command(root)
        .args(["symbolic-ref", "-q", "HEAD"])
        .status()
        .unwrap();
    assert_eq!(symbolic.code(), Some(1));
    assert_eq!(git(root, &["rev-parse", "HEAD"]), snapshot.head_commit);
    assert_eq!(
        git(root, &["for-each-ref", "--format=%(refname)", "refs/heads"]),
        ""
    );
}

#[test]
fn pushes_to_origin_work_offline_and_are_reported_without_touching_the_source() {
    // Given: a prepared workspace and a fingerprint of the dirty source repository.
    let source = Source::new();
    source.dirty();
    let before = source.fingerprint();
    let workspace = source.prepare(&source.snapshot("")).unwrap();
    let root = &workspace.toplevel;
    let head = git(root, &["rev-parse", "HEAD"]);

    // When: the agent commits, pushes `main` and a new branch, and edits a file.
    git(root, &["add", "--all"]);
    git(root, &["commit", "--quiet", "-m", "agent work"]);
    git(
        root,
        &["push", "--quiet", "origin", "main", "main:refs/heads/topic"],
    );
    std::fs::write(root.join("agent.txt"), "new\n").unwrap();
    let pushed = git(root, &["rev-parse", "HEAD"]);
    let result = workspace.finish().unwrap();

    // Then: both ref changes are reported, the diff holds only the agent's edit,
    // and the source repository is byte-for-byte unchanged.
    assert_eq!(
        result.pushed_refs,
        [
            RefChange {
                name: "refs/heads/main".into(),
                before: Some(head),
                after: Some(pushed.clone()),
            },
            RefChange {
                name: "refs/heads/topic".into(),
                before: None,
                after: Some(pushed),
            },
        ]
    );
    let diff = String::from_utf8(result.diff).unwrap();
    assert!(diff.contains("b/agent.txt"), "{diff}");
    assert!(!diff.contains("tracked.txt"), "{diff}");
    assert_ne!(result.final_tree, workspace.start_tree);
    assert_eq!(source.fingerprint(), before);
}

#[test]
fn unchanged_workspace_finishes_with_an_empty_diff() {
    // Given: a prepared workspace the agent did not change.
    let source = Source::new();
    source.dirty();
    let workspace = source.prepare(&source.snapshot("")).unwrap();

    // When: it is measured.
    let result = workspace.finish().unwrap();

    // Then: nothing changed and nothing was pushed.
    assert_eq!(result.final_tree, workspace.start_tree);
    assert!(result.diff.is_empty());
    assert!(result.pushed_refs.is_empty());
}

#[test]
fn diff_that_does_not_apply_fails_preparation() {
    // Given: a recorded diff whose context no longer matches the head.
    let source = Source::new();
    let mut snapshot = source.snapshot("");
    snapshot.diff = b"diff --git a/tracked.txt b/tracked.txt\n--- a/tracked.txt\n+++ b/tracked.txt\n@@ -1 +1 @@\n-something else\n+two\n".to_vec();

    // When: the workspace is prepared.
    let error = source.prepare(&snapshot).unwrap_err();

    // Then: preparation fails instead of continuing from a partial state.
    assert!(matches!(error, PrepareError::Apply(_)), "{error}");
}

#[test]
fn missing_head_commit_fails_preparation() {
    // Given: a start whose commit the source repository does not have.
    let source = Source::new();
    let mut snapshot = source.snapshot("");
    snapshot.head_commit = "0123456789abcdef0123456789abcdef01234567".into();

    // When / Then: preparation reports the missing commit.
    assert!(matches!(
        source.prepare(&snapshot),
        Err(PrepareError::MissingCommit(_))
    ));
}

#[test]
fn untracked_entry_below_a_symlink_is_refused() {
    // Given: an untracked symlink to a directory outside the clone and a file "inside" it.
    let source = Source::new();
    let outside = source.dir.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    let mut snapshot = source.snapshot("");
    snapshot.untracked = vec![
        UntrackedEntry {
            path: "escape".into(),
            kind: UntrackedKind::Symlink,
            executable: false,
            content: outside.to_string_lossy().into_owned().into_bytes(),
        },
        UntrackedEntry {
            path: "escape/file".into(),
            kind: UntrackedKind::File,
            executable: false,
            content: b"x".to_vec(),
        },
    ];
    let mut reversed = snapshot.clone();
    reversed.untracked.reverse();

    // When: the workspace is prepared with either entry order.
    let first = source.prepare(&snapshot);
    std::fs::remove_dir_all(source.dir.path().join("run")).unwrap();
    let second = source.prepare(&reversed);

    // Then: nothing is written through the link.
    for result in [first, second] {
        assert!(matches!(result, Err(PrepareError::Untracked { .. })));
    }
    assert!(!outside.join("file").exists());
}

#[test]
fn untracked_entry_outside_the_repository_is_refused() {
    // Given: untracked paths that climb out of the clone or are absolute.
    let source = Source::new();
    let outside = source.dir.path().join("outside");
    for path in [
        "../outside".to_owned(),
        "app/../../outside".to_owned(),
        outside.to_string_lossy().into_owned(),
    ] {
        let mut snapshot = source.snapshot("");
        snapshot.untracked = vec![UntrackedEntry {
            path: path.clone(),
            kind: UntrackedKind::File,
            executable: false,
            content: b"x".to_vec(),
        }];

        // When: the workspace is prepared.
        let result = source.prepare(&snapshot);
        let _ = std::fs::remove_dir_all(source.dir.path().join("run"));

        // Then: preparation fails and nothing is written outside.
        assert!(
            matches!(result, Err(PrepareError::Untracked { .. })),
            "{path}"
        );
        assert!(!outside.exists(), "{path}");
    }
}
