use super::*;
use crate::lane_management::{LaneManagementAction, LaneManagementActor, LaneManagementRequest};
use crate::lanes::*;

struct Fixture {
    _dir: tempfile::TempDir,
    db: crate::Database,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::Database::open(&crate::StorageConfig {
            db_path: dir.path().join("instructions.db"),
            ..Default::default()
        })
        .unwrap();
        let f = Self { _dir: dir, db };
        f.root("lead", "lead-root");
        f.root("other", "other-root");
        crate::repo::lanes::create(f.c(), &new_lane()).unwrap();
        f
    }
    fn started() -> Self {
        let f = Self::new();
        crate::repo::lanes::start_run(f.c(), "lane", 0, "lane-root", 2).unwrap();
        f
    }
    fn c(&self) -> &Connection {
        &self.db.conn
    }
    fn root(&self, thread: &str, run: &str) {
        self.c().execute("INSERT INTO threads(thread_id,root_run_id,chat_role,project_id,source,created_at_ns,updated_at_ns) VALUES (?1,?2,'Orchestrator','project','host',1,1)",params![thread,run]).unwrap();
        self.c().execute("INSERT INTO run_contexts(run_id,role,name,config_json,checkpoints_json,terminal_phase,restorable,updated_at_ns) VALUES (?1,'Orchestrator',?2,'{}','[]','Done',1,1)",params![run,format!("chat:Orchestrator:{thread}")]).unwrap();
    }
    fn lane(&self) -> LaneRecord {
        lane(self.c(), "lane").unwrap()
    }
    fn count(&self, table: &str) -> i64 {
        self.c()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }
    fn request_revision(&self) -> i64 {
        self.c()
            .query_row(
                "SELECT request_revision FROM lanes WHERE lane_id='lane'",
                [],
                |r| r.get(0),
            )
            .unwrap()
    }
    fn dump(&self) -> String {
        let mut values = vec![
            json(&self.lane()).unwrap(),
            self.request_revision().to_string(),
        ];
        for sql in [
            "SELECT body FROM lane_instructions ORDER BY instruction_seq",
            "SELECT payload||state FROM lane_notifications ORDER BY notification_id",
            "SELECT result_json FROM lane_instruction_receipts ORDER BY operation_id",
            "SELECT CAST(revision AS TEXT) FROM lane_state",
        ] {
            let mut s = self.c().prepare(sql).unwrap();
            values.extend(
                s.query_map([], |r| r.get::<_, String>(0))
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap(),
            );
        }
        values.join("\n")
    }
}
fn brief(objective: &str) -> LaneBrief {
    LaneBrief {
        objective: objective.into(),
        inputs: vec![],
        deliverables: vec!["artifact".into()],
        acceptance_criteria: vec!["test".into()],
        constraints: vec![],
        workspace: LaneWorkspaceDeclaration {
            branch: None,
            worktree: None,
            notes: None,
        },
        report_policy: LanePolicy::Attached,
        verification_requirements: Default::default(),
    }
}
fn new_lane() -> NewLane {
    NewLane {
        lane_id: "lane".into(),
        thread_id: "thread".into(),
        origin_thread_id: "lead".into(),
        origin_run_id: "lead-root".into(),
        creation_key: "open".into(),
        brief: brief("work"),
        start_seed: "seed".into(),
        project_id: Some("project".into()),
        project_root: None,
        title: None,
        at_ns: 1,
    }
}
fn lead() -> LaneInstructionActor {
    LaneInstructionActor {
        thread_id: "lead".into(),
        root_run_id: "lead-root".into(),
    }
}
fn request(
    id: &str,
    management: u64,
    brief_revision: u64,
    action: LaneInstructionAction,
) -> LaneInstructionRequest {
    LaneInstructionRequest {
        operation_id: id.into(),
        lane_id: "lane".into(),
        expected_management_revision: management,
        expected_brief_revision: brief_revision,
        action,
    }
}
fn supplement(text: &str) -> LaneInstructionAction {
    LaneInstructionAction::Supplement {
        message: text.into(),
    }
}
fn revise(objective: &str) -> LaneInstructionAction {
    LaneInstructionAction::ReviseBrief {
        brief: brief(objective),
    }
}
fn code(error: StorageError) -> String {
    match error {
        StorageError::LaneManagement(reason) => reason.as_str().into(),
        StorageError::StaleRevision { kind, .. } => format!("stale_{kind}"),
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn a_supplement_leaves_the_brief_and_close_evidence_while_a_revision_retires_them() {
    let f = Fixture::started();
    f.c()
        .execute(
            "UPDATE lanes SET current_close_id='close' WHERE lane_id='lane'",
            [],
        )
        .unwrap();
    let before = f.request_revision();

    let first = apply(
        f.c(),
        &request("s1", 1, 1, supplement("prefer the small fix")),
        &lead(),
        10,
    )
    .unwrap();
    assert_eq!(
        (first.instruction_seq, first.brief_revision, first.changed),
        (Some(1), 1, true)
    );
    assert_eq!(f.lane().brief_revision, 1);
    assert_eq!(f.request_revision(), before);
    let close: Option<String> = f
        .c()
        .query_row(
            "SELECT current_close_id FROM lanes WHERE lane_id='lane'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(close.as_deref(), Some("close"));

    let second = apply(
        f.c(),
        &request("r1", 1, 1, revise("new objective")),
        &lead(),
        11,
    )
    .unwrap();
    assert_eq!(
        (second.instruction_seq, second.brief_revision),
        (Some(2), 2)
    );
    assert_eq!(f.lane().brief.objective, "new objective");
    assert_eq!(f.request_revision(), before + 1);
    let close: Option<String> = f
        .c()
        .query_row(
            "SELECT current_close_id FROM lanes WHERE lane_id='lane'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(close, None);

    let unchanged = apply(
        f.c(),
        &request("r2", 1, 2, revise("new objective")),
        &lead(),
        12,
    )
    .unwrap();
    assert_eq!(
        (
            unchanged.changed,
            unchanged.instruction_seq,
            unchanged.brief_revision
        ),
        (false, None, 2)
    );
    assert_eq!(f.count("lane_instructions"), 2);
    assert_eq!(f.count("lane_notifications"), 2);
    assert_eq!(f.count("lane_instruction_receipts"), 3);

    let texts = list(f.c(), "lane", 0, 10).unwrap();
    assert_eq!(
        texts.iter().map(|t| t.instruction_seq).collect::<Vec<_>>(),
        [1, 2]
    );
    assert!(
        texts[0].body.contains("prefer the small fix")
            && texts[0].body.contains("not a real-user request")
    );
    assert!(
        texts[1].body.contains("new objective")
            && texts[1].body.contains("\"previous_brief_revision\":1")
    );
    assert_eq!(
        delivery(f.c(), "lane", 1).unwrap(),
        Some(LaneInstructionDelivery::Queued)
    );
}

#[test]
fn a_retry_replays_once_and_a_reused_operation_id_conflicts() {
    let f = Fixture::started();
    let r = request("op", 1, 1, supplement("one"));
    let first = apply(f.c(), &r, &lead(), 10).unwrap();
    let again = apply(f.c(), &r, &lead(), 11).unwrap();
    assert!(!first.replayed && again.replayed);
    assert_eq!(
        (first.instruction_seq, first.notification_id.clone()),
        (again.instruction_seq, again.notification_id)
    );
    assert_eq!(
        (f.count("lane_instructions"), f.count("lane_notifications")),
        (1, 1)
    );
    let other = request("op", 1, 1, supplement("two"));
    assert_eq!(
        code(apply(f.c(), &other, &lead(), 12).unwrap_err()),
        "operation_id_conflict"
    );
}

#[test]
fn only_the_current_lead_root_of_a_started_lane_with_both_revisions_may_send() {
    let f = Fixture::new();
    assert_eq!(
        code(apply(f.c(), &request("early", 1, 1, supplement("x")), &lead(), 3).unwrap_err()),
        "lane_not_started"
    );
    crate::repo::lanes::start_run(f.c(), "lane", 0, "lane-root", 2).unwrap();

    let impostor = LaneInstructionActor {
        thread_id: "other".into(),
        root_run_id: "other-root".into(),
    };
    let stale_root = LaneInstructionActor {
        thread_id: "lead".into(),
        root_run_id: "other-root".into(),
    };
    let lane_itself = LaneInstructionActor {
        thread_id: "thread".into(),
        root_run_id: "lane-root".into(),
    };
    for actor in [&impostor, &stale_root, &lane_itself] {
        assert_eq!(
            code(apply(f.c(), &request("who", 1, 1, supplement("x")), actor, 3).unwrap_err()),
            "not_current_lead"
        );
    }
    assert_eq!(
        code(apply(f.c(), &request("m", 9, 1, supplement("x")), &lead(), 3).unwrap_err()),
        "stale_management_revision"
    );
    assert_eq!(
        code(apply(f.c(), &request("b", 1, 9, supplement("x")), &lead(), 3).unwrap_err()),
        "stale_brief"
    );
    assert_eq!(
        code(apply(f.c(), &request("b2", 1, 9, revise("y")), &lead(), 3).unwrap_err()),
        "stale_brief"
    );
    assert_eq!(
        code(apply(f.c(), &request("e", 1, 1, supplement("  ")), &lead(), 3).unwrap_err()),
        "invalid_input"
    );
    let long = "x".repeat(LANE_SUPPLEMENT_MAX_BYTES + 1);
    assert_eq!(
        code(apply(f.c(), &request("l", 1, 1, supplement(&long)), &lead(), 3).unwrap_err()),
        "invalid_input"
    );
    let mut wrong_policy = brief("y");
    wrong_policy.report_policy = LanePolicy::Summary;
    let action = LaneInstructionAction::ReviseBrief {
        brief: wrong_policy,
    };
    assert!(apply(f.c(), &request("p", 1, 1, action), &lead(), 3).is_err());
    assert_eq!(
        (
            f.count("lane_instructions"),
            f.count("lane_instruction_receipts"),
            f.count("lane_notifications")
        ),
        (0, 0, 0)
    );
}

#[test]
fn every_failure_point_rolls_back_the_brief_text_outbox_and_receipt() {
    for step in 1..=6u8 {
        let f = Fixture::started();
        let before = f.dump();
        let r = request("op", 1, 1, revise("changed"));
        let mut seen = vec![];
        let result = apply_inner(f.c(), &r, &lead(), 10, |at| {
            seen.push(at);
            if at == step {
                Err(StorageError::Serialization("injected before commit".into()))
            } else {
                Ok(())
            }
        });
        assert!(result.is_err(), "step {step}");
        assert_eq!(seen, (1..=step).collect::<Vec<_>>());
        assert_eq!(f.dump(), before, "step {step} left a partial commit");
        let done = apply(f.c(), &r, &lead(), 10).unwrap();
        assert_eq!((done.instruction_seq, done.brief_revision), (Some(1), 2));
    }
}

#[test]
fn detach_keeps_accepted_instructions_for_the_lane_and_fences_the_old_lead() {
    let f = Fixture::started();
    let accepted = apply(
        f.c(),
        &request("keep", 1, 1, supplement("accepted")),
        &lead(),
        10,
    )
    .unwrap();
    let notification = accepted.notification_id.clone().unwrap();

    crate::repo::lane_management::apply(
        f.c(),
        &LaneManagementRequest {
            operation_id: "detach".into(),
            lane_id: "lane".into(),
            expected_management_revision: 1,
            action: LaneManagementAction::Detach,
        },
        &LaneManagementActor::Lead {
            thread_id: "lead".into(),
            root_run_id: "lead-root".into(),
        },
        None,
        11,
    )
    .unwrap();

    let (state, revision): (String, u64) = f
        .c()
        .query_row(
            "SELECT state,management_revision FROM lane_notifications WHERE notification_id=?1",
            [&notification],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((state.as_str(), revision), ("pending", 2));
    assert_eq!(
        delivery(f.c(), "lane", 1).unwrap(),
        Some(LaneInstructionDelivery::Queued)
    );
    assert_eq!(
        code(
            apply(
                f.c(),
                &request("late", 2, 1, supplement("new")),
                &lead(),
                12
            )
            .unwrap_err()
        ),
        "not_current_lead"
    );
    assert_eq!(
        code(
            apply(
                f.c(),
                &request("keep", 1, 1, supplement("accepted")),
                &lead(),
                12
            )
            .unwrap_err()
        ),
        "not_current_lead"
    );
    assert_eq!(f.count("lane_instructions"), 1);
}

fn ready_report() -> LaneReportInput {
    LaneReportInput {
        status: LaneStatus::Ready,
        summary: "done".into(),
        artifact_refs: vec![],
        artifact_revision: None,
        failure_episode: None,
        needs_human: false,
        question_refs: vec![],
        reason_code: None,
        producer: LaneReportProducer::LaneAgent,
    }
}
fn report_revision(f: &Fixture, at: i64) -> u64 {
    let management = f.lane().management_revision;
    crate::repo::lanes::append_report(f.c(), "lane", management, &ready_report(), at)
        .unwrap()
        .report
        .brief_revision
}
/// What the lane's conversation holds is proven by the receipt, which commits with the
/// snapshot; the ACK is a later step that may be lost.
fn receive(f: &Fixture, seq: u64) {
    f.c()
        .execute(
            "UPDATE lane_notifications SET receipt_prompt_id='receipt-'||notification_id WHERE notification_id=(SELECT notification_id FROM lane_instructions WHERE lane_id='lane' AND instruction_seq=?1)",
            [seq],
        )
        .unwrap();
}

#[test]
fn a_report_is_bound_to_the_brief_revision_the_lane_has_received() {
    let f = Fixture::started();
    assert_eq!(report_revision(&f, 5), 1);
    apply(f.c(), &request("v2", 1, 1, revise("second")), &lead(), 10).unwrap();
    apply(f.c(), &request("v3", 1, 2, revise("third")), &lead(), 11).unwrap();
    assert_eq!(f.lane().brief_revision, 3);
    // Neither revision is in the conversation: the lane still answers the first brief.
    assert_eq!(report_revision(&f, 12), 1);
    // Delivery is in order: a received first revision leaves the second unseen. The ACK
    // was never sent, yet the receipt counts.
    receive(&f, 1);
    assert_eq!(report_revision(&f, 13), 2);
    assert_eq!(
        delivery(f.c(), "lane", 1).unwrap(),
        Some(LaneInstructionDelivery::Delivered)
    );
    receive(&f, 2);
    assert_eq!(report_revision(&f, 14), 3);
    // A supplement changes no revision, whether or not it was received.
    apply(f.c(), &request("s1", 1, 3, supplement("note")), &lead(), 15).unwrap();
    assert_eq!(report_revision(&f, 16), 3);
}
