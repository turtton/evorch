//! Handoff provenance and recipient questions are not family membership edges.
use super::*;
#[test]
fn handoff_origin_and_usage_parent_do_not_join_independent_families() {
    for archived_source in [true, false] {
        let f = Fixture::new();
        f.thread("source", None, Some("source-run"), None);
        f.thread("target", None, Some("target-run"), Some("source-run"));
        f.run("source-run", None, Some("source"), "chat:Worker:source");
        f.run(
            "target-run",
            None,
            Some("target"),
            "chat:Orchestrator:target",
        );
        f.managed_lane("handoff", "target", None);
        f.sql("INSERT INTO usage_requests(request_id,at_ns,provider,model,run_id,parent_run_id,status) VALUES ('usage',1,'p','m','target-run','source-run','ok');");
        let database = f.database();
        let input = if archived_source {
            sidebar(&["source"], &["target"])
        } else {
            sidebar(&["target"], &["source"])
        };
        let states = settle(&database, &input, &policy(), BIG);
        let expected = if archived_source { "source" } else { "target" };
        assert_eq!(explored_threads(&states), vec![vec![expected.to_owned()]]);
        assert!(matches!(
            finalize(&database, expected, &input),
            FinalizeOutcome::Confirmed(_)
        ));
        assert_eq!(f.count("SELECT COUNT(*) FROM thread_tombstones"), 1);
        assert_eq!(f.count("SELECT COUNT(*) FROM run_tombstones"), 1);
    }
}
#[test]
fn source_reclaim_keeps_live_recipient_question_and_answer_then_last_recipient_collects_it() {
    for answer_before_reclaim in [true, false] {
        let f = Fixture::new();
        f.thread("source", None, Some("source-run"), None);
        f.thread("target", None, Some("target-run"), Some("source-run"));
        f.run("source-run", None, Some("source"), "chat:Worker:source");
        f.run(
            "target-run",
            None,
            Some("target"),
            "chat:Orchestrator:target",
        );
        f.managed_lane("handoff", "target", None);
        let storage = Storage::open(config(&f.path)).unwrap();
        let h = storage.handle();
        let q = event_bus::UserQuestion {
            id: "inherited".into(),
            run_id: "source-run".into(),
            root_run_id: "source-run".into(),
            root_name: "chat:Worker:source".into(),
            recipient_run_ids: vec![],
            title: "choose scope".into(),
            options: vec![],
            blocking: true,
            answer: None,
            answer_origin: None,
            responder: None,
            routing_revision: 0,
            management_revision: None,
        };
        h.create_user_question(&q).unwrap();
        h.bind_user_questions("source-run", "target-run", std::slice::from_ref(&q.id))
            .unwrap();
        let origin = event_bus::AnswerOrigin::Host {
            channel: Default::default(),
        };
        if answer_before_reclaim {
            h.answer_user_question(&q.id, "accepted", &origin).unwrap();
        }
        let db = f.database();
        let input = sidebar(&["source"], &["target"]);
        settle(&db, &input, &policy(), BIG);
        assert!(matches!(
            finalize(&db, "source", &input),
            FinalizeOutcome::Confirmed(_)
        ));
        drop(db);
        let progress = bounded_steps(&storage);
        assert_eq!(progress.jobs_db_pending, 0);
        assert_eq!(progress.last_error, None);
        assert_eq!(
            f.count("SELECT COUNT(*) FROM run_contexts WHERE run_id='source-run'"),
            0
        );
        assert_eq!(
            f.count("SELECT COUNT(*) FROM user_questions WHERE id='inherited'"),
            1
        );
        assert_eq!(
            f.count("SELECT COUNT(*) FROM user_question_links WHERE run_id='target-run'"),
            1
        );
        // The immutable handoff source is provenance, not an update fence on
        // an existing live lane after its source has been reclaimed.
        let mut mirror = storage::threads::ThreadUpsert {
            thread_id: "target".into(),
            root_run_id: Some("target-run".into()),
            escalation_source_run_id: Some("source-run".into()),
            title: Some("runtime independent target".into()),
            updated_at_ns: 50,
            ..Default::default()
        };
        h.upsert_thread_runtime(&mirror).unwrap();
        mirror.title = Some("host independent target".into());
        assert_eq!(
            h.upsert_threads_host(std::slice::from_ref(&mirror))
                .unwrap(),
            0
        );
        assert_eq!(f.count("SELECT COUNT(*) FROM threads WHERE thread_id='target' AND title='host independent target' AND escalation_source_run_id='source-run'"), 1);
        let c = Connection::open(&f.path).unwrap();
        c.execute("INSERT INTO run_tombstones(run_id,thread_id,family_key) VALUES ('another-dead-source','gone','gone')", []).unwrap();
        assert!(c.execute("UPDATE threads SET escalation_source_run_id='another-dead-source' WHERE thread_id='target'", []).is_err());
        assert!(c.execute("INSERT INTO threads(thread_id,escalation_source_run_id) VALUES ('new-target','source-run')", []).is_err());
        drop(c);
        mirror.escalation_source_run_id = Some("another-dead-source".into());
        assert!(matches!(
            h.upsert_thread_runtime(&mirror),
            Err(storage::StorageError::Tombstoned { .. })
        ));
        assert_eq!(
            h.upsert_threads_host(std::slice::from_ref(&mirror))
                .unwrap(),
            1
        );
        mirror.thread_id = "new-target".into();
        mirror.root_run_id = None;
        mirror.escalation_source_run_id = Some("source-run".into());
        assert!(matches!(
            h.upsert_thread_runtime(&mirror),
            Err(storage::StorageError::Tombstoned { .. })
        ));
        assert_eq!(
            h.upsert_threads_host(std::slice::from_ref(&mirror))
                .unwrap(),
            1
        );
        if !answer_before_reclaim {
            h.answer_user_question(&q.id, "accepted", &origin).unwrap();
        }
        let db = f.database();
        let qs = db.user_questions_for_run("target-run").unwrap();
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].id, q.id);
        assert_eq!(qs[0].run_id, q.run_id);
        assert_eq!(qs[0].answer, Some("accepted".into()));
        assert_eq!(qs[0].recipient_run_ids, vec!["target-run"]);
        assert!(db.user_questions_for_run("source-run").unwrap().is_empty());
        // Surviving recipient is the only return destination; provenance is retained.
        assert_eq!(
            db.user_question_recipients(&q.id).unwrap(),
            vec!["target-run"]
        );
        let input = sidebar(&["target"], &[]);
        settle(&db, &input, &policy(), BIG);
        assert!(matches!(
            finalize(&db, "target", &input),
            FinalizeOutcome::Confirmed(_)
        ));
        drop(db);
        let progress = bounded_steps(&storage);
        assert_eq!(progress.jobs_db_pending, 0);
        assert_eq!(progress.last_error, None);
        assert_eq!(f.count("SELECT COUNT(*) FROM user_questions"), 0);
        assert_eq!(f.count("SELECT COUNT(*) FROM user_question_links"), 0);
        storage.close();
    }
}

#[test]
fn topology_change_after_staging_discards_old_closure_and_reseeds_separate_ages() {
    for finalize_before_explore in [true, false] {
        let f = Fixture::new();
        f.thread("lead", None, Some("rl"), None);
        f.thread("lane", None, Some("ra"), None);
        f.run("rl", None, Some("lead"), "chat:Orchestrator:lead");
        f.run("ra", None, Some("lane"), "chat:Orchestrator:lane");
        f.managed_lane("managed", "lane", Some("lead"));
        let db = f.database();
        let input = sidebar(&["lead", "lane"], &[]);
        assert_eq!(
            explored_threads(&settle(&db, &input, &policy(), BIG)),
            vec![vec!["lane".to_owned(), "lead".to_owned()]]
        );
        // The durable management commit is simulated here; no sleeps, all order
        // boundaries are explicit. A new topology must shrink, not union staging.
        f.sql("UPDATE lanes SET lead_thread_id=NULL, policy='detached', management_revision=2, detached_since_management_revision=2 WHERE lane_id='managed'; UPDATE lane_state SET family_topology_revision=family_topology_revision+1;");
        if finalize_before_explore {
            assert!(matches!(
                finalize(&db, "lane", &input),
                FinalizeOutcome::Blocked(BlockReason::Exploring)
            ));
            assert_eq!(f.count("SELECT COUNT(*) FROM thread_tombstones"), 0);
        }
        // Until the file + mirror ACK, even an otherwise due family is held.
        assert!(explored_threads(&settle(&db, &input, &policy(), BIG)).is_empty());
        f.sql("UPDATE lanes SET management_projection_revision=management_revision WHERE lane_id='managed'");
        let mut independent = sidebar(&["lead", "lane"], &[]);
        independent
            .threads
            .iter_mut()
            .find(|t| t.thread_id == "lane")
            .unwrap()
            .archived_at_s = Some(NOW_NS / 1_000_000_000);
        let states = settle(&db, &independent, &policy(), BIG);
        assert_eq!(explored_threads(&states), vec![vec!["lead".to_owned()]]);
        assert!(matches!(
            finalize(&db, "lead", &independent),
            FinalizeOutcome::Confirmed(_)
        ));
        assert_eq!(
            f.count("SELECT COUNT(*) FROM thread_tombstones WHERE thread_id='lane'"),
            0
        );
        independent.now_ns += 100 * 86_400 * 1_000_000_000;
        let states = settle(&db, &independent, &policy(), BIG);
        assert!(explored_threads(&states).contains(&vec!["lane".to_owned()]));
        assert!(matches!(
            finalize(&db, "lane", &independent),
            FinalizeOutcome::Confirmed(_)
        ));
    }
}
