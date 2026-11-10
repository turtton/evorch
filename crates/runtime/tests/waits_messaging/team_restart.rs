use runtime::team::{ClaimState, TaskSpec, TeamBoard};

#[test]
fn restart_preserves_claim_generation_and_completion() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("team.db"),
        ..Default::default()
    };
    let writer = storage::Storage::open(config.clone()).unwrap();
    let board = TeamBoard::durable(writer.handle(), "team".into());
    board
        .enqueue(TaskSpec {
            id: "task".into(),
            paths: vec!["src".into()],
        })
        .unwrap();
    let lease = board.claim("task", "old-worker", 10).unwrap();
    board.heartbeat("task", &lease, 20).unwrap();
    drop(board);
    drop(writer);
    let writer = storage::Storage::open(config.clone()).unwrap();
    let snapshot = storage::Database::open(&config)
        .unwrap()
        .team_snapshot("team")
        .unwrap()
        .unwrap();
    let board = TeamBoard::restore(writer.handle(), snapshot).unwrap();
    assert!(
        matches!(&board.snapshot().unwrap()[0].state, ClaimState::Claimed(saved) if saved.expires_at == 5020)
    );
    assert_eq!(board.expire(5020).unwrap(), ["task"]);
    let replacement = board.claim("task", "new-worker", 5021).unwrap();
    assert_eq!(replacement.generation, lease.generation + 1);
    assert!(board.complete("task", &lease, 5022).is_err());
    board.complete("task", &replacement, 5022).unwrap();
    let snapshot = storage::Database::open(&config)
        .unwrap()
        .team_snapshot("team")
        .unwrap()
        .unwrap();
    assert_eq!(
        TeamBoard::restore(writer.handle(), snapshot)
            .unwrap()
            .snapshot()
            .unwrap()[0]
            .state,
        ClaimState::Complete
    );
}

#[test]
fn failed_persistence_leaves_board_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("team.db"),
        ..Default::default()
    };
    let writer = storage::Storage::open(config).unwrap();
    let board = TeamBoard::durable(writer.handle(), "team".into());
    writer.close();
    assert!(
        board
            .enqueue(TaskSpec {
                id: "task".into(),
                paths: vec![]
            })
            .is_err()
    );
    assert!(board.snapshot().unwrap().is_empty());
}
