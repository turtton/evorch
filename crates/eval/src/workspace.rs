//! Rebuild a thread's start state in a runner-owned clone (ADR 0036 §3, §4).
//!
//! The clone is a plain local clone: alternates would point into the user's
//! repository, which the shell sandbox does not mount. Its `origin` is a bare
//! repository inside the clone's git directory, so pull and push work inside
//! the sandbox without network. The user's repository is only read.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use storage::thread_start::{ThreadStartSnapshot, UntrackedEntry, UntrackedKind};

use crate::git::{self, GitError};

/// Bare repository standing in for the original remotes, relative to the clone's git directory.
const ORIGIN_DIR: &str = "evorch-origin.git";
const START_REF: &str = "refs/evorch/eval-start";

#[derive(Debug, thiserror::Error)]
pub enum PrepareError {
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("the recorded head commit {0} is not in the source repository")]
    MissingCommit(String),
    #[error("the recorded diff does not apply: {0}")]
    Apply(GitError),
    #[error("cannot restore untracked {path}: {reason}")]
    Untracked { path: String, reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommitIdentity {
    pub name: String,
    pub email: String,
}

impl CommitIdentity {
    /// The host's `user.name` and `user.email`, or a fixed evaluation identity.
    pub fn from_host(dir: &Path) -> Self {
        Self {
            name: git::user_config(dir, "user.name").unwrap_or_else(|| "evorch eval".into()),
            email: git::user_config(dir, "user.email")
                .unwrap_or_else(|| "eval@evorch.invalid".into()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Workspace {
    pub toplevel: PathBuf,
    /// The project root inside the clone: `toplevel` joined with the recorded prefix.
    pub project_dir: PathBuf,
    pub origin: PathBuf,
    pub start_tree: String,
    origin_refs: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefChange {
    pub name: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WorkspaceResult {
    pub final_tree: String,
    pub diff: Vec<u8>,
    pub pushed_refs: Vec<RefChange>,
}

/// Clone `source` into `target` and rebuild the recorded working tree there.
///
/// # Errors
/// Any step that fails leaves the run unusable; a partial state is never returned.
pub fn prepare(
    source: &Path,
    snapshot: &ThreadStartSnapshot,
    target: &Path,
    identity: &CommitIdentity,
) -> Result<Workspace, PrepareError> {
    let parent = target
        .parent()
        .ok_or_else(|| std::io::Error::other("workspace has no parent directory"))?;
    std::fs::create_dir_all(parent)?;
    let source = path_str(source)?;
    let target_str = path_str(target)?;
    git::run(
        parent,
        &[
            "clone",
            "--quiet",
            "--local",
            "--no-checkout",
            source,
            target_str,
        ],
    )?;
    let toplevel = target.canonicalize()?;
    let head = snapshot.head_commit.as_str();
    if git::run(
        &toplevel,
        &["cat-file", "-e", &format!("{head}^{{commit}}")],
    )
    .is_err()
    {
        return Err(PrepareError::MissingCommit(head.to_owned()));
    }
    for (key, value) in [
        ("commit.gpgsign", "false"),
        ("tag.gpgsign", "false"),
        ("user.name", identity.name.as_str()),
        ("user.email", identity.email.as_str()),
    ] {
        git::run(&toplevel, &["config", "--local", key, value])?;
    }

    let origin = toplevel.join(".git").join(ORIGIN_DIR);
    git::run(
        &toplevel,
        &[
            "clone",
            "--quiet",
            "--bare",
            "--local",
            source,
            path_str(&origin)?,
        ],
    )?;
    let branch = snapshot
        .head_ref
        .as_deref()
        .and_then(|name| name.strip_prefix("refs/heads/"));
    if let Some(name) = &snapshot.head_ref
        && branch.is_some()
    {
        git::run(&origin, &["update-ref", name, head])?;
    }
    for remote in git::text(&toplevel, &["remote"])?.lines() {
        git::run(&toplevel, &["remote", "remove", remote])?;
    }
    git::run(&toplevel, &["remote", "add", "origin", path_str(&origin)?])?;
    git::run(&toplevel, &["fetch", "--quiet", "origin"])?;

    match branch {
        Some(branch) => {
            git::run(
                &toplevel,
                &["checkout", "--quiet", "-f", "-B", branch, head],
            )?;
            git::run(
                &toplevel,
                &[
                    "branch",
                    "--quiet",
                    &format!("--set-upstream-to=origin/{branch}"),
                    branch,
                ],
            )?;
        }
        None => {
            git::run(&toplevel, &["checkout", "--quiet", "-f", "--detach", head])?;
        }
    }
    for local in git::text(
        &toplevel,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
    )?
    .lines()
    .filter(|local| Some(*local) != branch)
    {
        git::run(&toplevel, &["branch", "--quiet", "-D", local])?;
    }

    if !snapshot.diff.is_empty() {
        git::run_with_input(
            &toplevel,
            &["apply", "--binary", "--whitespace=nowarn", "-"],
            &snapshot.diff,
        )
        .map_err(PrepareError::Apply)?;
    }
    let mut untracked: Vec<_> = snapshot.untracked.iter().collect();
    untracked.sort_by_key(|entry| entry.kind == UntrackedKind::Symlink);
    for entry in untracked {
        restore_untracked(&toplevel, entry)?;
    }

    let start_tree = worktree_tree(&toplevel)?;
    git::run(&toplevel, &["update-ref", START_REF, &start_tree])?;
    let origin_refs = refs(&origin)?;
    let project_dir = toplevel.join(&snapshot.repo_prefix);
    Ok(Workspace {
        toplevel,
        project_dir,
        origin,
        start_tree,
        origin_refs,
    })
}

impl Workspace {
    /// Measure what the replay changed, relative to the rebuilt start.
    ///
    /// # Errors
    /// Fails when the clone can no longer be read, for example after its git directory was removed.
    pub fn finish(&self) -> Result<WorkspaceResult, GitError> {
        let final_tree = worktree_tree(&self.toplevel)?;
        let diff = git::run(
            &self.toplevel,
            &[
                "diff",
                "--binary",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                &self.start_tree,
                &final_tree,
            ],
        )?;
        let after = refs(&self.origin)?;
        let mut names: Vec<_> = self.origin_refs.keys().chain(after.keys()).collect();
        names.sort();
        names.dedup();
        let pushed_refs = names
            .into_iter()
            .filter_map(|name| {
                let before = self.origin_refs.get(name);
                let now = after.get(name);
                (before != now).then(|| RefChange {
                    name: name.clone(),
                    before: before.cloned(),
                    after: now.cloned(),
                })
            })
            .collect();
        Ok(WorkspaceResult {
            final_tree,
            diff,
            pushed_refs,
        })
    }
}

/// The tree of every non-ignored file in the working tree, built in a scratch
/// index so the clone's own index is left as the agent had it.
fn worktree_tree(toplevel: &Path) -> Result<String, GitError> {
    let git_dir = toplevel.join(".git");
    let index = git_dir.join("evorch-tree.index");
    let seeded = std::fs::copy(git_dir.join("index"), &index).is_ok();
    let env = [("GIT_INDEX_FILE", index.as_os_str())];
    let result = (|| {
        if !seeded {
            git::run_with_env(toplevel, &["read-tree", "--empty"], &env)?;
        }
        git::run_with_env(toplevel, &["add", "--all", "--", ":/"], &env)?;
        let tree = git::run_with_env(toplevel, &["write-tree"], &env)?;
        Ok(String::from_utf8_lossy(&tree).trim().to_owned())
    })();
    let _ = std::fs::remove_file(&index);
    result
}

fn refs(repository: &Path) -> Result<BTreeMap<String, String>, GitError> {
    Ok(git::text(
        repository,
        &["for-each-ref", "--format=%(refname) %(objectname)"],
    )?
    .lines()
    .filter_map(|line| line.split_once(' '))
    .map(|(name, id)| (name.to_owned(), id.to_owned()))
    .collect())
}

fn restore_untracked(toplevel: &Path, entry: &UntrackedEntry) -> Result<(), PrepareError> {
    let fail = |reason: String| PrepareError::Untracked {
        path: entry.path.clone(),
        reason,
    };
    let relative = Path::new(&entry.path);
    if relative.as_os_str().is_empty()
        || !relative
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
    {
        return Err(fail("the path leaves the repository".into()));
    }
    let path = toplevel.join(relative);
    let mut ancestor = path.parent();
    while let Some(dir) = ancestor.filter(|dir| *dir != toplevel) {
        match std::fs::symlink_metadata(dir) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(fail(format!("{} is not a directory", dir.display())));
            }
            _ => ancestor = dir.parent(),
        }
    }
    if std::fs::symlink_metadata(&path).is_ok() {
        return Err(fail("the path already exists".into()));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| fail(error.to_string()))?;
    }
    match entry.kind {
        UntrackedKind::File => write_file(&path, &entry.content, entry.executable),
        UntrackedKind::Symlink => symlink(&entry.content, &path),
    }
    .map_err(|error| fail(error.to_string()))
}

#[cfg(unix)]
fn write_file(path: &Path, content: &[u8], executable: bool) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, content)?;
    let mode = if executable { 0o755 } else { 0o644 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(unix)]
fn symlink(target: &[u8], path: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    std::os::unix::fs::symlink(OsStr::from_bytes(target), path)
}

fn path_str(path: &Path) -> Result<&str, PrepareError> {
    path.to_str().ok_or_else(|| {
        PrepareError::Io(std::io::Error::other(format!(
            "path is not UTF-8: {}",
            path.display()
        )))
    })
}

#[cfg(test)]
pub(crate) mod tests;
