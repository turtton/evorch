use super::*;
use crate::Database;

const LEGACY_FOR_RUN_SQL: &str = "SELECT q.payload FROM user_questions q WHERE ?1 NOT IN (SELECT run_id FROM run_tombstones) AND ((q.run_id=?1 AND q.root_run_id NOT IN (SELECT run_id FROM run_tombstones)) OR EXISTS (SELECT 1 FROM user_question_links l WHERE l.question_id=q.id AND l.run_id != q.run_id AND l.run_id=?1)) ORDER BY q.rowid";

fn ask(conn: &Connection, id: &str, run: &str, root: &str) {
    create(
        conn,
        &UserQuestion {
            responder: None,
            routing_revision: 0,
            management_revision: None,
            id: id.into(),
            run_id: run.into(),
            root_run_id: root.into(),
            root_name: "chat:Worker:thread".into(),
            recipient_run_ids: Vec::new(),
            title: format!("question {id}"),
            options: vec!["A".into()],
            blocking: true,
            answer: None,
            answer_origin: None,
            cancelled: false,
        },
    )
    .unwrap();
}

fn link(conn: &Connection, question: &str, run: &str) {
    conn.execute(
        "INSERT INTO user_question_links(question_id, run_id) VALUES (?1, ?2)",
        [question, run],
    )
    .unwrap();
}

fn tombstone(conn: &Connection, run: &str) {
    conn.execute(
        "INSERT INTO run_tombstones(run_id, thread_id, family_key) VALUES (?1, 'thread', 'family')",
        [run],
    )
    .unwrap();
}

fn ids(questions: Vec<UserQuestion>) -> Vec<String> {
    questions.into_iter().map(|q| q.id).collect()
}

#[test]
fn for_run_returns_the_legacy_rows_in_the_legacy_order() {
    let database = Database::open_in_memory().unwrap();
    let conn = &database.conn;
    ask(conn, "linked-early", "run-b", "run-b");
    ask(conn, "owned", "run-a", "run-a");
    ask(conn, "self-linked", "run-a", "run-a");
    ask(conn, "dead-root", "run-a", "root-gone");
    ask(conn, "dead-owner", "owner-gone", "owner-gone");
    ask(conn, "doomed-owned", "run-gone", "run-gone");
    ask(conn, "linked-late", "run-b", "run-b");
    ask(conn, "unrelated", "run-b", "run-b");
    link(conn, "linked-late", "run-a");
    link(conn, "linked-early", "run-a");
    link(conn, "self-linked", "run-a");
    link(conn, "dead-root", "run-c");
    link(conn, "dead-owner", "run-a");
    link(conn, "linked-early", "run-gone");
    link(conn, "owned", "run-c");
    for run in ["root-gone", "owner-gone", "run-gone"] {
        tombstone(conn, run);
    }

    for (run, expected) in [
        (
            "run-a",
            &[
                "linked-early",
                "owned",
                "self-linked",
                "dead-owner",
                "linked-late",
            ][..],
        ),
        ("run-b", &["linked-early", "linked-late", "unrelated"][..]),
        ("run-c", &["owned", "dead-root"][..]),
        ("run-gone", &[][..]),
        ("root-gone", &[][..]),
        ("owner-gone", &[][..]),
        ("nobody", &[][..]),
    ] {
        let legacy = ids(list(conn, LEGACY_FOR_RUN_SQL, run).unwrap());
        assert_eq!(legacy, expected, "legacy {run}");
        assert_eq!(ids(for_run(conn, run).unwrap()), legacy, "{run}");
    }
}

#[test]
fn for_run_searches_both_run_indexes_instead_of_scanning_questions() {
    let database = Database::open_in_memory().unwrap();
    let plan = database
        .conn
        .prepare(&format!("EXPLAIN QUERY PLAN {FOR_RUN_SQL}"))
        .unwrap()
        .query_map(["run-a"], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        !plan
            .iter()
            .any(|step| step.starts_with("SCAN q") || step.starts_with("SCAN l")),
        "{plan:?}"
    );
    for index in ["user_questions_run", "user_question_links_run"] {
        assert!(
            plan.iter()
                .any(|step| step.starts_with("SEARCH") && step.contains(index)),
            "{index}: {plan:?}"
        );
    }
}
