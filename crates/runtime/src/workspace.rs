//! runtime 所有 git worktree (isolated workspace) の管理。
//!
//! 残存 worktree は自動回収しない。新規作成は残存 path を
//! [`WorkspaceError::PathExists`] として拒否し、停止 run の復元だけが
//! [`WorktreeManager::open_existing`] により所有権を検証して再接続する。
//! stale worktree の自動 prune はこのモジュールの対象外である。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::run::RunId;

const BRANCH_PREFIX: &str = "evorch/task/";

/// worktree 管理の失敗を表す。
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    /// 作成対象の branch が既に存在する。
    #[error("branch already exists: {branch}")]
    BranchExists { branch: String },
    /// checkout 対象として指定された branch が存在しない。
    #[error("branch not found: {branch}")]
    BranchMissing { branch: String },
    /// 作成対象の path が既に存在する。
    #[error("worktree path already exists: {path}", path = path.display())]
    PathExists { path: PathBuf },
    /// 復元対象の worktree path が存在しない。
    #[error("worktree path not found: {path}", path = path.display())]
    PathMissing { path: PathBuf },
    /// 残存 path が期待する repository / branch の登録済み worktree ではない。
    #[error("retained worktree does not match this run: {path}", path = path.display())]
    RetainedWorktreeMismatch { path: PathBuf },
    /// 指定 path が管理対象の git repository root ではない。
    #[error("not a git repository root: {detail}")]
    NotARepo { detail: String },
    /// git command が失敗した。
    #[error("git command failed: {detail}")]
    Git { detail: String },
    /// filesystem 操作が失敗した。
    #[error("filesystem operation failed for {path}: {source}", path = path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// cleanup 対象が manager の所有範囲外である。
    #[error("refusing to clean foreign worktree path: {path}", path = path.display())]
    ForeignPath { path: PathBuf },
}

/// canonical な git repository root。
#[derive(Debug, Clone)]
pub struct Project {
    repo_root: PathBuf,
}

impl Project {
    /// path を canonicalize し、repository root であることを検証する。
    ///
    /// # Errors
    /// path を解決できない場合、git repository でない場合、または repository 内の
    /// subdirectory を指定した場合に [`WorkspaceError`] を返す。
    pub fn new(repo_root: PathBuf) -> Result<Self, WorkspaceError> {
        let canonical_root =
            fs::canonicalize(&repo_root).map_err(|source| WorkspaceError::NotARepo {
                detail: format!("{}: {source}", repo_root.display()),
            })?;
        let output = git_output(&canonical_root, &["rev-parse", "--show-toplevel"])?;
        if !output.status.success() {
            return Err(WorkspaceError::NotARepo {
                detail: output_detail(&output),
            });
        }
        let reported = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        let canonical_reported =
            fs::canonicalize(&reported).map_err(|source| WorkspaceError::NotARepo {
                detail: format!("{}: {source}", reported.display()),
            })?;
        if canonical_reported != canonical_root {
            return Err(WorkspaceError::NotARepo {
                detail: format!(
                    "{} resolves to repository root {}",
                    canonical_root.display(),
                    canonical_reported.display()
                ),
            });
        }
        Ok(Self {
            repo_root: canonical_root,
        })
    }

    /// canonical な repository root を返す。
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }
}

/// runtime 所有 worktree の作成を管理する。
#[derive(Debug, Clone)]
pub struct WorktreeManager {
    project: Project,
}

impl WorktreeManager {
    /// project に対する manager を作成する。
    pub const fn new(project: Project) -> Self {
        Self { project }
    }

    pub(crate) fn repo_root(&self) -> &Path {
        self.project.repo_root()
    }

    /// repository の git common directory を canonical path で返す。
    ///
    /// # Errors
    /// `git rev-parse` または path の解決に失敗した場合に [`WorkspaceError`] を返す。
    pub(crate) fn git_common_dir(&self) -> Result<PathBuf, WorkspaceError> {
        let output = git_output(self.project.repo_root(), &["rev-parse", "--git-common-dir"])?;
        if !output.status.success() {
            return Err(WorkspaceError::Git {
                detail: output_detail(&output),
            });
        }
        let reported = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        let path = if reported.is_absolute() {
            reported
        } else {
            self.project.repo_root().join(reported)
        };
        fs::canonicalize(&path).map_err(|source| WorkspaceError::Io { path, source })
    }

    /// run 専用 branch 名と worktree path を作成前に導出する。
    ///
    /// 実作成を行わずに inspection へ先行登録するために使う (issue #71 CI 対応)。
    pub(crate) fn planned(&self, run_id: RunId) -> (String, PathBuf) {
        let run_name = run_id.to_string();
        let branch = format!("{BRANCH_PREFIX}{run_name}");
        self.planned_on_branch(run_id, &branch)
    }

    /// 指定 branch の checkout 用に、run 名から worktree path を作成前に導出する。
    ///
    /// path は branch 名から導出せず run 名 (`run-N`) から決める。既存 branch を
    /// 別 run が再 checkout しても path が衝突しないためである (issue #73 D2)。
    pub(crate) fn planned_on_branch(&self, run_id: RunId, branch: &str) -> (String, PathBuf) {
        let run_name = run_id.to_string();
        let path = self
            .project
            .repo_root
            .join(".evorch")
            .join("worktrees")
            .join(&run_name);
        (branch.to_string(), path)
    }

    /// Reattach a retained run worktree without resetting its branch or dirty files.
    ///
    /// # Errors
    /// Returns [`WorkspaceError::PathMissing`] only when the expected path is absent.
    /// Existing paths must be registered in this repository on the run's own branch;
    /// foreign directories, moved worktrees, symlinks and branch mismatches fail closed.
    pub fn open_existing(&self, run_id: RunId) -> Result<OwnedWorktree, WorkspaceError> {
        let (branch, path) = self.planned(run_id);
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(WorkspaceError::PathMissing { path });
            }
            Err(source) => return Err(WorkspaceError::Io { path, source }),
        }
        let mismatch = || WorkspaceError::RetainedWorktreeMismatch { path: path.clone() };
        if fs::canonicalize(&path).map_err(|_| mismatch())? != path {
            return Err(mismatch());
        }
        let output = git_output(
            self.project.repo_root(),
            &["worktree", "list", "--porcelain", "-z"],
        )?;
        if !output.status.success() {
            return Err(WorkspaceError::Git {
                detail: output_detail(&output),
            });
        }
        let listing = String::from_utf8_lossy(&output.stdout);
        let expected_path = format!("worktree {}", path.display());
        let expected_branch = format!("branch refs/heads/{branch}");
        if !listing.split("\0\0").any(|record| {
            let fields: Vec<_> = record.split('\0').collect();
            fields.contains(&expected_path.as_str()) && fields.contains(&expected_branch.as_str())
        }) {
            return Err(mismatch());
        }
        // A stale registration alone is insufficient: the directory must still be
        // the registered checkout, not a replacement directory or foreign repository.
        let root = git_output(&path, &["rev-parse", "--show-toplevel"])?;
        let common = git_output(
            &path,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let head = git_output(&path, &["symbolic-ref", "--quiet", "HEAD"])?;
        if !root.status.success() || !common.status.success() || !head.status.success() {
            return Err(mismatch());
        }
        let reported_root = PathBuf::from(String::from_utf8_lossy(&root.stdout).trim());
        let reported_common = PathBuf::from(String::from_utf8_lossy(&common.stdout).trim());
        if reported_root != path
            || String::from_utf8_lossy(&head.stdout).trim() != format!("refs/heads/{branch}")
            || fs::canonicalize(reported_common).map_err(|_| mismatch())?
                != self.git_common_dir()?
        {
            return Err(mismatch());
        }
        Ok(OwnedWorktree {
            path,
            branch,
            run_name: run_id.to_string(),
            repo_root: self.project.repo_root.clone(),
        })
    }

    /// run 専用 branch と worktree を二段階で作成する。
    ///
    /// # Errors
    /// branch/path の衝突、git command、または filesystem 操作の失敗時に
    /// [`WorkspaceError`] を返す。worktree add の失敗時だけ、この呼出しで作成した
    /// branch と部分ディレクトリを rollback する。
    pub fn create(&self, run_id: RunId) -> Result<OwnedWorktree, WorkspaceError> {
        let run_name = run_id.to_string();
        let branch = format!("{BRANCH_PREFIX}{run_name}");
        if branch_exists(&self.project.repo_root, &branch)? {
            return Err(WorkspaceError::BranchExists { branch });
        }
        self.add_worktree(run_name, branch, true)
    }

    /// 既存 branch を checkout する worktree を run 名の path に作成する。
    ///
    /// branch は新規作成せず、存在しなければ fail-closed で拒否する (issue #73 D2:
    /// 修復ラウンドの run が同一 deliverable branch を再利用する)。worktree path
    /// は branch 名からではなく run 名から導出される。
    ///
    /// 直前 run の worktree cleanup は終端 event 発行後に非同期で走るため、同一
    /// branch の引き継ぎ checkout は git の "already used by worktree" 衝突を
    /// 有限 retry で待ち合わせる。
    ///
    /// # Errors
    /// branch 不在、path 衝突、git command、または filesystem 操作の失敗時に
    /// [`WorkspaceError`] を返す。worktree add の失敗時は部分ディレクトリだけを
    /// rollback する (branch は呼出し前から存在するため削除しない)。
    pub fn create_on_branch(
        &self,
        run_id: RunId,
        branch: &str,
    ) -> Result<OwnedWorktree, WorkspaceError> {
        let branch = branch.to_string();
        if !branch_exists(&self.project.repo_root, &branch)? {
            return Err(WorkspaceError::BranchMissing { branch });
        }
        let mut attempt = 0;
        loop {
            match self.add_worktree(run_id.to_string(), branch.clone(), false) {
                Ok(owned) => return Ok(owned),
                Err(error) if attempt < BRANCH_RELEASE_MAX_ATTEMPTS && is_branch_held(&error) => {
                    attempt += 1;
                    std::thread::sleep(BRANCH_RELEASE_RETRY_INTERVAL);
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// worktree 本体の作成共通部。`create_branch` が真のときだけ branch を
    /// `HEAD` から新規作成し、rollback も branch を含める。
    fn add_worktree(
        &self,
        run_name: String,
        branch: String,
        create_branch: bool,
    ) -> Result<OwnedWorktree, WorkspaceError> {
        let path = self
            .project
            .repo_root
            .join(".evorch")
            .join("worktrees")
            .join(&run_name);
        if path.try_exists().map_err(|source| WorkspaceError::Io {
            path: path.clone(),
            source,
        })? {
            return Err(WorkspaceError::PathExists { path });
        }

        if create_branch {
            run_git(
                &self.project.repo_root,
                &["branch", branch.as_str(), "HEAD"],
            )?;
        }
        let add_result = run_git(
            &self.project.repo_root,
            &[
                "worktree",
                "add",
                path.to_string_lossy().as_ref(),
                branch.as_str(),
            ],
        );
        if let Err(error) = add_result {
            if create_branch {
                rollback_created_branch(&self.project.repo_root, &path, &branch);
            } else {
                rollback_preexisting_branch_worktree(&self.project.repo_root, &path);
            }
            return Err(error);
        }

        ensure_git_metadata(&self.project.repo_root)?;
        append_info_exclude(&self.project.repo_root)?;

        Ok(OwnedWorktree {
            path,
            branch,
            run_name,
            repo_root: self.project.repo_root.clone(),
        })
    }
}

/// manager が作成した worktree と merge 用 branch。
#[derive(Debug)]
pub struct OwnedWorktree {
    /// worktree の絶対 path。
    pub path: PathBuf,
    /// merge deliverable として cleanup 後も保持する branch 名。
    pub branch: String,
    /// worktree path の導出元 run 名 (`run-N`)。cleanup の所有判定は
    /// branch 名からではなくこの値で行う (issue #73 D2)。
    pub run_name: String,
    repo_root: PathBuf,
}

impl OwnedWorktree {
    /// worktree を削除し、branch は保持する。
    ///
    /// # Errors
    /// manager の所有範囲外の path、または削除・prune の失敗時に
    /// [`WorkspaceError`] を返す。
    pub fn cleanup(self) -> Result<(), WorkspaceError> {
        let expected = self
            .repo_root
            .join(".evorch/worktrees")
            .join(&self.run_name);
        if expected != self.path {
            return Err(WorkspaceError::ForeignPath { path: self.path });
        }

        let path_arg = self.path.to_string_lossy();
        let remove = git_output(
            &self.repo_root,
            &["worktree", "remove", "--force", path_arg.as_ref()],
        )?;
        if remove.status.success() {
            return Ok(());
        }

        match fs::remove_dir_all(&self.path) {
            Ok(()) => {}
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(WorkspaceError::Io {
                    path: self.path,
                    source,
                });
            }
        }
        run_git(&self.repo_root, &["worktree", "prune"])
    }
}

const BRANCH_RELEASE_MAX_ATTEMPTS: u32 = 100;
const BRANCH_RELEASE_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

fn is_branch_held(error: &WorkspaceError) -> bool {
    matches!(
        error,
        WorkspaceError::Git { detail } if detail.contains("already used by worktree")
    )
}

fn branch_exists(repo_root: &Path, branch: &str) -> Result<bool, WorkspaceError> {
    let reference = format!("refs/heads/{branch}");
    let output = git_output(
        repo_root,
        &["show-ref", "--verify", "--quiet", reference.as_str()],
    )?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(WorkspaceError::Git {
            detail: output_detail(&output),
        }),
    }
}

fn rollback_created_branch(repo_root: &Path, path: &Path, branch: &str) {
    let _ = run_git(repo_root, &["branch", "-D", branch]);
    if path.exists() {
        let _ = fs::remove_dir_all(path);
    }
    let _ = run_git(repo_root, &["worktree", "prune"]);
    let _ = run_git(repo_root, &["branch", "-D", branch]);
}

/// 事前存在 branch の worktree add 失敗時の rollback。branch は呼出し前から
/// 存在するため削除せず、部分生成物だけを撤去する。
fn rollback_preexisting_branch_worktree(repo_root: &Path, path: &Path) {
    if path.exists() {
        let _ = fs::remove_dir_all(path);
    }
    let _ = run_git(repo_root, &["worktree", "prune"]);
}

fn ensure_git_metadata(repo_root: &Path) -> Result<(), WorkspaceError> {
    for path in [
        repo_root.join(".git/logs"),
        repo_root.join(".git/refs/heads"),
    ] {
        fs::create_dir_all(&path).map_err(|source| WorkspaceError::Io { path, source })?;
    }
    Ok(())
}

fn append_info_exclude(repo_root: &Path) -> Result<(), WorkspaceError> {
    let path = repo_root.join(".git/info/exclude");
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(WorkspaceError::Io {
                path: path.clone(),
                source,
            });
        }
    };
    if content.lines().any(|line| line == ".evorch/") {
        return Ok(());
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|source| WorkspaceError::Io {
            path: path.clone(),
            source,
        })?;
    if !content.is_empty() && !content.ends_with('\n') {
        file.write_all(b"\n").map_err(|source| WorkspaceError::Io {
            path: path.clone(),
            source,
        })?;
    }
    file.write_all(b".evorch/\n")
        .map_err(|source| WorkspaceError::Io { path, source })
}

fn run_git(repo_root: &Path, args: &[&str]) -> Result<(), WorkspaceError> {
    let output = git_output(repo_root, args)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(WorkspaceError::Git {
            detail: output_detail(&output),
        })
    }
}

fn git_output(repo_root: &Path, args: &[&str]) -> Result<Output, WorkspaceError> {
    let mut command = Command::new("git");
    // Git hooks export repository-local variables that override `git -C`.
    // Match `git rev-parse --local-env-vars`, plus the ref namespace, while
    // retaining global configuration (including the user's signing settings).
    for name in [
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_OBJECT_DIRECTORY",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_IMPLICIT_WORK_TREE",
        "GIT_GRAFT_FILE",
        "GIT_INDEX_FILE",
        "GIT_NO_REPLACE_OBJECTS",
        "GIT_REPLACE_REF_BASE",
        "GIT_PREFIX",
        "GIT_SHALLOW_FILE",
        "GIT_COMMON_DIR",
        "GIT_NAMESPACE",
    ] {
        command.env_remove(name);
    }
    command
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
        .map_err(|source| WorkspaceError::Git {
            detail: format!("could not execute git: {source}"),
        })
}

fn output_detail(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if stderr.is_empty() {
        format!("exit status {}", output.status)
    } else {
        stderr
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use tempfile::TempDir;

    use super::{OwnedWorktree, Project, WorkspaceError, WorktreeManager, git_output};
    use crate::run::RunId;

    fn git(repo: &Path, args: &[&str]) -> std::process::Output {
        git_output(repo, args).expect("git を実行できる")
    }

    fn init_repo() -> (TempDir, PathBuf) {
        let temp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).expect("リポジトリ用ディレクトリを作成できる");
        assert!(git(&repo, &["init"]).status.success());
        assert!(
            git(&repo, &["config", "user.name", "Evorch Test"])
                .status
                .success()
        );
        assert!(
            git(&repo, &["config", "user.email", "evorch@example.invalid"])
                .status
                .success()
        );
        fs::write(repo.join("README.md"), "# test\n").expect("初期ファイルを書き込める");
        assert!(git(&repo, &["add", "README.md"]).status.success());
        // Fixture commits must not depend on the user's signing key or agent.
        assert!(
            git(
                &repo,
                &["-c", "commit.gpgsign=false", "commit", "-m", "initial"]
            )
            .status
            .success()
        );
        (temp, repo)
    }

    fn manager(repo: &Path) -> WorktreeManager {
        let project = Project::new(repo.to_path_buf()).expect("git リポジトリを検証できる");
        WorktreeManager::new(project)
    }

    #[test]
    fn inherited_git_environment_does_not_modify_foreign_repository() {
        const CHILD: &str = "EVORCH_WORKSPACE_GIT_ENV_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let (_temp, repo) = init_repo();
            assert_eq!(
                String::from_utf8(git(&repo, &["config", "--local", "user.name"]).stdout)
                    .unwrap()
                    .trim(),
                "Evorch Test"
            );
            assert_eq!(
                String::from_utf8(git(&repo, &["config", "--local", "user.email"]).stdout)
                    .unwrap()
                    .trim(),
                "evorch@example.invalid"
            );
            let manager = manager(&repo);
            let owned = manager.create(RunId::new(99)).unwrap();
            assert_eq!(
                manager.open_existing(RunId::new(99)).unwrap().path,
                owned.path
            );
            owned.cleanup().unwrap();
            return;
        }

        let (_foreign_temp, foreign) = init_repo();
        assert!(
            git(
                &foreign,
                &["config", "--local", "user.name", "Foreign Owner"]
            )
            .status
            .success()
        );
        assert!(
            git(
                &foreign,
                &["config", "--local", "user.email", "foreign@example.invalid"],
            )
            .status
            .success()
        );
        let foreign_git = foreign.join(".git");
        let config_before = fs::read(foreign_git.join("config")).unwrap();
        let index_before = fs::read(foreign_git.join("index")).unwrap();
        let refs_before = git(&foreign, &["show-ref"]).stdout;

        // Change only a subprocess's environment, so parallel tests cannot be
        // redirected. Every injected path belongs to this temporary fixture.
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "workspace::tests::inherited_git_environment_does_not_modify_foreign_repository",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("GIT_DIR", &foreign_git)
            .env("GIT_COMMON_DIR", &foreign_git)
            .env("GIT_WORK_TREE", &foreign)
            .env("GIT_INDEX_FILE", foreign_git.join("index"))
            .env("GIT_OBJECT_DIRECTORY", foreign_git.join("objects"))
            .env("GIT_CONFIG", foreign_git.join("config"))
            .env("GIT_NAMESPACE", "foreign")
            .output()
            .unwrap();

        assert_eq!(fs::read(foreign_git.join("config")).unwrap(), config_before);
        assert_eq!(fs::read(foreign_git.join("index")).unwrap(), index_before);
        assert_eq!(git(&foreign, &["show-ref"]).stdout, refs_before);
        assert!(
            output.status.success(),
            "child failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Given: 1 commit を持つ git repo / When: run-7 の worktree を作成 / Then: worktree と対応 branch が存在する
    #[test]
    fn create_makes_worktree_and_branch() {
        let (_temp, repo) = init_repo();

        let owned = manager(&repo)
            .create(RunId::new(7))
            .expect("worktree を作成できる");

        assert_eq!(owned.path, repo.join(".evorch/worktrees/run-7"));
        assert_eq!(owned.branch, "evorch/task/run-7");
        assert!(owned.path.join(".git").exists());
        assert!(
            git(
                &repo,
                &["show-ref", "--verify", "refs/heads/evorch/task/run-7"]
            )
            .status
            .success()
        );
    }

    // Given: 先行 run が保持する branch / When: 保持側を遅延 cleanup しつつ同一 branch を checkout / Then: retry で待ち合わせて作成できる
    #[test]
    fn create_on_branch_waits_for_branch_release() {
        let (_temp, repo) = init_repo();
        let manager = manager(&repo);
        let held = manager
            .create(RunId::new(1))
            .expect("先行 run の worktree を作成できる");
        let branch = held.branch.clone();

        let releaser = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            held.cleanup().expect("先行 worktree を cleanup できる");
        });
        let adopted = manager
            .create_on_branch(RunId::new(2), &branch)
            .expect("branch 解放後の checkout が retry で成功する");
        releaser.join().expect("releaser を join できる");

        assert_eq!(adopted.branch, branch);
        assert!(adopted.path.join(".git").exists());
    }

    // Given: 1 commit を持つ git repo / When: 異なる run の worktree を 2 回作成 / Then: info/exclude の .evorch/ は 1 行だけ
    #[test]
    fn create_appends_info_exclude_once() {
        let (_temp, repo) = init_repo();
        let manager = manager(&repo);

        let first = manager
            .create(RunId::new(1))
            .expect("最初の worktree を作成できる");
        let second = manager
            .create(RunId::new(2))
            .expect("次の worktree を作成できる");

        let exclude =
            fs::read_to_string(repo.join(".git/info/exclude")).expect("info/exclude を読み込める");
        assert_eq!(
            exclude.lines().filter(|line| *line == ".evorch/").count(),
            1
        );
        first.cleanup().expect("最初の worktree を削除できる");
        second.cleanup().expect("次の worktree を削除できる");
    }

    // Given: git metadata の logs と refs/heads を削除した repo / When: worktree を作成 / Then: 両ディレクトリが存在する
    #[test]
    fn create_ensures_git_metadata_dirs() {
        let (_temp, repo) = init_repo();
        let git_dir = repo.join(".git");
        assert!(git(&repo, &["pack-refs", "--all"]).status.success());
        fs::remove_dir_all(git_dir.join("logs")).expect("logs を削除できる");
        fs::remove_dir_all(git_dir.join("refs/heads")).expect("refs/heads を削除できる");

        let owned = manager(&repo)
            .create(RunId::new(3))
            .expect("worktree を作成できる");

        assert!(git_dir.join("logs").is_dir());
        assert!(git_dir.join("refs/heads").is_dir());
        owned.cleanup().expect("worktree を削除できる");
    }

    // Given: 対象 branch が既にある repo / When: 同じ run の worktree を作成 / Then: BranchExists で fail-closed に拒否する
    #[test]
    fn create_fails_closed_when_branch_exists() {
        let (_temp, repo) = init_repo();
        assert!(
            git(&repo, &["branch", "evorch/task/run-4", "HEAD"])
                .status
                .success()
        );

        let error = manager(&repo)
            .create(RunId::new(4))
            .expect_err("既存 branch を拒否する");

        assert!(matches!(
            error,
            WorkspaceError::BranchExists { branch } if branch == "evorch/task/run-4"
        ));
    }

    // Given: 対象 worktree path が既にある repo / When: 同じ run の worktree を作成 / Then: PathExists で fail-closed に拒否する
    #[test]
    fn create_fails_closed_when_worktree_path_exists() {
        let (_temp, repo) = init_repo();
        let path = repo.join(".evorch/worktrees/run-5");
        fs::create_dir_all(&path).expect("衝突パスを作成できる");

        let error = manager(&repo)
            .create(RunId::new(5))
            .expect_err("既存 path を拒否する");

        assert!(matches!(
            error,
            WorkspaceError::PathExists { path: actual } if actual == path
        ));
        assert!(
            !git(
                &repo,
                &["show-ref", "--verify", "refs/heads/evorch/task/run-5"]
            )
            .status
            .success()
        );
    }

    // Given: worktree 親が書込不可の repo / When: branch 作成後の worktree add が失敗 / Then: この呼出しが作った branch と部分 path を rollback する
    #[test]
    fn midway_failure_rolls_back_own_branch() {
        let (_temp, repo) = init_repo();
        let parent = repo.join(".evorch/worktrees");
        fs::create_dir_all(&parent).expect("worktree 親を作成できる");
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o555))
            .expect("worktree 親を書込不可にできる");

        let error = manager(&repo)
            .create(RunId::new(6))
            .expect_err("worktree add が失敗する");

        assert!(matches!(error, WorkspaceError::Git { .. }));
        assert!(
            !git(
                &repo,
                &["show-ref", "--verify", "refs/heads/evorch/task/run-6"]
            )
            .status
            .success()
        );
        assert!(!parent.join("run-6").exists());
    }

    // Given: manager が作成した worktree / When: cleanup / Then: worktree は消え branch は保持される
    #[test]
    fn cleanup_removes_worktree_keeps_branch() {
        let (_temp, repo) = init_repo();
        let owned = manager(&repo)
            .create(RunId::new(8))
            .expect("worktree を作成できる");
        let path = owned.path.clone();

        owned.cleanup().expect("worktree を削除できる");

        assert!(!path.exists());
        assert!(
            git(
                &repo,
                &["show-ref", "--verify", "refs/heads/evorch/task/run-8"]
            )
            .status
            .success()
        );
    }

    #[test]
    fn open_existing_preserves_dirty_worktree() {
        let (_temp, repo) = init_repo();
        let manager = manager(&repo);
        let owned = manager.create(RunId::new(80)).unwrap();
        fs::write(owned.path.join("dirty.txt"), "unfinished work").unwrap();
        let reopened = manager.open_existing(RunId::new(80)).unwrap();
        assert_eq!(reopened.path, owned.path);
        assert_eq!(reopened.branch, owned.branch);
        assert_eq!(reopened.run_name, owned.run_name);
        assert_eq!(
            fs::read_to_string(reopened.path.join("dirty.txt")).unwrap(),
            "unfinished work"
        );
        reopened.cleanup().unwrap();
    }

    #[test]
    fn open_existing_missing_path() {
        let (_temp, repo) = init_repo();
        assert!(matches!(
            manager(&repo).open_existing(RunId::new(81)),
            Err(WorkspaceError::PathMissing { .. })
        ));
    }

    #[test]
    fn open_existing_rejects_foreign_directory() {
        let (_temp, repo) = init_repo();
        let manager = manager(&repo);
        let (_, path) = manager.planned(RunId::new(82));
        fs::create_dir_all(&path).unwrap();
        assert!(matches!(
            manager.open_existing(RunId::new(82)),
            Err(WorkspaceError::RetainedWorktreeMismatch { .. })
        ));
        assert!(path.is_dir());
    }

    #[test]
    fn open_existing_rejects_moved_worktree_and_stale_registration() {
        let (_temp, repo) = init_repo();
        let manager = manager(&repo);
        let owned = manager.create(RunId::new(83)).unwrap();
        let moved = repo.join("moved-worktree");
        fs::rename(&owned.path, &moved).unwrap();
        // The old registration remains, but its directory is now a replacement.
        fs::create_dir(&owned.path).unwrap();
        assert!(matches!(
            manager.open_existing(RunId::new(83)),
            Err(WorkspaceError::RetainedWorktreeMismatch { .. })
        ));
        assert!(moved.join(".git").exists());
    }

    #[test]
    fn open_existing_rejects_wrong_branch() {
        let (_temp, repo) = init_repo();
        let manager = manager(&repo);
        let owned = manager.create(RunId::new(84)).unwrap();
        assert!(
            git(&owned.path, &["checkout", "-b", "foreign-branch"])
                .status
                .success()
        );
        assert!(matches!(
            manager.open_existing(RunId::new(84)),
            Err(WorkspaceError::RetainedWorktreeMismatch { .. })
        ));
        assert!(owned.path.is_dir());
    }

    // Given: manager 管理外 path を持つ偽の OwnedWorktree / When: cleanup / Then: path を変更せず拒否する
    #[test]
    fn cleanup_refuses_foreign_paths() {
        let (_temp, repo) = init_repo();
        let foreign = repo.join("foreign");
        fs::create_dir(&foreign).expect("管理外 path を作成できる");
        let owned = OwnedWorktree {
            path: foreign.clone(),
            branch: "evorch/task/run-9".to_string(),
            run_name: "run-9".to_string(),
            repo_root: repo,
        };

        let error = owned.cleanup().expect_err("管理外 path を拒否する");

        assert!(matches!(error, WorkspaceError::ForeignPath { path } if path == foreign));
        assert!(foreign.is_dir());
    }

    // Given: 既存 branch を持つ repo / When: create_on_branch で別 run 名の path を作成 / Then: branch を checkout し cleanup でも branch は保持される
    #[test]
    fn create_on_branch_checks_out_existing_branch_at_run_named_path() {
        let (_temp, repo) = init_repo();
        assert!(
            git(&repo, &["branch", "evorch/task/run-1", "HEAD"])
                .status
                .success()
        );

        let owned = manager(&repo)
            .create_on_branch(RunId::new(10), "evorch/task/run-1")
            .expect("既存 branch の worktree を作成できる");
        let path = owned.path.clone();

        assert_eq!(path, repo.join(".evorch/worktrees/run-10"));
        assert_eq!(owned.branch, "evorch/task/run-1");
        assert_eq!(owned.run_name, "run-10");
        assert!(path.join(".git").exists());

        owned.cleanup().expect("worktree を削除できる");

        assert!(!path.exists());
        assert!(
            git(
                &repo,
                &["show-ref", "--verify", "refs/heads/evorch/task/run-1"]
            )
            .status
            .success()
        );
    }

    // Given: branch が存在しない repo / When: create_on_branch / Then: BranchMissing で fail-closed に拒否する
    #[test]
    fn create_on_branch_fails_closed_when_branch_missing() {
        let (_temp, repo) = init_repo();

        let error = manager(&repo)
            .create_on_branch(RunId::new(11), "evorch/task/absent")
            .expect_err("不在 branch を拒否する");

        assert!(matches!(
            error,
            WorkspaceError::BranchMissing { branch } if branch == "evorch/task/absent"
        ));
        assert!(!repo.join(".evorch/worktrees/run-11").exists());
    }
}
