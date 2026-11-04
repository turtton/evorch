use std::path::{Path, PathBuf};
use std::process::Command;

use super::*;

const AUTHOR: [&str; 4] = ["-c", "user.name=t", "-c", "user.email=t@example.com"];

/// Git for building fixtures: no global or system configuration.
fn git(dir: &Path, arguments: &[&str]) -> String {
    let mut command = Command::new("git");
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("GIT_") {
            command.env_remove(name);
        }
    }
    let output = command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(AUTHOR)
        .args(arguments)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

struct Fixture {
    dir: tempfile::TempDir,
    global_config: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let global_config = dir.path().join("global-config");
        std::fs::write(&global_config, "").unwrap();
        Self { dir, global_config }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// A repository with one commit on `branch` and `origin` set to `origin_url`.
    fn repo(&self, name: &str, branch: &str, origin_url: &str) -> PathBuf {
        let root = self.path(name);
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "--quiet", "--initial-branch", branch]);
        std::fs::write(root.join("file.txt"), name).unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--quiet", "-m", "initial"]);
        git(&root, &["remote", "add", "origin", origin_url]);
        std::fs::canonicalize(root).unwrap()
    }

    fn reader(&self) -> GitReader {
        GitReader::new(Instant::now() + Duration::from_secs(60))
            .with_global_config(self.global_config.clone())
    }

    fn resolve(
        &self,
        dirs: &[&Path],
        head_repository: Option<&str>,
        branch: &str,
    ) -> CheckoutResolution {
        let dirs: Vec<PathBuf> = dirs.iter().map(|dir| dir.to_path_buf()).collect();
        resolve_with(&self.reader(), &dirs, head_repository, branch, &|_, _| {})
    }
}

fn found(resolution: CheckoutResolution) -> Checkout {
    match resolution {
        CheckoutResolution::Found(checkout) => checkout,
        CheckoutResolution::Missing(miss) => panic!("expected a checkout, got {miss:?}"),
    }
}

fn missing(resolution: &CheckoutResolution) -> CheckoutMiss {
    match resolution {
        CheckoutResolution::Missing(miss) => *miss,
        CheckoutResolution::Found(checkout) => panic!("expected no checkout, got {checkout:?}"),
    }
}

const ORIGIN: &str = "https://github.com/turtton/evorch.git";

// Given: a checkout of the head branch / When: resolved / Then: its HEAD commit
#[test]
fn a_single_matching_checkout_gives_its_head_commit() {
    let f = Fixture::new();
    let repo = f.repo("project", "feat/x", ORIGIN);
    let head = git(&repo, &["rev-parse", "HEAD"]);

    let checkout = found(f.resolve(&[&repo], Some("turtton/evorch"), "feat/x"));

    assert_eq!(checkout.head_sha, head);
    assert_eq!(checkout.path, repo);
    assert_eq!(checkout.remote, "origin");
    assert_eq!(
        f.resolve(&[&repo], Some("turtton/evorch"), "feat/x")
            .head_sha(),
        Some(head.as_str())
    );
}

// Given: the project root's subdirectory is the allowed directory / When: resolved / Then: the
// checkout it belongs to
#[test]
fn a_subdirectory_resolves_to_its_checkout() {
    let f = Fixture::new();
    let repo = f.repo("project", "feat/x", ORIGIN);
    std::fs::create_dir_all(repo.join("crates/inner")).unwrap();

    let checkout = found(f.resolve(
        &[&repo.join("crates/inner")],
        Some("turtton/evorch"),
        "feat/x",
    ));

    assert_eq!(checkout.path, repo);
}

// Given: a fork's pull request and a checkout whose origin is the base repository, with the
// fork under another remote name / When: resolved / Then: every remote is compared
#[test]
fn any_remote_may_name_the_head_repository() {
    let f = Fixture::new();
    let repo = f.repo("project", "feat/x", ORIGIN);
    git(
        &repo,
        &["remote", "add", "alice", "git@github.com:Alice/evorch.git"],
    );

    let checkout = found(f.resolve(&[&repo], Some("alice/evorch"), "feat/x"));

    assert_eq!(checkout.remote, "alice");
    // The base repository's own pull request matches through origin.
    assert_eq!(
        found(f.resolve(&[&repo], Some("turtton/evorch"), "feat/x")).remote,
        "origin"
    );
}

// Given: checkouts that do not match / When: resolved / Then: none, with the reason
#[test]
fn checkouts_of_other_repositories_hosts_or_branches_do_not_match() {
    let f = Fixture::new();
    let repo = f.repo("project", "feat/x", ORIGIN);
    let enterprise = f.repo("ghe", "feat/x", "git@ghe.example.com:turtton/evorch.git");

    for (dirs, repository, branch) in [
        (vec![&repo], "turtton/other", "feat/x"),
        (vec![&repo], "turtton/evorch", "feat/y"),
        (vec![&repo], "turtton/evorch", "feat"),
        (vec![&enterprise], "turtton/evorch", "feat/x"),
    ] {
        let resolution = f.resolve(
            &dirs.iter().map(|d| d.as_path()).collect::<Vec<_>>(),
            Some(repository),
            branch,
        );
        assert_eq!(
            missing(&resolution),
            CheckoutMiss::NoCheckout,
            "{repository} {branch}"
        );
        assert_eq!(resolution.head_sha(), None);
    }
}

// Given: a pull request whose head repository is unknown / When: resolved / Then: none, and git
// is not even started
#[test]
fn an_unknown_head_repository_matches_nothing() {
    let f = Fixture::new();
    let repo = f.repo("project", "feat/x", ORIGIN);
    // A reader whose deadline passed would fail any git call with TimedOut.
    let expired = GitReader::new(Instant::now()).with_global_config(f.global_config.clone());

    for repository in [None, Some(""), Some("  "), Some("no-slash"), Some("/name")] {
        let resolution = resolve_with(
            &expired,
            std::slice::from_ref(&repo),
            repository,
            "feat/x",
            &|_, _| {},
        );
        assert_eq!(
            missing(&resolution),
            CheckoutMiss::HeadRepositoryUnknown,
            "{repository:?}"
        );
    }
}

// Given: a detached HEAD / When: resolved / Then: Detached, never a guess
#[test]
fn a_detached_head_is_not_named() {
    let f = Fixture::new();
    let repo = f.repo("project", "feat/x", ORIGIN);
    git(&repo, &["checkout", "--quiet", "--detach"]);

    let resolution = f.resolve(&[&repo], Some("turtton/evorch"), "feat/x");

    assert_eq!(missing(&resolution), CheckoutMiss::Detached);
}

// Given: a detached checkout and a checkout on the branch / When: resolved / Then: the one
// on the branch
#[test]
fn a_detached_checkout_does_not_hide_a_unique_match() {
    let f = Fixture::new();
    let detached = f.repo("a-detached", "feat/x", ORIGIN);
    git(&detached, &["checkout", "--quiet", "--detach"]);
    let on_branch = f.repo("b-branch", "feat/x", ORIGIN);

    let checkout = found(f.resolve(&[&detached, &on_branch], Some("turtton/evorch"), "feat/x"));

    assert_eq!(checkout.path, on_branch);
}

// Given: two clones on the same branch / When: resolved / Then: Ambiguous
#[test]
fn several_matching_checkouts_are_ambiguous() {
    let f = Fixture::new();
    let first = f.repo("first", "feat/x", ORIGIN);
    let second = f.repo("second", "feat/x", "git@github.com:turtton/evorch.git");

    let both = f.resolve(&[&first, &second], Some("turtton/evorch"), "feat/x");
    let one = f.resolve(&[&first], Some("turtton/evorch"), "feat/x");

    assert_eq!(missing(&both), CheckoutMiss::Ambiguous);
    assert_eq!(found(one).path, first);
}

// Given: a directory listed twice, through a symlink and as a subdirectory / When: resolved /
// Then: one checkout, not an ambiguity
#[cfg(unix)]
#[test]
fn a_checkout_reached_in_several_ways_is_one_candidate() {
    let f = Fixture::new();
    let repo = f.repo("project", "feat/x", ORIGIN);
    std::fs::create_dir(repo.join("sub")).unwrap();
    let link = f.path("link-to-project");
    std::os::unix::fs::symlink(&repo, &link).unwrap();

    let checkout = found(f.resolve(
        &[&repo, &repo, &link, &repo.join("sub"), &repo.join("sub/..")],
        Some("turtton/evorch"),
        "feat/x",
    ));

    assert_eq!(checkout.path, repo);
}

// Given: a linked worktree under the project root and one outside the given directories /
// When: resolved / Then: only the one in range is seen
#[test]
fn worktrees_are_found_only_inside_the_given_directories() {
    let f = Fixture::new();
    let repo = f.repo("project", "main", ORIGIN);
    let inside = repo.join(".worktrees/inside");
    let outside = f.path("outside-worktree");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "feat/in",
            inside.to_str().unwrap(),
        ],
    );
    git(
        &repo,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "feat/out",
            outside.to_str().unwrap(),
        ],
    );
    let inside = std::fs::canonicalize(inside).unwrap();
    let outside = std::fs::canonicalize(outside).unwrap();

    let in_range = found(f.resolve(&[&repo], Some("turtton/evorch"), "feat/in"));
    let out_of_range = f.resolve(&[&repo], Some("turtton/evorch"), "feat/out");
    let given_directly = found(f.resolve(&[&outside], Some("turtton/evorch"), "feat/out"));

    assert_eq!(in_range.path, inside);
    assert_eq!(missing(&out_of_range), CheckoutMiss::NoCheckout);
    assert_eq!(given_directly.path, outside);
    // The main checkout is a candidate too (it is the repository the directory belongs to).
    assert_eq!(
        found(f.resolve(&[&repo], Some("turtton/evorch"), "main")).path,
        repo
    );
    // A linked worktree given as the directory sees its repository's worktrees under it only.
    assert_eq!(
        missing(&f.resolve(&[&inside], Some("turtton/evorch"), "main")),
        CheckoutMiss::NoCheckout
    );
}

// Given: directories that are missing, plain, or files / When: resolved / Then: ignored
#[test]
fn directories_without_a_checkout_are_ignored() {
    let f = Fixture::new();
    let plain = f.path("plain");
    std::fs::create_dir(&plain).unwrap();
    std::fs::write(f.path("file.txt"), "x").unwrap();
    let repo = f.repo("project", "feat/x", ORIGIN);
    let dirs = [plain.clone(), f.path("missing"), f.path("file.txt")];

    let none = resolve_with(
        &f.reader(),
        &dirs,
        Some("turtton/evorch"),
        "feat/x",
        &|_, _| {},
    );
    let mixed = f.resolve(
        &[&plain, &f.path("missing"), &repo],
        Some("turtton/evorch"),
        "feat/x",
    );

    assert_eq!(missing(&none), CheckoutMiss::NoCheckout);
    assert_eq!(found(mixed).path, repo);
    assert_eq!(
        missing(&f.resolve(&[], Some("turtton/evorch"), "feat/x")),
        CheckoutMiss::NoCheckout
    );
}

// Given: the branch switches while the checkout is being read / When: resolved / Then: Changed
#[test]
fn a_branch_switch_during_the_read_gives_no_commit() {
    let f = Fixture::new();
    let repo = f.repo("project", "feat/x", ORIGIN);
    git(&repo, &["branch", "other"]);
    let switch = |stage: Stage, path: &Path| {
        if stage == Stage::BetweenReads {
            git(path, &["checkout", "--quiet", "other"]);
        }
    };

    let resolution = resolve_with(
        &f.reader(),
        std::slice::from_ref(&repo),
        Some("turtton/evorch"),
        "feat/x",
        &switch,
    );

    assert_eq!(missing(&resolution), CheckoutMiss::Changed);
}

// Given: a new commit lands between the two reads / When: resolved / Then: Changed as well
#[test]
fn a_new_commit_during_the_read_gives_no_commit() {
    let f = Fixture::new();
    let repo = f.repo("project", "feat/x", ORIGIN);
    let commit = |stage: Stage, path: &Path| {
        if stage == Stage::BetweenReads {
            git(path, &["commit", "--quiet", "--allow-empty", "-m", "more"]);
        }
    };

    let resolution = resolve_with(
        &f.reader(),
        std::slice::from_ref(&repo),
        Some("turtton/evorch"),
        "feat/x",
        &commit,
    );

    assert_eq!(missing(&resolution), CheckoutMiss::Changed);
}

// Given: one checkout that matches and one that cannot be read / When: resolved / Then: no
// commit, because the unreadable one may be a second match
#[test]
fn an_unreadable_candidate_means_no_commit_even_when_another_one_matches() {
    let f = Fixture::new();
    let good = f.repo("a-good", "feat/x", ORIGIN);
    let broken = f.repo("b-broken", "feat/x", ORIGIN);
    let dirs = [good.clone(), broken.clone()];
    let corrupt = |stage: Stage, path: &Path| {
        if stage == Stage::BeforeRead && path.ends_with("b-broken") {
            std::fs::rename(path.join(".git"), path.join("git-gone")).unwrap();
        }
    };

    let resolution = resolve_with(
        &f.reader(),
        &dirs,
        Some("turtton/evorch"),
        "feat/x",
        &corrupt,
    );

    assert_eq!(missing(&resolution), CheckoutMiss::ReadFailed);
    // The undamaged checkout alone is a clean match.
    assert_eq!(
        found(f.resolve(&[&good], Some("turtton/evorch"), "feat/x")).path,
        good
    );
}

// Given: more checkouts than the limit / When: resolved / Then: TooManyCandidates, not a guess
#[test]
fn too_many_checkouts_are_not_guessed_at() {
    let f = Fixture::new();
    let repo = f.repo("project", "main", ORIGIN);
    for index in 0..=MAX_CHECKOUT_CANDIDATES {
        let path = repo.join(format!(".worktrees/w{index}"));
        git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                path.to_str().unwrap(),
            ],
        );
    }

    let resolution = f.resolve(&[&repo], Some("turtton/evorch"), "main");

    assert_eq!(missing(&resolution), CheckoutMiss::TooManyCandidates);
}

// Given: a deadline that has passed / When: resolved / Then: TimedOut
#[test]
fn an_expired_deadline_is_reported() {
    let f = Fixture::new();
    let repo = f.repo("project", "feat/x", ORIGIN);
    let expired = GitReader::new(Instant::now()).with_global_config(f.global_config.clone());

    let resolution = resolve_with(
        &expired,
        std::slice::from_ref(&repo),
        Some("turtton/evorch"),
        "feat/x",
        &|_, _| {},
    );

    assert_eq!(missing(&resolution), CheckoutMiss::TimedOut);
}

// Given: the async entry point / When: awaited / Then: the same answer, from the blocking pool
#[tokio::test]
async fn the_async_entry_point_resolves_on_the_blocking_pool() {
    let f = Fixture::new();
    let repo = f.repo("project", "feat/x", ORIGIN);
    let head = git(&repo, &["rev-parse", "HEAD"]);

    let resolution = resolve_checkout(
        vec![repo.clone()],
        Some("turtton/evorch".to_owned()),
        "feat/x".to_owned(),
        Duration::from_secs(60),
    )
    .await;

    // The user's own git configuration may apply here; the answer must not depend on it.
    assert_eq!(resolution.head_sha(), Some(head.as_str()));
}

// Given: the shared git reader / When: used through the blocking helper / Then: it runs and
// reports a failing command by its exit status
#[tokio::test]
async fn the_shared_reader_runs_on_the_blocking_pool() {
    let f = Fixture::new();
    let repo = f.repo("project", "main", ORIGIN);

    let ok = f
        .reader()
        .run_blocking(repo.clone(), vec!["rev-parse".into(), "HEAD".into()], 1024)
        .await
        .unwrap();
    let failed = f
        .reader()
        .run_blocking(
            repo,
            vec!["rev-parse".into(), "--verify".into(), "nope".into()],
            1024,
        )
        .await
        .unwrap();

    assert!(ok.status.success() && failed.status.code() != Some(0));
}
