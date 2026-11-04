//! Pull request integration storage (ADR 0043 Phase 1): links, candidates, inheritance,
//! snapshots, notification keys, retention fences and restart behaviour.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Barrier;

use rusqlite::Connection;
use storage::pull_requests::{
    CandidateState, CheckConclusion, CheckKind, CheckStatus, FetchErrorKind, FetchFailure,
    InheritancePending, InheritanceReason, InheritanceResolution, InheritanceSkipReason,
    InheritanceState, LinkEndReason, LinkOrigin, LinkPullRequest, NewPullRequestCandidate,
    NewPullRequestInheritance, NotificationKey, PULL_REQUEST_CANDIDATES_MAX,
    PULL_REQUEST_CHECKS_MAX, PULL_REQUEST_NOTIFIED_MAX, PullRequestCheck, PullRequestFacts,
    PullRequestKey, PullRequestRejection, PullRequestState, ReviewDecision, sanitize_single_line,
};
use storage::{Storage, StorageConfig, StorageError, StorageHandle};

struct Fixture {
    _dir: tempfile::TempDir,
    config: StorageConfig,
    storage: Storage,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("pull-requests.db"),
            ..StorageConfig::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        Self {
            _dir: dir,
            config,
            storage,
        }
    }

    fn handle(&self) -> StorageHandle {
        self.storage.handle()
    }

    fn sql(&self) -> Connection {
        let conn = Connection::open(&self.config.db_path).unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        conn
    }

    fn tombstone(&self, thread: &str) {
        self.sql()
            .execute(
                "INSERT INTO thread_tombstones(thread_id, family_key, deleted_at_ns) \
                 VALUES (?1, 'family', 1)",
                [thread],
            )
            .unwrap();
    }

    fn thread(&self, thread: &str, lineage_kind: Option<&str>, archived: bool) {
        self.sql()
            .execute(
                "INSERT INTO threads(thread_id, lineage_kind, archived, created_at_ns, \
                 updated_at_ns, source) VALUES (?1, ?2, ?3, 1, 1, 'runtime')",
                rusqlite::params![thread, lineage_kind, archived],
            )
            .unwrap();
    }
}

fn key(number: u64) -> PullRequestKey {
    PullRequestKey::github("Owner/Repo", number).unwrap()
}

fn sha(c: char) -> String {
    c.to_string().repeat(40)
}

fn check(
    id: &str,
    name: &str,
    status: CheckStatus,
    conclusion: Option<CheckConclusion>,
) -> PullRequestCheck {
    PullRequestCheck {
        provider_id: id.into(),
        kind: CheckKind::CheckRun,
        name: name.into(),
        status,
        conclusion,
        head_sha: sha('a'),
    }
}

fn facts(state: PullRequestState) -> PullRequestFacts {
    PullRequestFacts {
        state,
        base_branch: "main".into(),
        head_repository: Some("Fork/Repo".into()),
        head_branch: "feat/x".into(),
        head_sha: sha('a'),
        checks: vec![check(
            "1",
            "ci / build",
            CheckStatus::Completed,
            Some(CheckConclusion::Success),
        )],
        checks_complete: true,
        review_decision: ReviewDecision::ReviewRequired,
    }
}

fn link(
    h: &StorageHandle,
    thread: &str,
    number: u64,
    expected: u64,
) -> storage::pull_requests::LinkReceipt {
    h.link_pull_request(LinkPullRequest {
        thread_id: thread.into(),
        key: key(number),
        expected_revision: expected,
        at_ns: 100 + i64::try_from(expected).unwrap(),
    })
    .unwrap()
}

fn fetch_open(h: &StorageHandle, number: u64, state: PullRequestState) -> u64 {
    let seq = h.begin_pull_request_fetch(&key(number)).unwrap();
    h.commit_pull_request_fetch(&key(number), seq, facts(state), 500)
        .unwrap();
    seq
}

fn rejection(error: &StorageError) -> &PullRequestRejection {
    match error {
        StorageError::PullRequest(rejection) => rejection,
        other => panic!("not a pull request rejection: {other:?}"),
    }
}

#[test]
fn a_relink_ends_the_old_row_and_creates_the_new_one_together() {
    let f = Fixture::new();
    let h = f.handle();

    // Given: an explicit link.
    let first = link(&h, "t1", 1, 0);
    assert_eq!(first.lifecycle_revision, 1);
    assert!(first.changed && first.superseded.is_none());
    assert_eq!(first.link.origin, LinkOrigin::Explicit);

    // When: the thread is linked to another pull request, from the revision it read.
    let second = link(&h, "t1", 2, 1);

    // Then: the old row is superseded as relinked at the new revision and exactly one
    // row is active.
    let ended = second.superseded.expect("the first link ended");
    assert_eq!(ended.link_id, first.link.link_id);
    let end = ended.end.expect("ended");
    assert_eq!(end.reason, LinkEndReason::Relinked);
    assert_eq!(end.revision, 2);
    assert_eq!(second.link.link_revision, 2);
    let state = h.pull_request_thread_state("t1").unwrap();
    assert_eq!(state.lifecycle_revision, 2);
    assert_eq!(state.active_link.unwrap().key, key(2));
    let active: i64 = f
        .sql()
        .query_row(
            "SELECT COUNT(*) FROM thread_pull_requests WHERE thread_id = 't1' \
             AND superseded_at_ns IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(active, 1);

    // And: linking the pull request it is already explicitly linked to changes nothing.
    let again = link(&h, "t1", 2, 2);
    assert!(!again.changed);
    assert_eq!(again.lifecycle_revision, 2);
    assert_eq!(h.pull_request_link_history("t1", 10).unwrap().len(), 2);
}

#[test]
fn a_failed_relink_leaves_the_active_link_untouched() {
    let f = Fixture::new();
    let h = f.handle();
    link(&h, "t1", 1, 0);

    // When: a relink with a malformed key is refused.
    let mut bad = key(2);
    bad.repository = "not-a-repository".into();
    let error = h
        .link_pull_request(LinkPullRequest {
            thread_id: "t1".into(),
            key: bad,
            expected_revision: 1,
            at_ns: 200,
        })
        .unwrap_err();

    // Then: nothing changed.
    assert!(matches!(
        rejection(&error),
        PullRequestRejection::Invalid {
            field: "repository",
            ..
        }
    ));
    let state = h.pull_request_thread_state("t1").unwrap();
    assert_eq!(state.lifecycle_revision, 1);
    assert_eq!(state.active_link.unwrap().key, key(1));
}

#[test]
fn keys_are_normalized_and_only_github_com_is_accepted() {
    let normalized = PullRequestKey::github("  Owner/Repo ", 7).unwrap();
    assert_eq!(normalized.repository, "owner/repo");
    assert_eq!(normalized.host, "github.com");
    let mut other_host = normalized.clone();
    other_host.host = "ghe.example.com".into();
    assert!(other_host.normalized().is_err());
    assert!(PullRequestKey::github("owner/repo", 0).is_err());
    assert!(PullRequestKey::github("owner/repo/extra", 1).is_err());
    assert!(PullRequestKey::github("owner", 1).is_err());
}

#[test]
fn a_stale_unlink_cannot_erase_a_newer_link() {
    let f = Fixture::new();
    let h = f.handle();

    // Given: window A read revision 1; window B then unlinked and linked again.
    link(&h, "t1", 1, 0);
    let window_a_revision = h
        .pull_request_thread_state("t1")
        .unwrap()
        .lifecycle_revision;
    let unlinked = h.unlink_pull_request("t1", 1, 200).unwrap();
    assert_eq!(unlinked.lifecycle_revision, 2);
    assert_eq!(
        unlinked.ended.unwrap().end.unwrap().reason,
        LinkEndReason::Unlinked
    );
    link(&h, "t1", 3, 2);

    // When: window A unlinks from its stale revision.
    let error = h
        .unlink_pull_request("t1", window_a_revision, 300)
        .unwrap_err();

    // Then: it is refused and the newer link stays.
    assert!(matches!(
        error,
        StorageError::StaleRevision {
            expected: 1,
            current: 3,
            ..
        }
    ));
    assert_eq!(
        h.pull_request_thread_state("t1")
            .unwrap()
            .active_link
            .unwrap()
            .key,
        key(3)
    );
    // A stale link request is refused the same way.
    let stale_link = h
        .link_pull_request(LinkPullRequest {
            thread_id: "t1".into(),
            key: key(4),
            expected_revision: 2,
            at_ns: 400,
        })
        .unwrap_err();
    assert!(matches!(stale_link, StorageError::StaleRevision { .. }));
}

#[test]
fn unlink_keeps_a_history_and_needs_an_active_link() {
    let f = Fixture::new();
    let h = f.handle();
    assert!(matches!(
        rejection(&h.unlink_pull_request("t1", 0, 1).unwrap_err()),
        PullRequestRejection::NotLinked
    ));
    link(&h, "t1", 1, 0);
    h.unlink_pull_request("t1", 1, 200).unwrap();
    assert!(matches!(
        rejection(&h.unlink_pull_request("t1", 2, 201).unwrap_err()),
        PullRequestRejection::NotLinked
    ));
    let state = h.pull_request_thread_state("t1").unwrap();
    assert!(state.active_link.is_none());
    assert_eq!(
        state.lifecycle_revision, 2,
        "the revision outlives the link"
    );
    let history = h.pull_request_link_history("t1", 10).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].end.unwrap().reason, LinkEndReason::Unlinked);
}

#[test]
fn an_explicit_decision_of_the_child_cancels_its_pending_inheritance_for_good() {
    let f = Fixture::new();
    let h = f.handle();
    let parent = link(&h, "parent", 1, 0);
    let seq = fetch_open(&h, 1, PullRequestState::Open);

    for (child, decision) in [("child-unlink", "unlink"), ("child-link", "link")] {
        // Given: a derived thread with a pending inheritance.
        let scheduled = h
            .schedule_pull_request_inheritance(NewPullRequestInheritance {
                child_thread_id: child.into(),
                parent_link_id: parent.link.link_id,
                at_ns: 10,
            })
            .unwrap();
        assert_eq!(scheduled.state, InheritanceState::Pending);
        assert_eq!(scheduled.expected_child_revision, 0);

        // When: the child decides for itself before the inheritance is applied.
        let receipt = if decision == "link" {
            let receipt = link(&h, child, 9, 0);
            assert!(receipt.cancelled_inheritance);
            receipt.lifecycle_revision
        } else {
            // Unlink needs a link first: link then unlink; either way the inheritance
            // was cancelled by the first decision.
            let receipt = link(&h, child, 9, 0);
            assert!(receipt.cancelled_inheritance);
            h.unlink_pull_request(child, 1, 20)
                .unwrap()
                .lifecycle_revision
        };
        assert!(receipt >= 1);

        // Then: the right is cancelled, and a retry never brings it back.
        let row = h.pull_request_inheritance(child).unwrap().unwrap();
        assert_eq!(row.state, InheritanceState::Cancelled);
        assert!(matches!(
            row.decision_reason,
            Some(InheritanceReason::ExplicitLink)
        ));
        for _ in 0..2 {
            assert_eq!(
                h.resolve_pull_request_inheritance(child, seq, 30).unwrap(),
                InheritanceResolution::AlreadyDecided(InheritanceState::Cancelled)
            );
        }
        let again = h
            .schedule_pull_request_inheritance(NewPullRequestInheritance {
                child_thread_id: child.into(),
                parent_link_id: parent.link.link_id,
                at_ns: 40,
            })
            .unwrap();
        assert_eq!(again.state, InheritanceState::Cancelled);
        let state = h.pull_request_thread_state(child).unwrap();
        assert!(
            state
                .active_link
                .as_ref()
                .is_none_or(|link| link.origin == LinkOrigin::Explicit),
            "no inherited link appeared"
        );
    }
}

#[test]
fn an_unlink_cancels_a_pending_inheritance_even_when_the_child_has_no_link() {
    let f = Fixture::new();
    let h = f.handle();
    let parent = link(&h, "parent", 1, 0);
    let seq = fetch_open(&h, 1, PullRequestState::Open);

    // Given: a derived thread with a pending inheritance and no link of its own.
    let scheduled = h
        .schedule_pull_request_inheritance(NewPullRequestInheritance {
            child_thread_id: "child".into(),
            parent_link_id: parent.link.link_id,
            at_ns: 10,
        })
        .unwrap();
    assert_eq!(scheduled.state, InheritanceState::Pending);
    assert_eq!(scheduled.expected_child_revision, 0);

    // When: the person unlinks it (declines the inheritance) from the revision they read.
    let receipt = h.unlink_pull_request("child", 0, 20).unwrap();

    // Then: the decision is recorded, the inheritance is cancelled in the same
    // transaction, and a later retry with an open snapshot creates no link.
    assert!(receipt.cancelled_inheritance);
    assert_eq!(receipt.lifecycle_revision, 1);
    let row = h.pull_request_inheritance("child").unwrap().unwrap();
    assert_eq!(row.state, InheritanceState::Cancelled);
    assert_eq!(row.decision_reason, Some(InheritanceReason::ExplicitUnlink));
    for _ in 0..2 {
        assert_eq!(
            h.resolve_pull_request_inheritance("child", seq, 30)
                .unwrap(),
            InheritanceResolution::AlreadyDecided(InheritanceState::Cancelled)
        );
    }
    let state = h.pull_request_thread_state("child").unwrap();
    assert!(state.active_link.is_none());
    assert!(h.pull_request_link_history("child", 10).unwrap().is_empty());

    // And: the revision check still applies, and a thread with neither a link nor a
    // pending inheritance still has nothing to unlink.
    assert!(matches!(
        h.unlink_pull_request("child", 0, 40).unwrap_err(),
        StorageError::StaleRevision {
            expected: 0,
            current: 1,
            ..
        }
    ));
    assert_eq!(
        rejection(&h.unlink_pull_request("child", 1, 41).unwrap_err()),
        &PullRequestRejection::NotLinked
    );
    assert_eq!(
        rejection(&h.unlink_pull_request("nobody", 0, 42).unwrap_err()),
        &PullRequestRejection::NotLinked
    );
}

#[test]
fn a_child_that_decided_before_the_inheritance_was_scheduled_does_not_inherit() {
    let f = Fixture::new();
    let h = f.handle();
    let parent = link(&h, "parent", 1, 0);
    let seq = fetch_open(&h, 1, PullRequestState::Open);
    let schedule = |child: &str| {
        h.schedule_pull_request_inheritance(NewPullRequestInheritance {
            child_thread_id: child.into(),
            parent_link_id: parent.link.link_id,
            at_ns: 50,
        })
        .unwrap()
    };

    // Given: one child linked and then unlinked, another still linked, both before
    // anything was scheduled for them.
    link(&h, "unlinked-child", 5, 0);
    h.unlink_pull_request("unlinked-child", 1, 20).unwrap();
    link(&h, "linked-child", 6, 0);

    // When: the inheritance is scheduled afterwards.
    let unlinked_row = schedule("unlinked-child");
    let linked_row = schedule("linked-child");

    // Then: it is decided at once; the explicit decision stands and nothing is inherited.
    assert_eq!(unlinked_row.state, InheritanceState::Cancelled);
    assert_eq!(
        unlinked_row.decision_reason,
        Some(InheritanceReason::ExplicitUnlink)
    );
    assert_eq!(linked_row.state, InheritanceState::Cancelled);
    assert_eq!(
        linked_row.decision_reason,
        Some(InheritanceReason::ExplicitLink)
    );
    for child in ["unlinked-child", "linked-child"] {
        assert_eq!(
            h.resolve_pull_request_inheritance(child, seq, 60).unwrap(),
            InheritanceResolution::AlreadyDecided(InheritanceState::Cancelled)
        );
    }
    assert!(
        h.pull_request_thread_state("unlinked-child")
            .unwrap()
            .active_link
            .is_none()
    );
    let kept = h.pull_request_thread_state("linked-child").unwrap();
    assert_eq!(kept.active_link.unwrap().key, key(6));

    // And: a pending row whose child has an explicit link row that the cancelling
    // writes never saw (the revision still matches) is cancelled by the resolution.
    schedule("sneaky-child");
    f.sql()
        .execute(
            "INSERT INTO thread_pull_requests(thread_id, provider, host, repository, number, \
             origin, created_at_ns, link_revision, superseded_at_ns, end_reason, end_revision) \
             VALUES ('sneaky-child', 'github', 'github.com', 'o/r', 8, 'explicit', 1, 1, 2, \
             'unlinked', 2)",
            [],
        )
        .unwrap();
    assert_eq!(
        h.resolve_pull_request_inheritance("sneaky-child", seq, 70)
            .unwrap(),
        InheritanceResolution::Cancelled(InheritanceReason::ExplicitUnlink)
    );
    assert!(
        h.pull_request_thread_state("sneaky-child")
            .unwrap()
            .active_link
            .is_none()
    );
}

#[test]
fn inheritance_needs_the_pull_request_confirmed_open() {
    let f = Fixture::new();
    let h = f.handle();
    let parent = link(&h, "parent", 1, 0);
    let schedule = |child: &str| {
        h.schedule_pull_request_inheritance(NewPullRequestInheritance {
            child_thread_id: child.into(),
            parent_link_id: parent.link.link_id,
            at_ns: 10,
        })
        .unwrap()
    };

    // Unknown state keeps it pending (and counts the attempt).
    schedule("c1");
    assert_eq!(
        h.resolve_pull_request_inheritance("c1", 0, 11).unwrap(),
        InheritanceResolution::Pending(InheritancePending::NoFacts)
    );
    let seq_old = fetch_open(&h, 1, PullRequestState::Open);
    // A snapshot older than the one the caller requires is not a confirmation.
    assert_eq!(
        h.resolve_pull_request_inheritance("c1", seq_old + 1, 12)
            .unwrap(),
        InheritanceResolution::Pending(InheritancePending::NeedsFresherSnapshot)
    );
    let row = h.pull_request_inheritance("c1").unwrap().unwrap();
    assert_eq!(
        (row.state, row.attempt_count),
        (InheritanceState::Pending, 2)
    );
    assert!(row.last_attempt_note.is_some());
    h.record_pull_request_inheritance_attempt("c1", "fetch failed\nbadly", 13)
        .unwrap();
    let row = h.pull_request_inheritance("c1").unwrap().unwrap();
    assert_eq!(row.attempt_count, 3);
    assert_eq!(row.last_attempt_note.as_deref(), Some("fetch failed badly"));

    // Open and fresh enough: applied as an inherited link pinned to the parent link.
    let resolved = h
        .resolve_pull_request_inheritance("c1", seq_old, 14)
        .unwrap();
    let InheritanceResolution::Applied {
        link: child_link,
        lifecycle_revision,
    } = resolved
    else {
        panic!("not applied: {resolved:?}");
    };
    assert_eq!(lifecycle_revision, 1);
    assert_eq!(child_link.origin, LinkOrigin::Inherited);
    assert_eq!(child_link.parent_link_id, Some(parent.link.link_id));
    assert_eq!(child_link.key, key(1));
    let row = h.pull_request_inheritance("c1").unwrap().unwrap();
    assert_eq!(row.state, InheritanceState::Applied);
    assert_eq!(row.applied_link_id, Some(child_link.link_id));
    // Applying twice is a no-op.
    assert_eq!(
        h.resolve_pull_request_inheritance("c1", seq_old, 15)
            .unwrap(),
        InheritanceResolution::AlreadyDecided(InheritanceState::Applied)
    );

    // Merged and closed are never inherited.
    let merged_parent = link(&h, "parent2", 2, 0);
    let closed_parent = link(&h, "parent3", 3, 0);
    for (parent_link, number, state, reason) in [
        (
            &merged_parent,
            2,
            PullRequestState::Merged,
            InheritanceReason::Merged,
        ),
        (
            &closed_parent,
            3,
            PullRequestState::Closed,
            InheritanceReason::Closed,
        ),
    ] {
        let child = format!("child-{number}");
        h.schedule_pull_request_inheritance(NewPullRequestInheritance {
            child_thread_id: child.clone(),
            parent_link_id: parent_link.link.link_id,
            at_ns: 10,
        })
        .unwrap();
        let seq = fetch_open(&h, number, state);
        assert_eq!(
            h.resolve_pull_request_inheritance(&child, seq, 20).unwrap(),
            InheritanceResolution::Skipped(reason)
        );
        assert!(
            h.pull_request_thread_state(&child)
                .unwrap()
                .active_link
                .is_none()
        );
    }
}

#[test]
fn a_parent_unlink_changes_neither_the_child_link_nor_its_inheritance_row() {
    let f = Fixture::new();
    let h = f.handle();
    let parent = link(&h, "parent", 1, 0);
    let seq = fetch_open(&h, 1, PullRequestState::Open);

    // Given: one child that already inherited and one still pending.
    h.schedule_pull_request_inheritance(NewPullRequestInheritance {
        child_thread_id: "applied-child".into(),
        parent_link_id: parent.link.link_id,
        at_ns: 10,
    })
    .unwrap();
    assert!(matches!(
        h.resolve_pull_request_inheritance("applied-child", seq, 11)
            .unwrap(),
        InheritanceResolution::Applied { .. }
    ));
    let pending_before = h
        .schedule_pull_request_inheritance(NewPullRequestInheritance {
            child_thread_id: "pending-child".into(),
            parent_link_id: parent.link.link_id,
            at_ns: 12,
        })
        .unwrap();
    let applied_before = h.pull_request_thread_state("applied-child").unwrap();

    // When: the parent unlinks.
    h.unlink_pull_request("parent", 1, 20).unwrap();

    // Then: the children's rows are exactly what they were, and the pending child can
    // still take the pinned link.
    assert_eq!(
        h.pull_request_thread_state("applied-child").unwrap(),
        applied_before
    );
    assert_eq!(
        h.pull_request_inheritance("pending-child")
            .unwrap()
            .unwrap(),
        pending_before
    );
    assert!(matches!(
        h.resolve_pull_request_inheritance("pending-child", seq, 30)
            .unwrap(),
        InheritanceResolution::Applied { .. }
    ));
    // A parent link that ended cannot be pinned any more.
    let error = h
        .schedule_pull_request_inheritance(NewPullRequestInheritance {
            child_thread_id: "late-child".into(),
            parent_link_id: parent.link.link_id,
            at_ns: 40,
        })
        .unwrap_err();
    assert_eq!(
        rejection(&error),
        &PullRequestRejection::ParentLinkNotActive
    );
}

#[test]
fn pending_inheritances_are_listed_in_pages_without_gaps_or_repeats() {
    let f = Fixture::new();
    let h = f.handle();
    let parent = link(&h, "parent", 1, 0);
    // Rows share creation times, so the child ID is the tie-break of the cursor.
    for (child, at_ns) in [("c", 5), ("a", 5), ("e", 7), ("b", 5), ("d", 6)] {
        h.schedule_pull_request_inheritance(NewPullRequestInheritance {
            child_thread_id: child.into(),
            parent_link_id: parent.link.link_id,
            at_ns,
        })
        .unwrap();
    }
    h.skip_pull_request_inheritance("d", InheritanceSkipReason::Other, 9)
        .unwrap();

    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let page = h
            .pending_pull_request_inheritances(cursor.clone(), 2)
            .unwrap();
        seen.extend(page.iter().map(|row| row.child_thread_id.clone()));
        if page.len() < 2 {
            break;
        }
        cursor = page
            .last()
            .map(storage::pull_requests::InheritanceRecord::cursor);
    }
    assert_eq!(
        seen,
        vec!["a", "b", "c", "e"],
        "decided rows are not listed"
    );
}

#[test]
fn lanes_and_the_parent_itself_never_inherit() {
    let f = Fixture::new();
    let h = f.handle();
    f.thread("lane-thread", Some("lane"), false);
    let parent = link(&h, "parent", 1, 0);
    let schedule = |child: &str, link_id: u64| {
        h.schedule_pull_request_inheritance(NewPullRequestInheritance {
            child_thread_id: child.into(),
            parent_link_id: link_id,
            at_ns: 10,
        })
    };
    assert_eq!(
        rejection(&schedule("lane-thread", parent.link.link_id).unwrap_err()),
        &PullRequestRejection::LaneThread
    );
    assert_eq!(
        rejection(&schedule("parent", parent.link.link_id).unwrap_err()),
        &PullRequestRejection::SameThread
    );
    assert_eq!(
        rejection(&schedule("c", 9_999).unwrap_err()),
        &PullRequestRejection::ParentLinkNotFound
    );

    // The runtime can also skip a pending row by itself; a decided row stays.
    schedule("skipped", parent.link.link_id).unwrap();
    let skipped = h
        .skip_pull_request_inheritance("skipped", InheritanceSkipReason::Other, 20)
        .unwrap()
        .unwrap();
    assert_eq!(skipped.state, InheritanceState::Skipped);
    let again = h
        .skip_pull_request_inheritance("skipped", InheritanceSkipReason::Merged, 30)
        .unwrap()
        .unwrap();
    assert_eq!(again.decision_reason, Some(InheritanceReason::Other));
    assert!(
        h.skip_pull_request_inheritance("nobody", InheritanceSkipReason::Other, 1)
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_child_that_changed_since_scheduling_is_not_given_the_link() {
    let f = Fixture::new();
    let h = f.handle();
    let parent = link(&h, "parent", 1, 0);
    let seq = fetch_open(&h, 1, PullRequestState::Open);
    h.schedule_pull_request_inheritance(NewPullRequestInheritance {
        child_thread_id: "child".into(),
        parent_link_id: parent.link.link_id,
        at_ns: 10,
    })
    .unwrap();

    // When: the child's revision moved without going through the cancelling writes
    // (a defect or a hand edit); the CAS still refuses.
    f.sql()
        .execute(
            "INSERT INTO thread_pull_request_state(thread_id, lifecycle_revision) VALUES ('child', 4)",
            [],
        )
        .unwrap();

    // Then: the right is cancelled instead of applied.
    assert_eq!(
        h.resolve_pull_request_inheritance("child", seq, 20)
            .unwrap(),
        InheritanceResolution::Cancelled(InheritanceReason::ChildChanged)
    );
    assert!(
        h.pull_request_thread_state("child")
            .unwrap()
            .active_link
            .is_none()
    );
}

#[test]
fn a_result_older_than_the_stored_one_is_refused_and_a_failure_keeps_the_last_success() {
    let f = Fixture::new();
    let h = f.handle();
    let k = key(1);

    // Given: three fetches started in order 1, 2, 3.
    let s1 = h.begin_pull_request_fetch(&k).unwrap();
    let s2 = h.begin_pull_request_fetch(&k).unwrap();
    let s3 = h.begin_pull_request_fetch(&k).unwrap();
    assert_eq!((s1, s2, s3), (1, 2, 3));

    // When: 2 finishes first, then the older 1 arrives.
    let mut newer = facts(PullRequestState::Open);
    newer.head_sha = sha('b');
    let c2 = h.commit_pull_request_fetch(&k, s2, newer, 20).unwrap();
    let c1 = h
        .commit_pull_request_fetch(&k, s1, facts(PullRequestState::Closed), 21)
        .unwrap();

    // Then: only the newer facts are stored.
    assert!(c2.facts_stored && c2.attempt_recorded);
    assert!(!c1.facts_stored && !c1.attempt_recorded);
    let snapshot = h.pull_request_snapshot(&k).unwrap().unwrap();
    assert_eq!(
        snapshot.facts.as_ref().unwrap().state,
        PullRequestState::Open
    );
    assert_eq!(snapshot.facts.as_ref().unwrap().head_sha, sha('b'));
    assert_eq!((snapshot.success_seq, snapshot.fetch_seq), (2, 3));

    // And: the newest fetch failing keeps the facts, records the error and counts it.
    let failure = FetchFailure {
        kind: FetchErrorKind::RateLimited,
        message: "API rate limit\nexceeded".into(),
    };
    let failed = h
        .fail_pull_request_fetch(&k, s3, failure.clone(), 30)
        .unwrap();
    assert!(!failed.facts_stored && failed.attempt_recorded);
    let snapshot = h.pull_request_snapshot(&k).unwrap().unwrap();
    assert_eq!(snapshot.facts.unwrap().head_sha, sha('b'));
    assert_eq!(snapshot.success_seq, 2);
    let attempt = snapshot.last_attempt.unwrap();
    assert_eq!(attempt.seq, 3);
    assert_eq!(attempt.failure.unwrap().message, "API rate limit exceeded");
    assert_eq!(snapshot.consecutive_failures, 1);

    // And: replaying a failure or an older failure changes nothing (idempotent).
    let replay = h
        .fail_pull_request_fetch(&k, s3, failure.clone(), 31)
        .unwrap();
    let older = h.fail_pull_request_fetch(&k, s1, failure, 32).unwrap();
    assert!(!replay.attempt_recorded && !older.attempt_recorded);
    assert_eq!(
        h.pull_request_snapshot(&k)
            .unwrap()
            .unwrap()
            .consecutive_failures,
        1
    );

    // And: a success that started before the failed attempt stores its facts but does
    // not hide the failure that came after it.
    let s4 = h.begin_pull_request_fetch(&k).unwrap();
    let s5 = h.begin_pull_request_fetch(&k).unwrap();
    h.fail_pull_request_fetch(
        &k,
        s5,
        FetchFailure {
            kind: FetchErrorKind::Network,
            message: "offline".into(),
        },
        40,
    )
    .unwrap();
    let late = h
        .commit_pull_request_fetch(&k, s4, facts(PullRequestState::Merged), 41)
        .unwrap();
    assert!(late.facts_stored && !late.attempt_recorded);
    let snapshot = h.pull_request_snapshot(&k).unwrap().unwrap();
    assert_eq!(snapshot.facts.unwrap().state, PullRequestState::Merged);
    assert_eq!(snapshot.last_attempt.unwrap().seq, 5);

    // And: a newer success clears the error and the failure count.
    let s6 = h.begin_pull_request_fetch(&k).unwrap();
    h.commit_pull_request_fetch(&k, s6, facts(PullRequestState::Open), 50)
        .unwrap();
    let snapshot = h.pull_request_snapshot(&k).unwrap().unwrap();
    let attempt = snapshot.last_attempt.unwrap();
    assert!(attempt.failure.is_none());
    assert_eq!((attempt.seq, snapshot.consecutive_failures), (6, 0));

    // A sequence nobody handed out is refused.
    assert!(matches!(
        rejection(
            &h.commit_pull_request_fetch(&k, 99, facts(PullRequestState::Open), 60)
                .unwrap_err()
        ),
        PullRequestRejection::Invalid { field: "seq", .. }
    ));
}

#[test]
fn sequences_are_unique_and_ordered_across_writers() {
    // Given: two storages (two writer threads, as two processes would be) on one file.
    let f = Fixture::new();
    let other = Storage::open(f.config.clone()).unwrap();
    let handles = [f.handle(), other.handle()];
    let k = key(1);
    let barrier = Arc::new(Barrier::new(4));

    // When: four threads start fetches at the same time.
    let workers: Vec<_> = (0..4)
        .map(|index| {
            let handle = handles[index % 2].clone();
            let (k, barrier) = (k.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                (0..10)
                    .map(|_| handle.begin_pull_request_fetch(&k).unwrap())
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let mut seen = BTreeSet::new();
    for worker in workers {
        let seqs = worker.join().unwrap();
        assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]), "{seqs:?}");
        for seq in seqs {
            assert!(seen.insert(seq), "sequence {seq} was handed out twice");
        }
    }

    // Then: the sequences are exactly 1..=40, and results committed from both writers in
    // reverse order leave the newest facts.
    assert_eq!(seen, (1..=40).collect::<BTreeSet<_>>());
    for (writer, seq) in [(0, 40), (1, 39), (0, 38)] {
        let mut facts = facts(PullRequestState::Open);
        facts.head_sha = sha(char::from(b'a' + u8::try_from(seq % 6).unwrap()));
        handles[writer]
            .commit_pull_request_fetch(&k, seq, facts, 1_000 + i64::try_from(seq).unwrap())
            .unwrap();
    }
    let snapshot = handles[1].pull_request_snapshot(&k).unwrap().unwrap();
    assert_eq!(snapshot.success_seq, 40);
    assert_eq!(
        snapshot.facts.unwrap().head_sha,
        sha(char::from(b'a' + 40 % 6))
    );
}

#[test]
fn snapshot_facts_are_structured_sanitized_and_bounded() {
    let f = Fixture::new();
    let h = f.handle();
    let k = key(1);
    let seq = h.begin_pull_request_fetch(&k).unwrap();

    // A check name from a third party loses its newlines and is cut.
    let mut with_names = facts(PullRequestState::Open);
    with_names.checks = vec![
        check(
            "9\n9",
            &format!("build\n  /  \u{7}test {}", "x".repeat(400)),
            CheckStatus::InProgress,
            None,
        ),
        check(
            "10",
            "lint",
            CheckStatus::Completed,
            Some(CheckConclusion::Failure),
        ),
    ];
    with_names.checks_complete = false;
    h.commit_pull_request_fetch(&k, seq, with_names, 5).unwrap();
    let stored = h.pull_request_snapshot(&k).unwrap().unwrap().facts.unwrap();
    assert_eq!(stored.checks[0].provider_id, "9 9");
    assert!(stored.checks[0].name.starts_with("build / test x"));
    assert!(!stored.checks[0].name.contains(['\n', '\u{7}']));
    assert!(stored.checks[0].name.chars().count() <= 160);
    assert!(!stored.checks_complete, "completeness is kept");
    assert_eq!(stored.head_repository.as_deref(), Some("fork/repo"));
    assert_eq!(stored.review_decision, ReviewDecision::ReviewRequired);

    // Malformed facts are refused instead of stored.
    let seq = h.begin_pull_request_fetch(&k).unwrap();
    let mut bad_sha = facts(PullRequestState::Open);
    bad_sha.head_sha = "not-a-sha".into();
    let mut orphan_conclusion = facts(PullRequestState::Open);
    orphan_conclusion.checks = vec![check(
        "1",
        "n",
        CheckStatus::Queued,
        Some(CheckConclusion::Success),
    )];
    let mut too_many = facts(PullRequestState::Open);
    too_many.checks = (0..=PULL_REQUEST_CHECKS_MAX)
        .map(|i| check(&i.to_string(), "n", CheckStatus::Queued, None))
        .collect();
    let mut no_branch = facts(PullRequestState::Open);
    no_branch.head_branch = "  ".into();
    for bad in [bad_sha, orphan_conclusion, too_many, no_branch] {
        let error = h.commit_pull_request_fetch(&k, seq, bad, 6).unwrap_err();
        assert!(matches!(
            rejection(&error),
            PullRequestRejection::Invalid { .. }
        ));
    }
    assert_eq!(
        h.pull_request_snapshot(&k).unwrap().unwrap().success_seq,
        1,
        "nothing malformed was stored"
    );
    assert_eq!(sanitize_single_line(" a\n\tb  c ", 10), "a b c");
    assert_eq!(sanitize_single_line("abcdef", 3), "abc");
}

#[test]
fn a_credential_that_straddles_the_length_limit_is_never_stored_in_part() {
    let f = Fixture::new();
    let h = f.handle();
    let k = key(1);
    let token = format!("ghp_{}", "A1b2C3d4E5".repeat(4));

    // Given: third-party texts in which a credential starts just before the cut.
    let straddling = |limit: usize| format!("{} {token} trailing", "n".repeat(limit - 8));
    let mut with_secret = facts(PullRequestState::Open);
    with_secret.checks = vec![check(
        &straddling(128),
        &straddling(160),
        CheckStatus::Queued,
        None,
    )];
    let seq = h.begin_pull_request_fetch(&k).unwrap();
    h.commit_pull_request_fetch(&k, seq, with_secret, 5)
        .unwrap();
    let failure_seq = h.begin_pull_request_fetch(&k).unwrap();
    h.fail_pull_request_fetch(
        &k,
        failure_seq,
        FetchFailure {
            kind: FetchErrorKind::Other,
            message: straddling(256),
        },
        6,
    )
    .unwrap();

    // Then: no part of the credential reached the database.
    let snapshot = h.pull_request_snapshot(&k).unwrap().unwrap();
    let stored_check = &snapshot.facts.unwrap().checks[0];
    let message = snapshot.last_attempt.unwrap().failure.unwrap().message;
    for text in [&stored_check.name, &stored_check.provider_id, &message] {
        assert!(!text.contains("ghp_"), "{text}");
        assert!(!text.contains("A1b2C3d4E5"), "{text}");
        assert!(text.contains("[REDACT"), "{text}");
    }
    let raw: String = f
        .sql()
        .query_row(
            "SELECT checks_json || COALESCE(last_error_message, '') FROM pull_request_snapshots",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!raw.contains("ghp_"), "{raw}");
}

#[test]
fn a_notification_is_claimed_once_and_only_for_the_active_link() {
    let f = Fixture::new();
    let h = f.handle();
    let linked = link(&h, "t1", 1, 0);
    let seq = h
        .next_pull_request_transition("t1", linked.link.link_revision)
        .unwrap();
    assert_eq!(seq, 1);
    let key_a = NotificationKey {
        link_revision: linked.link.link_revision,
        transition_seq: seq,
        kind: "checks_failed".into(),
    };

    // The first claim wins, every repeat is refused, and the question can be asked.
    assert!(
        !h.pull_request_notification_claimed("t1", key_a.clone())
            .unwrap()
    );
    assert!(
        h.claim_pull_request_notification("t1", key_a.clone())
            .unwrap()
    );
    assert!(
        !h.claim_pull_request_notification("t1", key_a.clone())
            .unwrap()
    );
    assert!(
        h.pull_request_notification_claimed("t1", key_a.clone())
            .unwrap()
    );
    // Another kind of the same transition is its own notification.
    let key_b = NotificationKey {
        kind: "merged".into(),
        ..key_a.clone()
    };
    assert!(h.claim_pull_request_notification("t1", key_b).unwrap());
    // A sequence that was never issued, and a malformed kind, are refused.
    let future = NotificationKey {
        transition_seq: 7,
        ..key_a.clone()
    };
    assert!(h.claim_pull_request_notification("t1", future).is_err());
    let bad_kind = NotificationKey {
        kind: "Checks Failed".into(),
        ..key_a.clone()
    };
    assert!(h.claim_pull_request_notification("t1", bad_kind).is_err());

    // After a relink the old link revision cannot notify, and the transition counter
    // keeps counting.
    let relinked = link(&h, "t1", 2, 1);
    assert!(matches!(
        rejection(
            &h.claim_pull_request_notification("t1", key_a.clone())
                .unwrap_err()
        ),
        PullRequestRejection::LinkNotActive
    ));
    assert_eq!(
        h.next_pull_request_transition("t1", relinked.link.link_revision)
            .unwrap(),
        2
    );
    // After an unlink nothing can notify at all.
    h.unlink_pull_request("t1", 2, 300).unwrap();
    assert!(matches!(
        rejection(&h.next_pull_request_transition("t1", 2).unwrap_err()),
        PullRequestRejection::NotLinked
    ));
    // Already-claimed keys are still reported as claimed.
    assert!(h.pull_request_notification_claimed("t1", key_a).unwrap());
}

#[test]
fn old_notification_keys_stay_claimed_when_the_exact_list_is_trimmed() {
    let f = Fixture::new();
    let h = f.handle();
    let linked = link(&h, "t1", 1, 0);
    let revision = linked.link.link_revision;
    let total = PULL_REQUEST_NOTIFIED_MAX + 10;
    for index in 1..=total {
        let seq = h.next_pull_request_transition("t1", revision).unwrap();
        assert_eq!(seq, u64::try_from(index).unwrap());
        assert!(
            h.claim_pull_request_notification(
                "t1",
                NotificationKey {
                    link_revision: revision,
                    transition_seq: seq,
                    kind: "checks_failed".into(),
                },
            )
            .unwrap()
        );
    }
    // The oldest keys are no longer listed exactly, but they cannot be sent again.
    for seq in [1_u64, 5, 10] {
        let key = NotificationKey {
            link_revision: revision,
            transition_seq: seq,
            kind: "checks_failed".into(),
        };
        assert!(
            h.pull_request_notification_claimed("t1", key.clone())
                .unwrap()
        );
        assert!(!h.claim_pull_request_notification("t1", key).unwrap());
    }
    let listed: String = f
        .sql()
        .query_row(
            "SELECT notified_json FROM thread_pull_request_state WHERE thread_id = 't1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let entries: Vec<serde_json::Value> = serde_json::from_str(&listed).unwrap();
    assert_eq!(entries.len(), PULL_REQUEST_NOTIFIED_MAX);
}

#[test]
fn the_local_head_belongs_to_the_link_revision_it_was_observed_under() {
    let f = Fixture::new();
    let h = f.handle();
    let linked = link(&h, "t1", 1, 0);
    let revision = linked.link.link_revision;

    assert!(
        h.record_pull_request_local_head("t1", revision, &sha('c'), 50)
            .unwrap()
    );
    let seen = h
        .pull_request_thread_state("t1")
        .unwrap()
        .local_head
        .unwrap();
    assert_eq!(
        (seen.link_revision, seen.head_sha.as_str()),
        (revision, sha('c').as_str())
    );
    // An observation made earlier does not replace a later one.
    assert!(
        !h.record_pull_request_local_head("t1", revision, &sha('d'), 40)
            .unwrap()
    );
    assert_eq!(
        h.pull_request_thread_state("t1")
            .unwrap()
            .local_head
            .unwrap()
            .head_sha,
        sha('c')
    );

    // A relink drops the observation; a stale revision cannot write a new one.
    link(&h, "t1", 2, 1);
    assert!(
        h.pull_request_thread_state("t1")
            .unwrap()
            .local_head
            .is_none()
    );
    assert_eq!(
        rejection(
            &h.record_pull_request_local_head("t1", revision, &sha('e'), 60)
                .unwrap_err()
        ),
        &PullRequestRejection::LinkNotActive
    );
    assert!(
        h.pull_request_thread_state("t1")
            .unwrap()
            .local_head
            .is_none()
    );
    assert!(
        h.record_pull_request_local_head("t1", 2, "xyz", 61)
            .is_err()
    );
}

#[test]
fn candidates_are_presented_once_and_decided_by_a_person() {
    let f = Fixture::new();
    let h = f.handle();
    let present = |number: u64, call: &str| {
        h.present_pull_request_candidate(NewPullRequestCandidate {
            thread_id: "t1".into(),
            key: key(number),
            source_run_id: "run-1".into(),
            source_call_id: call.into(),
            at_ns: 10,
        })
        .unwrap()
    };

    // Seeing the same pull request again keeps the first candidate.
    let first = present(1, "call-a");
    assert!(first.created);
    let repeat = present(1, "call-b");
    assert!(!repeat.created);
    assert_eq!(repeat.candidate, first.candidate);

    // Dismissing sticks: it is not revived by the next sighting.
    let dismissed = h
        .dismiss_pull_request_candidate("t1", first.candidate.candidate_id, 20)
        .unwrap();
    assert_eq!(dismissed.state, CandidateState::Dismissed);
    assert_eq!(
        present(1, "call-c").candidate.state,
        CandidateState::Dismissed
    );

    // Linking a suggested pull request confirms its candidate, which then stays confirmed.
    let second = present(2, "call-d");
    link(&h, "t1", 2, 0);
    let candidates = h.pull_request_candidates("t1", None).unwrap();
    assert_eq!(candidates.len(), 2);
    assert_eq!(
        h.pull_request_candidates("t1", Some(CandidateState::Confirmed))
            .unwrap()[0]
            .candidate_id,
        second.candidate.candidate_id
    );
    let still = h
        .dismiss_pull_request_candidate("t1", second.candidate.candidate_id, 30)
        .unwrap();
    assert_eq!(still.state, CandidateState::Confirmed);

    // A candidate of another thread is not found, and a missing one neither.
    assert_eq!(
        rejection(
            &h.dismiss_pull_request_candidate("t2", second.candidate.candidate_id, 31)
                .unwrap_err()
        ),
        &PullRequestRejection::CandidateNotFound
    );
}

#[test]
fn undecided_candidates_are_limited_and_decided_ones_make_room() {
    let f = Fixture::new();
    let h = f.handle();
    let present = |number: u64| {
        h.present_pull_request_candidate(NewPullRequestCandidate {
            thread_id: "t1".into(),
            key: key(number),
            source_run_id: "run-1".into(),
            source_call_id: format!("call-{number}"),
            at_ns: i64::try_from(number).unwrap(),
        })
    };
    let max = u64::try_from(PULL_REQUEST_CANDIDATES_MAX).unwrap();
    for number in 1..=max {
        present(number).unwrap();
    }
    let error = present(max + 1).unwrap_err();
    assert_eq!(rejection(&error), &PullRequestRejection::CandidateLimit);

    let first = h.pull_request_candidates("t1", None).unwrap()[0].candidate_id;
    h.dismiss_pull_request_candidate("t1", first, 500).unwrap();
    present(max + 1).unwrap();
    let all = h.pull_request_candidates("t1", None).unwrap();
    assert_eq!(all.len(), PULL_REQUEST_CANDIDATES_MAX);
    assert!(
        all.iter().all(|c| c.candidate_id != first),
        "the decided one made room"
    );
}

#[test]
fn writes_for_a_deleted_thread_are_refused() {
    let f = Fixture::new();
    let h = f.handle();
    let parent = link(&h, "parent", 1, 0);
    link(&h, "gone", 2, 0);
    h.present_pull_request_candidate(NewPullRequestCandidate {
        thread_id: "gone".into(),
        key: key(3),
        source_run_id: "run-1".into(),
        source_call_id: "call".into(),
        at_ns: 1,
    })
    .unwrap();
    let seq = h.next_pull_request_transition("gone", 1).unwrap();

    // When: retention tombstones the thread.
    f.tombstone("gone");

    // Then: every late write for it is refused, by the API and by the schema itself.
    let tombstoned = |error: StorageError| matches!(error, StorageError::Tombstoned { .. });
    assert!(tombstoned(
        h.link_pull_request(LinkPullRequest {
            thread_id: "gone".into(),
            key: key(5),
            expected_revision: 1,
            at_ns: 9,
        })
        .unwrap_err()
    ));
    assert!(tombstoned(h.unlink_pull_request("gone", 1, 9).unwrap_err()));
    assert!(tombstoned(
        h.present_pull_request_candidate(NewPullRequestCandidate {
            thread_id: "gone".into(),
            key: key(6),
            source_run_id: "run-1".into(),
            source_call_id: "call-2".into(),
            at_ns: 9,
        })
        .unwrap_err()
    ));
    assert!(tombstoned(
        h.claim_pull_request_notification(
            "gone",
            NotificationKey {
                link_revision: 1,
                transition_seq: seq,
                kind: "merged".into(),
            },
        )
        .unwrap_err()
    ));
    assert!(tombstoned(
        h.next_pull_request_transition("gone", 1).unwrap_err()
    ));
    assert!(tombstoned(
        h.record_pull_request_local_head("gone", 1, &sha('a'), 9)
            .unwrap_err()
    ));
    assert!(tombstoned(
        h.schedule_pull_request_inheritance(NewPullRequestInheritance {
            child_thread_id: "gone".into(),
            parent_link_id: parent.link.link_id,
            at_ns: 9,
        })
        .unwrap_err()
    ));
    let conn = f.sql();
    for sql in [
        "INSERT INTO thread_pull_requests(thread_id, provider, host, repository, number, origin, \
         created_at_ns, link_revision) VALUES ('gone', 'github', 'github.com', 'o/r', 9, 'explicit', 1, 7)",
        "UPDATE thread_pull_request_state SET transition_seq = transition_seq + 1 WHERE thread_id = 'gone'",
        "INSERT INTO thread_pull_request_inheritance(child_thread_id, parent_thread_id, parent_link_id, \
         provider, host, repository, number, expected_child_revision, state, created_at_ns) \
         VALUES ('gone', 'p', 1, 'github', 'github.com', 'o/r', 1, 0, 'pending', 1)",
        "INSERT INTO thread_pull_request_candidates(thread_id, provider, host, repository, number, \
         source_run_id, source_call_id, state, created_at_ns, updated_at_ns) \
         VALUES ('gone', 'github', 'github.com', 'o/r', 8, 'r', 'c', 'presented', 1, 1)",
    ] {
        assert!(conn.execute(sql, []).is_err(), "{sql}");
    }
    // A candidate from a deleted run is refused as well.
    conn.execute(
        "INSERT INTO run_tombstones(run_id, thread_id, family_key) VALUES ('dead-run', 'x', 'f')",
        [],
    )
    .unwrap();
    assert!(tombstoned(
        h.present_pull_request_candidate(NewPullRequestCandidate {
            thread_id: "alive".into(),
            key: key(6),
            source_run_id: "dead-run".into(),
            source_call_id: "call".into(),
            at_ns: 9,
        })
        .unwrap_err()
    ));
    // Other threads are unaffected.
    link(&h, "alive", 7, 0);
}

#[test]
fn the_polling_set_skips_archived_and_deleted_threads() {
    let f = Fixture::new();
    let h = f.handle();
    f.thread("archived", None, true);
    f.thread("live", None, false);
    for (index, thread) in ["live", "archived", "deleted", "unknown-to-threads"]
        .into_iter()
        .enumerate()
    {
        link(&h, thread, u64::try_from(index).unwrap() + 1, 0);
    }
    f.tombstone("deleted");
    let listed: Vec<String> = h
        .active_pull_request_links(0, 10)
        .unwrap()
        .into_iter()
        .map(|link| link.thread_id)
        .collect();
    assert_eq!(listed, vec!["live", "unknown-to-threads"]);
    let first_page = h.active_pull_request_links(0, 1).unwrap();
    let next = h
        .active_pull_request_links(first_page[0].link_id, 10)
        .unwrap();
    assert_eq!(next.len(), 1);
}

#[test]
fn candidates_history_and_pending_inheritance_survive_a_restart() {
    // Given: a database with a dismissed and a presented candidate, a thread that linked
    // and unlinked, a pending inheritance and a stored snapshot.
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("restart.db"),
        ..StorageConfig::default()
    };
    let (dismissed_id, presented_id, parent_link_id);
    {
        let storage = Storage::open(config.clone()).unwrap();
        let h = storage.handle();
        let candidate = |number: u64| {
            h.present_pull_request_candidate(NewPullRequestCandidate {
                thread_id: "t1".into(),
                key: key(number),
                source_run_id: "run-1".into(),
                source_call_id: format!("call-{number}"),
                at_ns: 5,
            })
            .unwrap()
            .candidate
        };
        dismissed_id = candidate(1).candidate_id;
        presented_id = candidate(2).candidate_id;
        h.dismiss_pull_request_candidate("t1", dismissed_id, 6)
            .unwrap();
        link(&h, "t1", 3, 0);
        h.unlink_pull_request("t1", 1, 7).unwrap();
        parent_link_id = link(&h, "parent", 4, 0).link.link_id;
        h.schedule_pull_request_inheritance(NewPullRequestInheritance {
            child_thread_id: "child".into(),
            parent_link_id,
            at_ns: 8,
        })
        .unwrap();
        fetch_open(&h, 4, PullRequestState::Open);
        storage.close();
    }

    // When: the database is opened again.
    let storage = Storage::open(config).unwrap();
    let h = storage.handle();

    // Then: every decision is where it was left.
    let candidates = h.pull_request_candidates("t1", None).unwrap();
    let states: Vec<_> = candidates
        .iter()
        .map(|c| (c.candidate_id, c.state))
        .collect();
    assert_eq!(
        states,
        vec![
            (dismissed_id, CandidateState::Dismissed),
            (presented_id, CandidateState::Presented)
        ]
    );
    let state = h.pull_request_thread_state("t1").unwrap();
    assert!(state.active_link.is_none());
    assert_eq!(state.lifecycle_revision, 2);
    let history = h.pull_request_link_history("t1", 10).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].end.unwrap().reason, LinkEndReason::Unlinked);
    assert!(matches!(
        h.unlink_pull_request("t1", 1, 9).unwrap_err(),
        StorageError::StaleRevision {
            expected: 1,
            current: 2,
            ..
        }
    ));
    assert_eq!(
        rejection(&h.unlink_pull_request("t1", 2, 9).unwrap_err()),
        &PullRequestRejection::NotLinked
    );
    let pending = h.pending_pull_request_inheritances(None, 10).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].child_thread_id, "child");
    assert_eq!(pending[0].parent_link_id, parent_link_id);
    // The retry set still works: the pending right is applied after the restart.
    let snapshot = h.pull_request_snapshot(&key(4)).unwrap().unwrap();
    assert!(matches!(
        h.resolve_pull_request_inheritance("child", snapshot.success_seq, 20)
            .unwrap(),
        InheritanceResolution::Applied { .. }
    ));
    assert!(
        h.pending_pull_request_inheritances(None, 10)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn nothing_is_stored_about_a_pull_request_but_structured_fields() {
    let f = Fixture::new();
    let conn = f.sql();
    let forbidden = ["body", "comment", "log", "title", "description", "text"];
    for table in [
        "thread_pull_request_state",
        "thread_pull_requests",
        "pull_request_snapshots",
        "thread_pull_request_candidates",
        "thread_pull_request_inheritance",
    ] {
        let mut statement = conn
            .prepare("SELECT name FROM pragma_table_info(?1)")
            .unwrap();
        let columns: Vec<String> = statement
            .query_map([table], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(!columns.is_empty(), "{table} exists");
        for column in columns {
            assert!(
                !forbidden.iter().any(|word| column.contains(word)),
                "{table}.{column} looks like free text"
            );
        }
    }
}
