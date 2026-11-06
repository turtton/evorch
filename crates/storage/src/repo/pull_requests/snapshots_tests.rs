//! Retention clears the facts of a pull request that no thread references any more. A
//! fetch that was still running at that moment must not write its result back.

use super::{begin_fetch, clear_unshared, commit_failure, commit_success, get};
use crate::Database;
use crate::pull_requests::{
    DisplayTitle, FetchErrorKind, FetchFailure, LinkPullRequest, NewPullRequestCandidate,
    PullRequestDisplay, PullRequestFacts, PullRequestKey, PullRequestState, ReviewDecision,
};
use crate::repo::pull_requests as repo;

fn facts() -> PullRequestFacts {
    PullRequestFacts {
        state: PullRequestState::Open,
        base_branch: "main".into(),
        head_repository: None,
        head_branch: "feat".into(),
        head_sha: "a".repeat(40),
        checks: Vec::new(),
        checks_complete: true,
        review_decision: ReviewDecision::None,
    }
}

fn display(title: &str) -> PullRequestDisplay {
    PullRequestDisplay {
        title: DisplayTitle::from_untrusted(title),
    }
}

fn failure() -> FetchFailure {
    FetchFailure {
        kind: FetchErrorKind::Network,
        message: "offline".into(),
    }
}

#[test]
fn a_fetch_started_before_the_facts_were_cleared_cannot_write_them_back() {
    let database = Database::open_in_memory().unwrap();
    let conn = &database.conn;
    let with_facts = PullRequestKey::github("o/with-facts", 1).unwrap();
    let first_fetch = PullRequestKey::github("o/first-fetch", 2).unwrap();

    // Given: thread "gone" is the only one that references both pull requests. The first has
    // stored facts and two more fetches in flight (one will succeed, one will fail); the
    // second has only its very first fetch in flight, so it holds no facts yet.
    repo::link(
        conn,
        &LinkPullRequest {
            thread_id: "gone".into(),
            key: with_facts.clone(),
            expected_revision: 0,
            at_ns: 1,
        },
    )
    .unwrap();
    repo::present_candidate(
        conn,
        &NewPullRequestCandidate {
            thread_id: "gone".into(),
            key: first_fetch.clone(),
            source_run_id: "run".into(),
            source_call_id: "call".into(),
            at_ns: 1,
        },
    )
    .unwrap();
    let done = begin_fetch(conn, &with_facts).unwrap();
    commit_success(
        conn,
        &with_facts,
        done,
        &facts(),
        &display("Add the title"),
        5,
    )
    .unwrap();
    let running_success = begin_fetch(conn, &with_facts).unwrap();
    let running_failure = begin_fetch(conn, &with_facts).unwrap();
    let running_first = begin_fetch(conn, &first_fetch).unwrap();

    assert_eq!(
        get(conn, &with_facts)
            .unwrap()
            .unwrap()
            .display
            .title
            .unwrap()
            .as_str(),
        "Add the title"
    );

    // When: retention clears what only the deleted thread referenced.
    assert_eq!(clear_unshared(conn, "gone", 100).unwrap(), 2);
    assert_eq!(clear_unshared(conn, "gone", 100).unwrap(), 0, "idempotent");

    // Then: the late results are refused, whether they succeed or fail, with or without
    // earlier facts.
    for commit in [
        commit_success(
            conn,
            &with_facts,
            running_success,
            &facts(),
            &display("late"),
            10,
        )
        .unwrap(),
        commit_failure(conn, &with_facts, running_failure, &failure(), 11).unwrap(),
        commit_success(
            conn,
            &first_fetch,
            running_first,
            &facts(),
            &display("late"),
            12,
        )
        .unwrap(),
        commit_failure(conn, &first_fetch, running_first, &failure(), 13).unwrap(),
    ] {
        assert!(
            !commit.facts_stored && !commit.attempt_recorded,
            "{commit:?}"
        );
    }
    for key in [&with_facts, &first_fetch] {
        let snapshot = get(conn, key).unwrap().unwrap();
        assert!(snapshot.facts.is_none(), "{key:?}");
        assert!(snapshot.display.title.is_none(), "{key:?}");
        assert!(snapshot.last_attempt.is_none(), "{key:?}");
    }

    // And: a fetch that starts afterwards outranks the cleared ones and is stored.
    let later = begin_fetch(conn, &with_facts).unwrap();
    assert!(later > running_failure);
    let commit = commit_success(conn, &with_facts, later, &facts(), &display("after"), 20).unwrap();
    assert!(commit.facts_stored && commit.attempt_recorded);
    let stored = get(conn, &with_facts).unwrap().unwrap();
    assert!(stored.facts.is_some());
    assert_eq!(stored.display.title.unwrap().as_str(), "after");
}
