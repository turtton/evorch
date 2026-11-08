use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

fn expansion_cost(unrelated: usize) -> [u64; 2] {
    let dir = tempfile::tempdir().unwrap();
    let config = crate::StorageConfig {
        db_path: dir.path().join("closure.db"),
        ..Default::default()
    };
    drop(crate::Database::open_unchecked(&config).unwrap());
    let mut c = Connection::open(&config.db_path).unwrap();
    c.pragma_update(None, "foreign_keys", true).unwrap();
    let tx = c.transaction().unwrap();
    tx.execute(
        "INSERT INTO usage_requests(request_id,at_ns,provider,model,run_id,parent_run_id,status)
         VALUES ('request',1,'test','test','child','parent','ok')",
        [],
    )
    .unwrap();
    // Populate the real ledger/indexes, but no neighbors of the expanded runs.
    // Metadata payloads are opaque to closure traversal.
    tx.execute(
        "WITH RECURSIVE n(i) AS (
          SELECT 1 WHERE 1<=?1 UNION ALL SELECT i+1 FROM n WHERE i<?1
         )
         INSERT INTO lanes(lane_id,thread_id,origin_thread_id,origin_run_id,creation_key,
          request_hash,scope,policy,management_revision,brief_json,brief_revision,start_seed,
          root_run_id,generation,created_at_ns,updated_at_ns,last_activity_at_ns)
         SELECT 'lane-'||i,'thread-'||i,'origin','origin-run','key-'||i,
          'hash','lane','detached',1,'{}',1,'seed','run-'||i,1,1,1,1 FROM n",
        [i64::try_from(unrelated).unwrap()],
    )
    .unwrap();
    tx.execute(
        "INSERT INTO thread_runs(run_id,thread_id,root_run_id,depth)
         SELECT root_run_id,thread_id,root_run_id,0 FROM lanes",
        [],
    )
    .unwrap();
    tx.commit().unwrap();
    let mut costs = [0; 2];
    for (index, (run, expected)) in [("parent", "child"), ("child", "parent")]
        .into_iter()
        .enumerate()
    {
        let steps = Arc::new(AtomicU64::new(0));
        let count = steps.clone();
        c.progress_handler(
            1,
            Some(move || {
                count.fetch_add(1, Ordering::Relaxed);
                false
            }),
        )
        .unwrap();
        let mut neighbors = Vec::new();
        let mut edges = 0;
        let result = expand_node(
            &c,
            NodeKind::Run,
            run,
            Position::default(),
            &mut edges,
            u64::MAX,
            &mut |neighbor| {
                neighbors.push(neighbor);
                Ok(true)
            },
        )
        .unwrap();
        c.progress_handler(0, None::<fn() -> bool>).unwrap();
        assert_eq!(result, Expansion::Done);
        assert_eq!(neighbors, [Neighbor::Run(expected.into())]);
        costs[index] = steps.load(Ordering::Relaxed);
    }
    costs
}

#[test]
fn usage_provenance_exclusion_does_not_scan_unrelated_lanes_in_either_direction() {
    let small = expansion_cost(0);
    let large = expansion_cost(20_000);
    assert!(small.into_iter().all(|steps| steps > 0));
    assert_eq!(
        large, small,
        "A bounded node expansion must not materialize unrelated lane roots or members"
    );
}
