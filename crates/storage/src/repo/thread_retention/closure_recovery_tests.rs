use super::*;

#[test]
fn recovery_edge_paging_charges_at_most_the_component_size_per_node() {
    for count in [0_u32, 1, 2, 513, 1025] {
        let db = crate::Database::open_in_memory().unwrap();
        db.conn.execute(
            "INSERT INTO recovery_manifests(manifest_id,schema_version,committed,member_count,edge_count,edges_json)
             VALUES('live',1,1,?1,0,'[]')", [count],
        ).unwrap();
        db.conn.execute(
            "WITH RECURSIVE n(i) AS (
               SELECT 0 WHERE ?1>0 UNION ALL SELECT i+1 FROM n WHERE i+1<?1
             ) INSERT INTO recovery_members(manifest_id,run_id,incarnation,intent_revision,resume,record_json)
             SELECT 'live','run-'||i,'old',0,1,'{}' FROM n", [count],
        ).unwrap();
        let mut position = Position {
            step: (RUN_STEPS.len() - 1) as u32,
            after: 0,
        };
        let mut charged = 0;
        let mut neighbors = BTreeSet::new();
        loop {
            let limit = charged + 3;
            match expand_node(
                &db.conn,
                NodeKind::Run,
                "run-0",
                position,
                &mut charged,
                limit,
                &mut |neighbor| {
                    let Neighbor::Run(id) = neighbor else {
                        panic!("unexpected edge")
                    };
                    assert!(neighbors.insert(id), "a resumed page must not repeat a run");
                    Ok(true)
                },
            )
            .unwrap()
            {
                Expansion::Done => break,
                Expansion::Budget(next) => position = next,
                Expansion::Stopped => panic!("unexpected stop"),
            }
        }
        assert_eq!(neighbors.len(), (count as usize).saturating_sub(1));
        assert!(charged >= (count.saturating_sub(1)).max(1) as u64);
        assert!(
            charged <= count.max(1) as u64,
            "{charged} exceeds component size {count}"
        );
        db.conn
            .execute("UPDATE recovery_manifests SET superseded=1", [])
            .unwrap();
        let mut charged = 0;
        let result = expand_node(
            &db.conn,
            NodeKind::Run,
            "run-0",
            Position {
                step: (RUN_STEPS.len() - 1) as u32,
                after: 0,
            },
            &mut charged,
            u64::MAX,
            &mut |_| panic!("retired history is not a live edge"),
        )
        .unwrap();
        assert_eq!(result, Expansion::Done);
        assert_eq!(charged, 1);
    }
}
