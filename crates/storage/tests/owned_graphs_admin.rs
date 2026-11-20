//! Read-only observations never reconstruct an authority or migrate a database.
#[path = "quarantine_admin/fixture.rs"]
#[allow(
    dead_code,
    reason = "reuse the public storage fixture without its discard operations"
)]
mod fixture;

#[test]
fn owned_graph_reads_do_not_create_a_missing_database() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("absent.db"),
        ..Default::default()
    };
    assert!(
        storage::Database::read_owned_graphs(&config, |db| db.owned_graph_snapshots()).is_err()
    );
    assert!(!config.db_path.exists());
}

#[test]
fn owned_snapshot_is_consistent_and_incomplete_membership_fails_closed() {
    let f = fixture::Fixture::new();
    f.seed(true);
    f.transaction(true);
    storage::Database::read_owned_graphs(&f.config, |db| {
        let before = db.owned_graph_snapshots()?;
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].cas.membership.len(), 2);
        assert!(before[0].cas.claim.is_some());
        f.sql()
            .execute("UPDATE restart_components SET revision=revision+1", [])
            .unwrap();
        let during = db.owned_graph_snapshots()?;
        assert_eq!(before[0].cas, during[0].cas);
        Ok(())
    })
    .unwrap();
    let fresh =
        storage::Database::read_owned_graphs(&f.config, |db| db.owned_graph_snapshots()).unwrap();
    assert_eq!(fresh[0].cas.component.revision, 1);
    f.sql()
        .execute(
            "DELETE FROM restart_expected_members WHERE run_id='run-b'",
            [],
        )
        .unwrap();
    assert!(
        storage::Database::read_owned_graphs(&f.config, |db| db.owned_graph_snapshots()).is_err()
    );
}

#[test]
fn per_thread_progress_has_four_distinct_current_format_states() {
    use storage::recovery::graph_progress::GraphDisposalStep::*;
    let values =
        [NotStarted, Applied, NotApplicable, Unknown].map(|s| serde_json::to_string(&s).unwrap());
    assert_eq!(
        values
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        4
    );
}
