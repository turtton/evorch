//! Bounds of `search_threads`: what a request may carry and a page may hold.
use workbench_protocol::errors::{ErrorCode, WorkbenchError};
use workbench_protocol::handshake::{Capability, WireLimits};
use workbench_protocol::ids::{ProjectId, ThreadId};
use workbench_protocol::namespaces::workspace::*;
use workbench_protocol::namespaces::{WorkspaceQuery, WorkspaceReply};

fn search(query: &str, cursor: Option<&str>) -> WorkspaceQuery {
    WorkspaceQuery::SearchThreads(SearchThreads {
        project: ProjectId::new("proj-1").unwrap(),
        query: query.into(),
        cursor: cursor.map(|c| ThreadSearchCursor::new(c).unwrap()),
    })
}

fn code(query: &WorkspaceQuery) -> Option<ErrorCode> {
    query
        .validate(&WireLimits::default())
        .err()
        .map(|e| WorkbenchError::from(e).code)
}

fn hit() -> ThreadSearchHit {
    ThreadSearchHit {
        thread: ThreadId::new("thr-1").unwrap(),
        title: Some("title".into()),
        snippet_kind: Some("user".into()),
        snippet: Some("text".into()),
        archived: false,
    }
}

fn results(hits: Vec<ThreadSearchHit>) -> WorkspaceReply {
    WorkspaceReply::ThreadSearchResults(ThreadSearchResults {
        hits,
        next_cursor: None,
        index: None,
        title_scan_limited: false,
    })
}

#[test]
fn a_search_is_a_query_and_may_be_blank_or_japanese() {
    assert_eq!(search("x", None).required_capability(), Capability::Queries);
    assert_eq!(code(&search("", None)), None);
    assert_eq!(code(&search("設計の本文検索", None)), None);
    assert_eq!(
        code(&search(&"a".repeat(1024), Some("host:titles:20"))),
        None
    );
}

#[test]
fn query_and_cursor_are_bounded() {
    assert_eq!(
        code(&search(&"a".repeat(1025), None)),
        Some(ErrorCode::LimitExceeded)
    );
    assert_eq!(code(&search("a\nb", None)), Some(ErrorCode::InvalidRequest));
    assert!(ThreadSearchCursor::new("").is_err());
    assert!(ThreadSearchCursor::new("a\u{7}b").is_err());
    assert!(ThreadSearchCursor::new("c".repeat(1024)).is_ok());
    assert!(ThreadSearchCursor::new("c".repeat(1025)).is_err());
    // A cursor that arrives on the wire is checked like one built here.
    let wire =
        r#"{"type":"search_threads","payload":{"project":"proj-1","query":"q","cursor":""}}"#;
    let query: WorkspaceQuery = serde_json::from_str(wire).unwrap();
    assert_eq!(code(&query), Some(ErrorCode::InvalidRequest));
}

#[test]
fn a_search_names_no_page_size_or_path() {
    for extra in [r#""limit":5"#, r#""path":"/etc""#] {
        let wire = format!(
            r#"{{"type":"search_threads","payload":{{"project":"proj-1","query":"q",{extra}}}}}"#
        );
        assert!(
            serde_json::from_str::<WorkspaceQuery>(&wire).is_err(),
            "{extra}"
        );
    }
}

#[test]
fn a_page_holds_at_most_twenty_bounded_hits() {
    let validate = |reply: &WorkspaceReply| reply.validate(&WireLimits::default());
    assert!(validate(&results(vec![hit(); 20])).is_ok());
    assert!(validate(&results(vec![hit(); 21])).is_err());
    for broken in [
        ThreadSearchHit {
            title: Some("a\nb".into()),
            ..hit()
        },
        ThreadSearchHit {
            title: Some("t".repeat(1025)),
            ..hit()
        },
        ThreadSearchHit {
            snippet: Some("s".repeat(1025)),
            ..hit()
        },
        ThreadSearchHit {
            snippet: Some("a\u{0}b".into()),
            ..hit()
        },
        ThreadSearchHit {
            snippet_kind: Some("k".repeat(33)),
            ..hit()
        },
    ] {
        assert!(validate(&results(vec![broken])).is_err());
    }
    let snippet = "界".repeat(341); // 1023 bytes
    let ok = ThreadSearchHit {
        snippet: Some(snippet),
        ..hit()
    };
    assert!(validate(&results(vec![ok])).is_ok());
}
