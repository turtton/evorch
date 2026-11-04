//! Finding the local checkout of a pull request's head, so the harness can report that
//! checkout's HEAD commit (`local_worktree_head`).
//!
//! The question is "which checkout holds this pull request's head branch?", and the answer
//! is given only when it is certain:
//!
//! - The search stays inside the directories it is given (the project root, the allowed
//!   directories, the thread's read directories). For each it takes the checkout the
//!   directory belongs to and the worktrees of that repository that live under the
//!   directory. Nothing else on the disk is looked at.
//! - Checkouts are identified by canonical path, so a directory reached twice, through a
//!   symlink or as a subdirectory, is one checkout.
//! - A checkout matches when **any** of its remotes (not only `origin`) names the head
//!   repository on github.com and its current branch is the head branch.
//! - Exactly one match gives its HEAD commit. No match, several matches, a detached HEAD, an
//!   unknown head repository (the fork was deleted), a branch that changed while it was being
//!   read, and a git failure all give no commit, and say why ([`CheckoutMiss`]).
//!
//! Git is run through [`crate::git_read`] (hardened, bounded, under one deadline) on the
//! blocking pool.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::git_read::{GitReadError, GitReader};
use crate::repository::parse_remote;

/// The most checkouts one resolution looks at; more is "too many to be sure".
pub const MAX_CHECKOUT_CANDIDATES: usize = 32;

const LIST_LIMIT: usize = 256 * 1024;
const SMALL_LIMIT: usize = 16 * 1024;

/// A checkout that holds the head branch of the pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkout {
    /// The canonical path of the checkout.
    pub path: PathBuf,
    /// The commit its HEAD is at (lowercase hex).
    pub head_sha: String,
    /// The name of the remote that points at the head repository.
    pub remote: String,
}

/// Why no checkout was named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckoutMiss {
    /// The head repository is not known (a deleted fork), so nothing can be matched.
    HeadRepositoryUnknown,
    /// No checkout in the given directories is on the head branch of the head repository
    /// (or the directories hold no git checkout at all).
    NoCheckout,
    /// A checkout of the head repository has a detached HEAD, and no other one matches.
    Detached,
    /// More than one checkout matches.
    Ambiguous,
    /// The branch or HEAD of a checkout changed while it was being read.
    Changed,
    /// More than [`MAX_CHECKOUT_CANDIDATES`] checkouts are in range.
    TooManyCandidates,
    /// Git failed (not installed, unreadable repository, output over the limit).
    ReadFailed,
    /// The deadline passed.
    TimedOut,
}

/// The outcome of a resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutResolution {
    /// Exactly one checkout matches.
    Found(Checkout),
    /// No checkout can be named.
    Missing(CheckoutMiss),
}

impl CheckoutResolution {
    /// The HEAD commit of the matching checkout, if there is exactly one.
    #[must_use]
    pub fn head_sha(&self) -> Option<&str> {
        match self {
            Self::Found(checkout) => Some(&checkout.head_sha),
            Self::Missing(_) => None,
        }
    }
}

/// Finds the checkout of `head_branch` of `head_repository` (`owner/name`, lowercase; `None`
/// when unknown) among `dirs`, within `budget`. Runs git on the blocking pool.
pub async fn resolve_checkout(
    dirs: Vec<PathBuf>,
    head_repository: Option<String>,
    head_branch: String,
    budget: Duration,
) -> CheckoutResolution {
    tokio::task::spawn_blocking(move || {
        resolve_blocking(&dirs, head_repository.as_deref(), &head_branch, budget)
    })
    .await
    .unwrap_or(CheckoutResolution::Missing(CheckoutMiss::ReadFailed))
}

/// [`resolve_checkout`] without the pool, for a caller that is already on a blocking thread.
#[must_use]
pub fn resolve_blocking(
    dirs: &[PathBuf],
    head_repository: Option<&str>,
    head_branch: &str,
    budget: Duration,
) -> CheckoutResolution {
    let reader = GitReader::new(Instant::now() + budget);
    resolve_with(&reader, dirs, head_repository, head_branch, &|_| {})
}

/// What one candidate looked like.
enum Inspection {
    Match(Checkout),
    NoMatch,
    Detached,
    ReadFailed,
}

pub(crate) fn resolve_with(
    reader: &GitReader,
    dirs: &[PathBuf],
    head_repository: Option<&str>,
    head_branch: &str,
    between_reads: &dyn Fn(&Path),
) -> CheckoutResolution {
    use CheckoutResolution::Missing;
    let head_repository = head_repository
        .map(|repository| repository.trim().to_ascii_lowercase())
        .filter(|repository| {
            repository
                .split_once('/')
                .is_some_and(|(o, n)| !o.is_empty() && !n.is_empty())
        });
    let Some(head_repository) = head_repository else {
        return Missing(CheckoutMiss::HeadRepositoryUnknown);
    };
    let candidates = match candidates(reader, dirs) {
        Ok(candidates) => candidates,
        Err(miss) => return Missing(miss),
    };
    if candidates.len() > MAX_CHECKOUT_CANDIDATES {
        return Missing(CheckoutMiss::TooManyCandidates);
    }
    let mut matches = Vec::new();
    let (mut detached, mut read_failed) = (false, false);
    for path in &candidates {
        match inspect(reader, path, &head_repository, head_branch, between_reads) {
            Ok(Inspection::Match(checkout)) => matches.push(checkout),
            Ok(Inspection::NoMatch) => {}
            Ok(Inspection::Detached) => detached = true,
            Ok(Inspection::ReadFailed) => read_failed = true,
            Err(miss) => return Missing(miss),
        }
    }
    match matches.len() {
        1 => CheckoutResolution::Found(matches.remove(0)),
        0 if detached => Missing(CheckoutMiss::Detached),
        0 if read_failed => Missing(CheckoutMiss::ReadFailed),
        0 => Missing(CheckoutMiss::NoCheckout),
        _ => Missing(CheckoutMiss::Ambiguous),
    }
}

fn miss_of(error: GitReadError) -> CheckoutMiss {
    match error {
        GitReadError::TimedOut => CheckoutMiss::TimedOut,
        GitReadError::TooLarge | GitReadError::Failed => CheckoutMiss::ReadFailed,
    }
}

/// The text a successful git command printed, or `None` when it exited non-zero.
fn git_text(
    reader: &GitReader,
    dir: &Path,
    arguments: &[&str],
    limit: usize,
) -> Result<Option<String>, CheckoutMiss> {
    let output = reader.run(dir, arguments, limit).map_err(miss_of)?;
    if !output.status.success() {
        return Ok(None);
    }
    String::from_utf8(output.stdout)
        .map(Some)
        .map_err(|_| CheckoutMiss::ReadFailed)
}

/// The canonical checkouts in range, sorted. A directory contributes the checkout it
/// belongs to, and the worktrees of that repository that are located under it.
fn candidates(reader: &GitReader, dirs: &[PathBuf]) -> Result<BTreeSet<PathBuf>, CheckoutMiss> {
    let roots: BTreeSet<PathBuf> = dirs
        .iter()
        .filter_map(|dir| std::fs::canonicalize(dir).ok())
        .filter(|dir| dir.is_dir())
        .collect();
    let mut found = BTreeSet::new();
    for root in &roots {
        let Some(toplevel) =
            git_text(reader, root, &["rev-parse", "--show-toplevel"], SMALL_LIMIT)?
        else {
            continue;
        };
        let Ok(toplevel) = std::fs::canonicalize(toplevel.trim_end_matches('\n')) else {
            continue;
        };
        found.insert(toplevel.clone());
        let Some(list) = git_text(
            reader,
            &toplevel,
            &["worktree", "list", "--porcelain"],
            LIST_LIMIT,
        )?
        else {
            continue;
        };
        for entry in worktrees(&list) {
            if let Ok(path) = std::fs::canonicalize(entry)
                && path.starts_with(root)
            {
                found.insert(path);
            }
        }
        if found.len() > MAX_CHECKOUT_CANDIDATES {
            break;
        }
    }
    Ok(found)
}

/// The paths of the live, non-bare worktrees in `git worktree list --porcelain` output.
/// Quoted paths (unusual characters) are skipped rather than guessed at.
fn worktrees(list: &str) -> Vec<&str> {
    list.split("\n\n")
        .filter_map(|block| {
            let mut path = None;
            for line in block.lines() {
                if let Some(value) = line.strip_prefix("worktree ") {
                    path = Some(value);
                } else if line == "bare" || line.starts_with("prunable") {
                    return None;
                }
            }
            path.filter(|path| !path.starts_with('"'))
        })
        .collect()
}

/// HEAD as read at one moment: the branch it is on (`None` when detached) and its commit.
#[derive(Debug, PartialEq, Eq)]
struct Head {
    branch: Option<String>,
    sha: String,
}

/// `Ok(None)` for a repository without a commit yet.
fn read_head(reader: &GitReader, dir: &Path) -> Result<Option<Head>, CheckoutMiss> {
    let symbolic = reader
        .run(dir, &["symbolic-ref", "-q", "HEAD"], SMALL_LIMIT)
        .map_err(miss_of)?;
    let branch = match symbolic.status.code() {
        Some(0) => {
            let reference = String::from_utf8(symbolic.stdout)
                .map_err(|_| CheckoutMiss::ReadFailed)?
                .trim()
                .to_owned();
            // A HEAD that points somewhere else than a local branch matches no branch name.
            Some(
                reference
                    .strip_prefix("refs/heads/")
                    .map_or_else(|| format!("({reference})"), str::to_owned),
            )
        }
        Some(1) => None,
        _ => return Err(CheckoutMiss::ReadFailed),
    };
    let Some(sha) = git_text(
        reader,
        dir,
        &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
        SMALL_LIMIT,
    )?
    else {
        return Ok(None);
    };
    let sha = sha.trim().to_ascii_lowercase();
    if !matches!(sha.len(), 40 | 64) || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(CheckoutMiss::ReadFailed);
    }
    Ok(Some(Head { branch, sha }))
}

/// The remote (its name) whose fetch URL is the head repository on github.com.
fn matching_remote(
    reader: &GitReader,
    dir: &Path,
    head_repository: &str,
) -> Result<Option<String>, CheckoutMiss> {
    let Some(text) = git_text(reader, dir, &["remote", "-v"], LIST_LIMIT)? else {
        return Err(CheckoutMiss::ReadFailed);
    };
    for line in text.lines() {
        // `<name>\t<url> (fetch)`
        let Some((name, rest)) = line.split_once('\t') else {
            continue;
        };
        let Some(url) = rest.strip_suffix(" (fetch)") else {
            continue;
        };
        if let Some(identity) = parse_remote(url)
            && identity.is_github_com()
            && identity.lowercase_slug() == head_repository
        {
            return Ok(Some(name.to_owned()));
        }
    }
    Ok(None)
}

/// A timeout or a change of the checkout ends the whole resolution (`Err`); any other trouble
/// with one checkout only rules that checkout out.
fn inspect(
    reader: &GitReader,
    path: &Path,
    head_repository: &str,
    head_branch: &str,
    between_reads: &dyn Fn(&Path),
) -> Result<Inspection, CheckoutMiss> {
    let soft = |miss: CheckoutMiss| match miss {
        CheckoutMiss::TimedOut => Err(CheckoutMiss::TimedOut),
        _ => Ok(Inspection::ReadFailed),
    };
    let first = match read_head(reader, path) {
        Ok(Some(head)) => head,
        Ok(None) => return Ok(Inspection::NoMatch),
        Err(miss) => return soft(miss),
    };
    let remote = match matching_remote(reader, path, head_repository) {
        Ok(Some(remote)) => remote,
        Ok(None) => return Ok(Inspection::NoMatch),
        Err(miss) => return soft(miss),
    };
    between_reads(path);
    let second = match read_head(reader, path) {
        Ok(Some(head)) => head,
        Ok(None) => return Err(CheckoutMiss::Changed),
        Err(miss) => return soft(miss),
    };
    if first != second {
        return Err(CheckoutMiss::Changed);
    }
    match first.branch {
        None => Ok(Inspection::Detached),
        Some(branch) if branch == head_branch => Ok(Inspection::Match(Checkout {
            path: path.to_owned(),
            head_sha: first.sha,
            remote,
        })),
        Some(_) => Ok(Inspection::NoMatch),
    }
}

#[cfg(test)]
mod tests;
