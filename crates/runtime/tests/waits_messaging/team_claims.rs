use runtime::team::{TaskSpec, TeamBoard};

#[test]
fn claim_is_atomic_and_expiry_fences_old_worker() {
    let board = TeamBoard::default();
    board
        .enqueue(TaskSpec {
            id: "a".into(),
            paths: vec!["src/a.rs".into()],
        })
        .unwrap();
    let first = board.claim("a", "worker-1", 100).unwrap();
    assert!(board.claim("a", "worker-2", 101).is_err());
    assert_eq!(board.expire(5_100).unwrap(), ["a"]);
    let second = board.claim("a", "worker-2", 5_100).unwrap();
    assert!(board.complete("a", &first, 5_101).is_err());
    board.complete("a", &second, 5_101).unwrap();
}

#[test]
fn overlapping_artifacts_cannot_be_claimed() {
    let board = TeamBoard::default();
    for (id, path) in [("a", "src"), ("b", "src/a.rs")] {
        board
            .enqueue(TaskSpec {
                id: id.into(),
                paths: vec![path.into()],
            })
            .unwrap();
    }
    board.claim("a", "one", 0).unwrap();
    assert!(board.claim("b", "two", 0).is_err());
}

#[test]
fn heartbeat_extends_only_a_live_lease() {
    let board = TeamBoard::default();
    board
        .enqueue(TaskSpec {
            id: "a".into(),
            paths: vec![],
        })
        .unwrap();
    let token = board.claim("a", "one", 0).unwrap();
    board.heartbeat("a", &token, 4_000).unwrap();
    assert!(board.expire(5_000).unwrap().is_empty());
    assert!(board.heartbeat("a", &token, 9_000).is_err());
}

#[test]
fn concurrent_claim_has_exactly_one_winner() {
    let board = std::sync::Arc::new(TeamBoard::default());
    board
        .enqueue(TaskSpec {
            id: "one".into(),
            paths: vec![],
        })
        .unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let handles: Vec<_> = (0..3)
        .map(|worker| {
            let board = board.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                board.claim("one", &worker.to_string(), 0).is_ok()
            })
        })
        .collect();
    let winners = handles
        .into_iter()
        .map(|handle| usize::from(handle.join().unwrap()))
        .sum::<usize>();
    assert_eq!(winners, 1);
}
