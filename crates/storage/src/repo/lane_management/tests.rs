use super::*;
use crate::lane_delivery::*;
use crate::lanes::*;
use event_bus::UserQuestion;

struct Fixture {
    _dir: tempfile::TempDir,
    db: crate::Database,
}
impl Fixture {
    fn new(policy: LanePolicy) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::Database::open(&crate::StorageConfig {
            db_path: dir.path().join("management.db"),
            ..Default::default()
        })
        .unwrap();
        let f = Self { _dir: dir, db };
        f.root("lead", "lead-root", "project");
        f.root("target", "target-root", "project");
        crate::repo::lanes::create(
            f.c(),
            &NewLane {
                lane_id: "lane".into(),
                thread_id: "thread".into(),
                origin_thread_id: "lead".into(),
                origin_run_id: "lead-root".into(),
                creation_key: "open".into(),
                brief: LaneBrief {
                    objective: "work".into(),
                    inputs: vec![],
                    deliverables: vec!["artifact".into()],
                    acceptance_criteria: vec!["test".into()],
                    constraints: vec![],
                    workspace: LaneWorkspaceDeclaration {
                        branch: None,
                        worktree: None,
                        notes: None,
                    },
                    report_policy: policy,
                    verification_requirements: Default::default(),
                },
                start_seed: "seed".into(),
                project_id: Some("project".into()),
                project_root: None,
                title: None,
                at_ns: 1,
            },
        )
        .unwrap();
        f
    }
    fn c(&self) -> &Connection {
        &self.db.conn
    }
    fn root(&self, thread: &str, run: &str, project: &str) {
        self.c().execute("INSERT INTO threads(thread_id,root_run_id,chat_role,project_id,source,created_at_ns,updated_at_ns) VALUES (?1,?2,'Orchestrator',?3,'host',1,1)",params![thread,run,project]).unwrap();
        self.c().execute("INSERT INTO run_contexts(run_id,role,name,config_json,checkpoints_json,terminal_phase,restorable,updated_at_ns) VALUES (?1,'Orchestrator',?2,'{}','[]','Done',1,1)",params![run,format!("chat:Orchestrator:{thread}")]).unwrap();
    }
    fn request(
        &self,
        id: &str,
        revision: u64,
        action: LaneManagementAction,
    ) -> LaneManagementRequest {
        LaneManagementRequest {
            operation_id: id.into(),
            lane_id: "lane".into(),
            expected_management_revision: revision,
            action,
        }
    }
    fn apply(&self, id: &str, revision: u64, action: LaneManagementAction) -> LaneManagementResult {
        apply(
            self.c(),
            &self.request(id, revision, action),
            &LaneManagementActor::User,
            Some(LaneAttachGuard {
                pinned: false,
                archived: false,
            }),
            10,
        )
        .unwrap()
    }
    fn question(&self) -> UserQuestion {
        let q = UserQuestion {
            id: "q".into(),
            run_id: "run".into(),
            root_run_id: "run".into(),
            root_name: "chat:Orchestrator:thread".into(),
            recipient_run_ids: vec![],
            title: "choose".into(),
            options: vec![],
            blocking: true,
            answer: None,
            answer_origin: None,
            responder: None,
            routing_revision: 0,
            management_revision: None,
        };
        crate::repo::lane_delivery::create_question(
            self.c(),
            "lane",
            q,
            QuestionRequester {
                run_id: "run".into(),
                root_run_id: "run".into(),
                thread_id: "thread".into(),
            },
            LaneDeliveryBinding::Bound,
            2,
        )
        .unwrap();
        crate::repo::user_questions::get(self.c(), "q")
            .unwrap()
            .unwrap()
    }
    fn notice(&self, kind: LaneNotificationKind, destination: &str) -> String {
        crate::repo::lane_delivery::enqueue(
            self.c(),
            LaneNotification {
                notification_id: String::new(),
                lane_id: "lane".into(),
                management_revision: 1,
                source_thread_id: "thread".into(),
                destination_thread_id: destination.into(),
                kind,
                report_seq: None,
                created_at_ns: 2,
            },
        )
        .unwrap()
    }
    fn control_state(&self) {
        use event_bus::lane::{
            LaneNudgeState, LaneProgressAnchor, LaneStallClassification, LaneStallEpisode,
        };
        let notification_id = self.notice(
            LaneNotificationKind::Nudge {
                episode_id: "episode".into(),
            },
            "thread",
        );
        let e = LaneStallEpisode {
            episode_id: "episode".into(),
            lane_id: "lane".into(),
            management_revision: 1,
            revision: 1,
            classification: LaneStallClassification::SilentDone,
            progress_anchor: LaneProgressAnchor { epoch: 0 },
            first_detected_at_ns: 2,
            resolved_at_ns: None,
            nudge_state: LaneNudgeState::Pending,
            notification_id: Some(notification_id),
        };
        self.c().execute("INSERT INTO lane_stall_episodes(episode_id,lane_id,management_revision,progress_epoch,payload) VALUES ('episode','lane',1,0,?1)", [json(&e).unwrap()]).unwrap();
        self.c().execute_batch("INSERT INTO lane_delivery_destinations(thread_id,claim_epoch,lease_token,lease_until_ns) VALUES ('lead',3,'lease',100); UPDATE threads SET pinned=1,archived=1,pin_revision=7,archive_revision=8 WHERE thread_id='thread';
            UPDATE lanes SET current_close_id='previous-close' WHERE lane_id='lane';
            INSERT INTO lane_list_snapshots(snapshot_id,filter_hash,lead_thread_id,as_of_revision) VALUES (1,'filter','lead',1);
            INSERT INTO lane_list_snapshot_rows(snapshot_id,position,lane_id) VALUES (1,0,'lane');").unwrap();
    }
    fn dump(&self) -> String {
        let l = lane(self.c(), "lane").unwrap();
        let mut values = vec![
            json(&l).unwrap(),
            topology_revision(self.c()).unwrap().to_string(),
        ];
        for sql in [
            "SELECT payload FROM lane_question_routes ORDER BY question_id",
            "SELECT payload||state||coalesce(claim_epoch,0) FROM lane_notifications ORDER BY notification_id",
            "SELECT payload FROM lane_stall_episodes ORDER BY episode_id",
            "SELECT json_array(thread_id,epoch,stopped,owner_json,claim_epoch,lease_token,lease_until_ns) FROM lane_delivery_destinations ORDER BY thread_id",
            "SELECT result_json FROM lane_management_receipts ORDER BY operation_id",
            "SELECT coalesce(current_close_id,'')||management_projection_revision FROM lanes",
            "SELECT json_array(pinned,archived,pin_revision,archive_revision) FROM threads ORDER BY thread_id",
            "SELECT json_array(snapshot_id,filter_hash,lead_thread_id,as_of_revision) FROM lane_list_snapshots ORDER BY snapshot_id",
            "SELECT json_array(snapshot_id,position,lane_id) FROM lane_list_snapshot_rows ORDER BY snapshot_id,position",
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
fn rollback(step: u8) {
    let f = Fixture::new(LanePolicy::Attached);
    f.question();
    f.notice(LaneNotificationKind::Ready { revision: None }, "lead");
    f.control_state();
    let before = f.dump();
    let r = f.request("detach", 1, LaneManagementAction::Detach);
    let mut seen = vec![];
    let result = apply_inner(f.c(), &r, &LaneManagementActor::User, None, 10, |at| {
        seen.push(at);
        if at == step {
            Err(StorageError::Serialization("injected before commit".into()))
        } else {
            Ok(())
        }
    });
    assert!(result.is_err());
    assert_eq!(seen, (1..=step).collect::<Vec<_>>());
    assert_eq!(f.dump(), before);
    let result = apply(f.c(), &r, &LaneManagementActor::User, None, 10).unwrap();
    assert_eq!(result.lane.management_revision, 2);
    assert_eq!(result.lane.detached_since_management_revision, Some(2));
}
#[test]
fn rollback_step_1_receipt() {
    rollback(1)
}
#[test]
fn rollback_step_2_validation() {
    rollback(2)
}
#[test]
fn rollback_step_3_management() {
    rollback(3)
}
#[test]
fn rollback_step_4_routing() {
    rollback(4)
}
#[test]
fn rollback_step_5_notifications() {
    rollback(5)
}
#[test]
fn rollback_step_6_close_invalidation() {
    rollback(6)
}
#[test]
fn rollback_step_7_topology_projection() {
    rollback(7)
}
#[test]
fn rollback_step_8_result_before_commit() {
    rollback(8)
}
#[test]
fn actual_commit_failure_rolls_back_all_effects() {
    let f = Fixture::new(LanePolicy::Attached);
    f.question();
    f.control_state();
    let before = f.dump();
    f.c().commit_hook(Some(|| true)).unwrap();
    assert!(
        apply(
            f.c(),
            &f.request("detach", 1, LaneManagementAction::Detach),
            &LaneManagementActor::User,
            None,
            10
        )
        .is_err()
    );
    f.c().commit_hook(None::<fn() -> bool>).unwrap();
    assert_eq!(f.dump(), before);
    assert_eq!(
        f.apply("detach", 1, LaneManagementAction::Detach)
            .lane
            .management_revision,
        2
    );
}
#[test]
fn receipt_replay_after_commit_and_later_transition_returns_original_snapshot() {
    let f = Fixture::new(LanePolicy::Attached);
    let first = f.apply("detach", 1, LaneManagementAction::Detach);
    f.apply(
        "attach",
        2,
        LaneManagementAction::Reattach {
            lead_thread_id: "target".into(),
            expected_lead_root_run_id: "target-root".into(),
        },
    );
    let mut replay = f.apply("detach", 1, LaneManagementAction::Detach);
    assert!(replay.replayed);
    replay.replayed = false;
    assert_eq!(replay, first);
    let request = f.request("detach", 3, LaneManagementAction::Detach);
    assert_eq!(
        apply(f.c(), &request, &LaneManagementActor::User, None, 11).unwrap_err(),
        err(LaneManagementError::OperationIdConflict)
    );
    let old = f.request("detach", 1, LaneManagementAction::Detach);
    assert_eq!(
        apply(
            f.c(),
            &old,
            &LaneManagementActor::Lead {
                thread_id: "lead".into(),
                root_run_id: "lead-root".into()
            },
            None,
            11
        )
        .unwrap_err(),
        err(LaneManagementError::OperationIdConflict)
    );
}
#[test]
fn actor_and_attach_target_fences() {
    let f = Fixture::new(LanePolicy::Attached);
    let r = f.request(
        "policy",
        1,
        LaneManagementAction::SetPolicy {
            policy: ManagedLanePolicy::Summary,
        },
    );
    for (thread, run) in [
        ("thread", "run"),
        ("lead", "child"),
        ("target", "target-root"),
    ] {
        assert_eq!(
            apply(
                f.c(),
                &r,
                &LaneManagementActor::Lead {
                    thread_id: thread.into(),
                    root_run_id: run.into()
                },
                None,
                2
            )
            .unwrap_err(),
            err(LaneManagementError::NotCurrentLead)
        );
    }
    let lead = LaneManagementActor::Lead {
        thread_id: "lead".into(),
        root_run_id: "lead-root".into(),
    };
    assert!(apply(f.c(), &r, &lead, None, 2).is_ok());
    f.apply("detach", 2, LaneManagementAction::Detach);
    let r = f.request(
        "attach",
        3,
        LaneManagementAction::Reattach {
            lead_thread_id: "target".into(),
            expected_lead_root_run_id: "target-root".into(),
        },
    );
    assert_eq!(
        apply(f.c(), &r, &lead, None, 3).unwrap_err(),
        err(LaneManagementError::HumanActionRequired)
    );
    for (guard, error) in [
        (None, LaneManagementError::StaleFamilyRevision),
        (
            Some(LaneAttachGuard {
                pinned: true,
                archived: false,
            }),
            LaneManagementError::LanePinned,
        ),
        (
            Some(LaneAttachGuard {
                pinned: false,
                archived: true,
            }),
            LaneManagementError::LaneArchived,
        ),
    ] {
        assert_eq!(
            apply(f.c(), &r, &LaneManagementActor::User, guard, 3).unwrap_err(),
            err(error)
        );
    }
    let guard = Some(LaneAttachGuard {
        pinned: false,
        archived: false,
    });
    for sql in [
        "UPDATE threads SET project_id='other' WHERE thread_id='target'",
        "UPDATE threads SET project_id=NULL WHERE thread_id IN ('thread','target')",
        "UPDATE threads SET chat_role='Worker' WHERE thread_id='target'",
        "UPDATE threads SET root_run_id='new-root' WHERE thread_id='target'",
        "UPDATE run_contexts SET parent_run_id='lead-root' WHERE run_id='target-root'",
    ] {
        f.c().execute_batch(sql).unwrap();
        assert_eq!(
            apply(f.c(), &r, &LaneManagementActor::User, guard, 3).unwrap_err(),
            err(LaneManagementError::InvalidAttachTarget)
        );
        f.c().execute_batch("UPDATE threads SET project_id='project' WHERE thread_id='thread';UPDATE threads SET project_id='project',chat_role='Orchestrator',root_run_id='target-root' WHERE thread_id='target';UPDATE run_contexts SET parent_run_id=NULL WHERE run_id='target-root'").unwrap();
    }
    for (column, error) in [
        ("pinned", LaneManagementError::LanePinned),
        ("archived", LaneManagementError::LaneArchived),
    ] {
        f.c()
            .execute_batch(&format!(
                "UPDATE threads SET {column}=1 WHERE thread_id='thread'"
            ))
            .unwrap();
        assert_eq!(
            apply(f.c(), &r, &LaneManagementActor::User, guard, 3).unwrap_err(),
            err(error)
        );
        f.c()
            .execute_batch(&format!(
                "UPDATE threads SET {column}=0 WHERE thread_id='thread'"
            ))
            .unwrap();
    }
    let attached = apply(f.c(), &r, &LaneManagementActor::User, guard, 3).unwrap();
    assert_eq!(attached.lane.policy, LanePolicy::Summary);
    assert_eq!(attached.lane.lead_thread_id.as_deref(), Some("target"));
}
#[test]
fn operation_id_bounds_and_revision_race() {
    let f = Fixture::new(LanePolicy::Attached);
    for id in ["".into(), "x".repeat(257), "bad\n".into()] {
        assert_eq!(
            apply(
                f.c(),
                &f.request(&id, 1, LaneManagementAction::Detach),
                &LaneManagementActor::User,
                None,
                2
            )
            .unwrap_err(),
            err(LaneManagementError::InvalidInput)
        );
    }
    f.apply(
        &"x".repeat(256),
        1,
        LaneManagementAction::SetPolicy {
            policy: ManagedLanePolicy::Summary,
        },
    );
    assert_eq!(
        apply(
            f.c(),
            &f.request("loser", 1, LaneManagementAction::Detach),
            &LaneManagementActor::User,
            None,
            2
        )
        .unwrap_err(),
        err(LaneManagementError::StaleManagementRevision)
    );
}
#[test]
fn membership_epoch_topology_projection_are_independent_of_reports_and_policy() {
    let f = Fixture::new(LanePolicy::Detached);
    assert_eq!(
        lane(f.c(), "lane")
            .unwrap()
            .detached_since_management_revision,
        Some(1)
    );
    let a = f.apply(
        "attach",
        1,
        LaneManagementAction::Reattach {
            lead_thread_id: "target".into(),
            expected_lead_root_run_id: "target-root".into(),
        },
    );
    assert_eq!(a.family_topology_revision, 1);
    assert_eq!(a.lane.detached_since_management_revision, None);
    let a = f.apply(
        "policy",
        2,
        LaneManagementAction::SetPolicy {
            policy: ManagedLanePolicy::Attached,
        },
    );
    assert_eq!(a.family_topology_revision, 1);
    assert!(a.projection_pending);
    assert!(ack_projection(f.c(), "lane", 2).is_err());
    ack_projection(f.c(), "lane", 3).unwrap();
    assert!(pending_projections(f.c(), 20).unwrap().is_empty());
    let d = f.apply("detach", 3, LaneManagementAction::Detach);
    assert_eq!(d.family_topology_revision, 2);
    assert_eq!(d.lane.detached_since_management_revision, Some(4));
    assert_eq!(pending_projections(f.c(), 20).unwrap(), vec![d.lane]);
}
#[test]
fn a_lane_moved_to_another_lead_and_detached_there_is_found_again_from_that_lead() {
    let f = Fixture::new(LanePolicy::Attached);
    let found = |lead: &str| -> Vec<String> {
        crate::repo::lanes::detached_lanes_for(f.c(), lead)
            .unwrap()
            .into_iter()
            .map(|lane| lane.lane_id)
            .collect()
    };
    assert!(found("lead").is_empty(), "a managed lane is not detached");

    // Created by "lead", detached, handed to "target", then detached there.
    f.apply("detach-1", 1, LaneManagementAction::Detach);
    assert_eq!(found("lead"), ["lane"]);
    assert!(found("target").is_empty(), "target never managed it");
    f.apply(
        "reattach",
        2,
        LaneManagementAction::Reattach {
            lead_thread_id: "target".into(),
            expected_lead_root_run_id: "target-root".into(),
        },
    );
    assert!(found("lead").is_empty() && found("target").is_empty());
    f.apply("detach-2", 3, LaneManagementAction::Detach);
    assert_eq!(found("target"), ["lane"], "the last lead finds it again");
    assert_eq!(
        found("lead"),
        ["lane"],
        "and so does the thread it came from"
    );
    assert!(found("stranger").is_empty());

    // It can be attached to the last lead again.
    let back = f.apply(
        "reattach-2",
        4,
        LaneManagementAction::Reattach {
            lead_thread_id: "target".into(),
            expected_lead_root_run_id: "target-root".into(),
        },
    );
    assert_eq!(back.lane.lead_thread_id.as_deref(), Some("target"));
    assert!(found("target").is_empty());
}
#[test]
fn policy_routing_preserves_requester_and_does_not_steal_user_questions() {
    let f = Fixture::new(LanePolicy::Attached);
    let before = f.question();
    f.apply(
        "summary",
        1,
        LaneManagementAction::SetPolicy {
            policy: ManagedLanePolicy::Summary,
        },
    );
    let q = crate::repo::user_questions::get(f.c(), "q")
        .unwrap()
        .unwrap();
    assert_eq!(q.id, before.id);
    assert_eq!(q.run_id, before.run_id);
    assert_eq!(q.root_run_id, before.root_run_id);
    assert_eq!(q.title, before.title);
    assert_eq!(q.routing_revision, before.routing_revision + 1);
    assert_eq!(q.responder, Some(QuestionResponder::User));
    f.apply(
        "attached",
        2,
        LaneManagementAction::SetPolicy {
            policy: ManagedLanePolicy::Attached,
        },
    );
    let q = crate::repo::user_questions::get(f.c(), "q")
        .unwrap()
        .unwrap();
    assert_eq!(q.responder, Some(QuestionResponder::User));
    assert_eq!(q.routing_revision, 2);
    assert_eq!(q.management_revision, Some(3));
    f.apply("detach", 3, LaneManagementAction::Detach);
    assert_eq!(
        crate::repo::user_questions::get(f.c(), "q")
            .unwrap()
            .unwrap()
            .run_id,
        before.run_id
    );
}
#[test]
fn policy_restamps_important_notices_detach_keeps_only_accepted_answer() {
    let f = Fixture::new(LanePolicy::Attached);
    f.question();
    let kinds = vec![
        LaneNotificationKind::Ready { revision: None },
        LaneNotificationKind::Failed {
            episode: "failure".into(),
            revision: None,
        },
        LaneNotificationKind::Closed {
            close_id: "close".into(),
        },
        LaneNotificationKind::Intervention {
            input_id: "input".into(),
        },
    ];
    let mut ids = vec![];
    for kind in kinds {
        ids.push(f.notice(kind, "lead"));
    }
    f.notice(LaneNotificationKind::Milestone, "lead");
    let answer = f.notice(
        LaneNotificationKind::Answer {
            question_id: "q".into(),
            routing_revision: 2,
        },
        "thread",
    );
    let nudge = f.notice(
        LaneNotificationKind::Nudge {
            episode_id: "ep".into(),
        },
        "thread",
    );
    f.apply(
        "summary",
        1,
        LaneManagementAction::SetPolicy {
            policy: ManagedLanePolicy::Summary,
        },
    );
    let lead = crate::repo::lane_delivery::inbox(f.c(), "lead", "").unwrap();
    assert_eq!(lead.len(), 4);
    assert!(
        lead.iter()
            .all(|n| n.management_revision == 2 && ids.contains(&n.notification_id))
    );
    let lane = crate::repo::lane_delivery::inbox(f.c(), "thread", "").unwrap();
    assert_eq!(lane.len(), 2);
    assert!(lane.iter().all(|n| n.management_revision == 2));
    f.apply("detach", 2, LaneManagementAction::Detach);
    assert!(
        crate::repo::lane_delivery::inbox(f.c(), "lead", "")
            .unwrap()
            .is_empty()
    );
    let lane = crate::repo::lane_delivery::inbox(f.c(), "thread", "").unwrap();
    assert_eq!(lane.len(), 1);
    assert_eq!(lane[0].notification_id, answer);
    assert_ne!(answer, nudge);
    assert_eq!(lane[0].management_revision, 3);
    f.apply(
        "attach",
        3,
        LaneManagementAction::Reattach {
            lead_thread_id: "target".into(),
            expected_lead_root_run_id: "target-root".into(),
        },
    );
    assert!(
        crate::repo::lane_delivery::inbox(f.c(), "target", "")
            .unwrap()
            .is_empty()
    );
}
#[test]
fn report_cas_rejects_both_old_and_current_detached_revision() {
    let f = Fixture::new(LanePolicy::Attached);
    let report = LaneReportInput {
        status: LaneStatus::Working,
        summary: "working".into(),
        artifact_refs: vec![],
        needs_human: false,
        question_refs: vec![],
        reason_code: None,
        producer: LaneReportProducer::LaneAgent,
        artifact_revision: None,
        failure_episode: None,
    };
    crate::repo::lane_delivery::append_report(f.c(), "lane", 1, report.clone(), 2).unwrap();
    f.apply("detach", 1, LaneManagementAction::Detach);
    for revision in [1, 2] {
        assert!(
            crate::repo::lane_delivery::append_report(f.c(), "lane", revision, report.clone(), 3)
                .is_err()
        );
    }
    assert_eq!(lane(f.c(), "lane").unwrap().latest_report_seq, 1);
    assert_eq!(
        crate::repo::lane_control::current_close(f.c(), "lane").unwrap(),
        None
    );
}
