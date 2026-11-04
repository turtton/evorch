//! The repository a working directory belongs to, as its `origin` remote names it.
//!
//! Two uses with different strictness:
//!
//! - **Display** ([`derive_repo_slug`], [`parse_remote_slug`]): an `owner/name` label for a
//!   project. It falls back to the directory name when the remote is missing or not
//!   understood, so the label always exists. It must not be used to decide anything.
//! - **Identity** ([`parse_remote`]): the structured host, owner and name of a remote, for
//!   matching a pull request's repository with a checkout. It never falls back: a remote it
//!   does not understand has no identity.
//!
//! SSH remotes are only parsed; nothing here connects to a remote.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::git_read::GitReader;

const REMOTE_READ_DEADLINE: Duration = Duration::from_secs(5);
const REMOTE_READ_LIMIT: usize = 4096;

/// The host and `owner/name` of a git remote, normalized from its HTTPS, `ssh://`,
/// `git://` or scp-like (`git@host:owner/name`) spelling. A trailing `.git` or `/`, the
/// user name, the password and the port are not part of the identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemoteIdentity {
    /// The host, lowercase. A different spelling of the same service (for example
    /// `ssh.github.com`) is a different host: nothing is guessed.
    pub host: String,
    /// The owner (organization or user), as written.
    pub owner: String,
    /// The repository name without `.git`, as written.
    pub name: String,
}

impl RemoteIdentity {
    /// `owner/name` as written in the remote.
    #[must_use]
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// `owner/name` in lowercase, the form pull request keys use.
    #[must_use]
    pub fn lowercase_slug(&self) -> String {
        self.slug().to_ascii_lowercase()
    }

    /// Whether the remote is on github.com itself (not Enterprise, not a lookalike).
    #[must_use]
    pub fn is_github_com(&self) -> bool {
        self.host == "github.com"
    }
}

/// Parses a remote URL into its identity. `None` for anything that is not a single
/// `owner/name` repository on a named host (a local path, a nested group path, an
/// unknown scheme, a malformed authority, whitespace or control characters).
#[must_use]
pub fn parse_remote(remote: &str) -> Option<RemoteIdentity> {
    let remote = remote.trim();
    if remote.is_empty()
        || remote
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
    {
        return None;
    }
    let (authority, path) = match remote.split_once("://") {
        Some((scheme, rest)) => {
            if !matches!(
                scheme.to_ascii_lowercase().as_str(),
                "https" | "http" | "ssh" | "git" | "git+ssh" | "ssh+git"
            ) {
                return None;
            }
            rest.split_once('/')?
        }
        None => {
            let (before, path) = remote.split_once(':')?;
            if before.contains('/') {
                return None;
            }
            (before, path)
        }
    };
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = match host_port.rsplit_once(':') {
        Some((host, port)) if remote.contains("://") => {
            if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            host
        }
        _ => host_port,
    };
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    {
        return None;
    }
    let path = path.trim_start_matches('/');
    if path.contains(['?', '#']) {
        return None;
    }
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut segments = path.split('/');
    let (owner, name) = (segments.next()?, segments.next()?);
    if segments.next().is_some() || owner.is_empty() || name.is_empty() {
        return None;
    }
    Some(RemoteIdentity {
        host: host.to_ascii_lowercase(),
        owner: owner.to_owned(),
        name: name.to_owned(),
    })
}

/// The `owner/name` label of the repository at `repo_root`, from `git remote get-url
/// origin`. Falls back to the directory name when that fails. For display only.
#[must_use]
pub fn derive_repo_slug(repo_root: &Path) -> String {
    origin_url(repo_root)
        .as_deref()
        .and_then(parse_remote_slug)
        .unwrap_or_else(|| fallback_slug(repo_root))
}

fn origin_url(repo_root: &Path) -> Option<String> {
    let output = GitReader::new(Instant::now() + REMOTE_READ_DEADLINE)
        .run(
            repo_root,
            &["remote", "get-url", "origin"],
            REMOTE_READ_LIMIT,
        )
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// `owner/name` from an origin URL (https / ssh / scp-like), for display. It does not look
/// at the host and keeps the case as written.
#[must_use]
pub fn parse_remote_slug(remote: &str) -> Option<String> {
    let trimmed = remote.trim().trim_end_matches('/');
    let without_suffix = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    if let Some(rest) = without_suffix.strip_prefix("git@") {
        let (_host, path) = rest.split_once(':')?;
        return slug_from_path(path);
    }
    if let Some((_scheme, remainder)) = without_suffix.split_once("://") {
        let path = remainder.split_once('/').map(|(_, path)| path)?;
        return slug_from_path(path);
    }
    slug_from_path(without_suffix)
}

fn slug_from_path(path: &str) -> Option<String> {
    let mut segments = path.split('/').filter(|segment| !segment.is_empty());
    let owner = segments.next()?;
    let name = segments.next()?;
    segments.next().is_none().then(|| format!("{owner}/{name}"))
}

fn fallback_slug(repo_root: &Path) -> String {
    repo_root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| String::from("unknown"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(host: &str, owner: &str, name: &str) -> Option<RemoteIdentity> {
        Some(RemoteIdentity {
            host: host.into(),
            owner: owner.into(),
            name: name.into(),
        })
    }

    // Given: the spellings of one GitHub remote / When: parsed / Then: one identity
    #[test]
    fn every_spelling_of_a_remote_has_the_same_identity() {
        let expected = identity("github.com", "turtton", "evorch");
        for remote in [
            "https://github.com/turtton/evorch",
            "https://github.com/turtton/evorch.git",
            "https://github.com/turtton/evorch/",
            "https://github.com/turtton/evorch.git/",
            "HTTPS://GitHub.com/turtton/evorch.git",
            "https://user:secret@github.com/turtton/evorch.git",
            "https://github.com:443/turtton/evorch.git",
            "git@github.com:turtton/evorch.git",
            "github.com:turtton/evorch",
            "org-123@github.com:turtton/evorch.git",
            "git@github.com:/turtton/evorch.git",
            "ssh://git@github.com/turtton/evorch",
            "ssh://git@github.com:22/turtton/evorch.git",
            "git://github.com/turtton/evorch.git",
            "  https://github.com/turtton/evorch.git\n",
        ] {
            assert_eq!(parse_remote(remote), expected, "{remote}");
        }
    }

    // Given: other hosts and malformed remotes / When: parsed / Then: no guess is made
    #[test]
    fn hosts_are_kept_apart_and_unparseable_remotes_have_no_identity() {
        assert_eq!(
            parse_remote("git@ghe.example.com:team/app.git"),
            identity("ghe.example.com", "team", "app")
        );
        assert_eq!(
            parse_remote("ssh://git@ssh.github.com:443/turtton/evorch.git"),
            identity("ssh.github.com", "turtton", "evorch"),
            "another name for the service is another host"
        );
        assert!(
            !parse_remote("ssh://git@ssh.github.com/o/r")
                .unwrap()
                .is_github_com()
        );
        assert!(
            parse_remote("https://github.com/o/r")
                .unwrap()
                .is_github_com()
        );
        assert!(
            !parse_remote("https://github.com.evil.com/o/r")
                .unwrap()
                .is_github_com()
        );
        for remote in [
            "",
            "not a remote",
            "/srv/git/repo.git",
            "./repo",
            "file:///srv/git/repo.git",
            "ftp://github.com/o/r",
            "https://github.com/only",
            "https://github.com/o/r/extra",
            "https://gitlab.com/group/sub/repo.git",
            "https://github.com:port/o/r",
            "https://github.com/o/r?x=1",
            "https://github.com\\@evil.com/o/r",
            "C:\\work\\repo",
            "https:///o/r",
        ] {
            assert_eq!(parse_remote(remote), None, "{remote:?}");
        }
    }

    #[test]
    fn slugs_keep_the_case_the_remote_was_written_in() {
        let parsed = parse_remote("https://github.com/Turtton/EvOrch.git").unwrap();
        assert_eq!(parsed.slug(), "Turtton/EvOrch");
        assert_eq!(parsed.lowercase_slug(), "turtton/evorch");
    }

    // Given: origin URL 由来のリポジトリ識別子が必要なとき
    // When: parse_remote_slug する
    // Then: https / ssh 両形式から owner/name を抽出し、解釈不能なら None を返す
    #[test]
    fn parse_remote_slug_extracts_owner_and_name() {
        assert_eq!(
            parse_remote_slug("https://github.com/turtton/evorch.git").as_deref(),
            Some("turtton/evorch")
        );
        assert_eq!(
            parse_remote_slug("git@github.com:turtton/evorch.git").as_deref(),
            Some("turtton/evorch")
        );
        assert_eq!(
            parse_remote_slug("ssh://git@github.com/turtton/evorch").as_deref(),
            Some("turtton/evorch")
        );
        assert_eq!(parse_remote_slug("not a remote"), None);
        assert_eq!(parse_remote_slug("https://github.com/only"), None);
    }

    // Given: a directory that is not a repository, or has no origin / When: its label is
    // derived / Then: the directory name is the fallback, which a pull request never uses
    #[test]
    fn the_display_slug_falls_back_to_the_directory_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("my-project");
        std::fs::create_dir(&root).unwrap();
        assert_eq!(derive_repo_slug(&root), "my-project");
        assert_eq!(parse_remote("my-project"), None);
    }
}
