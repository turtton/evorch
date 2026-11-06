use super::*;
use agents::Role;
use event_bus::{EventBus, EventKind, LifecycleEvent};
use providers::{ContentBlock, FinishReason, Message, ToolSpec};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Default)]
enum Mode {
    #[default]
    Approve,
    Reject,
    Silent,
    Error,
    Hold,
}
type RecordedRequest = (String, Vec<Message>, Vec<ToolSpec>);
#[derive(Default)]
struct Model {
    mode: Mutex<Mode>,
    override_id: Mutex<Option<String>>,
    requests: Mutex<Vec<RecordedRequest>>,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl crate::AgentModel for Model {
    async fn complete(
        &self,
        ctx: &crate::AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<providers::ChatResponse, crate::RuntimeError> {
        self.requests
            .lock()
            .unwrap()
            .push((ctx.run_id.clone(), messages.to_vec(), tools.to_vec()));
        if messages.last().is_some_and(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
        }) {
            return Ok(text("tool result observed"));
        }
        if let Some(id) = messages
            .last()
            .filter(|m| m.role == providers::Role::User)
            .and_then(|m| {
                m.content.iter().find_map(|b| match b {
                    ContentBlock::Text { text } => text.strip_prefix("FORGED_SUBMIT "),
                    _ => None,
                })
            })
        {
            return Ok(tool(
                "submit_lane_verification",
                json!({"verification_ref":id,"verdict":"approved","checks":[{"criterion_index":0,"passed":true,"evidence_refs":["artifact.txt"]}],"evidence_refs":["artifact.txt"]}),
            ));
        }
        if role == Role::Reviewer {
            let mode = loop {
                let mode = *self.mode.lock().unwrap();
                if matches!(mode, Mode::Hold) {
                    self.entered.notify_one();
                    self.release.notified().await;
                } else {
                    break mode;
                }
            };
            match mode {
                Mode::Hold => {
                    self.entered.notify_one();
                    std::future::pending::<()>().await;
                }
                Mode::Error => {
                    return Err(crate::RuntimeError::Model {
                        reason: "injected reviewer failure".into(),
                    });
                }
                Mode::Silent => return Ok(text("no structured submission")),
                Mode::Approve | Mode::Reject => {}
            }
            let prompt = messages
                .iter()
                .flat_map(|m| m.content.iter())
                .find_map(|b| match b {
                    ContentBlock::Text { text } if text.contains("verification_ref ") => Some(text),
                    _ => None,
                })
                .unwrap();
            let id = prompt
                .split("verification_ref ")
                .nth(1)
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap()
                .trim_end_matches(',');
            let override_id = self.override_id.lock().unwrap().clone();
            let id = override_id.as_deref().unwrap_or(id);
            return Ok(tool(
                "submit_lane_verification",
                json!({"verification_ref":id,"verdict":if matches!(mode,Mode::Approve){"approved"}else{"rejected"},"checks":[{"criterion_index":0,"passed":matches!(mode,Mode::Approve),"evidence_refs":["artifact.txt"]}],"evidence_refs":["artifact.txt"],"findings":if matches!(mode,Mode::Approve){Vec::<String>::new()}else{vec!["missing required output".into()]}}),
            ));
        }
        Ok(text("idle"))
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test".into()
    }
}
fn text(value: &str) -> providers::ChatResponse {
    providers::ChatResponse {
        usage_reported: true,
        message: Message {
            role: providers::Role::Assistant,
            content: vec![ContentBlock::Text { text: value.into() }],
        },
        finish_reason: FinishReason::Stop,
        usage: Default::default(),
    }
}
fn tool(name: &str, input: Value) -> providers::ChatResponse {
    let mut r = text("");
    r.finish_reason = FinishReason::ToolUse;
    r.message.content = vec![ContentBlock::ToolUse {
        id: "submission".into(),
        name: name.into(),
        input,
    }];
    r
}
struct Fixture {
    runtime: AgentRuntime,
    model: Arc<Model>,
    storage: storage::Storage,
    events: event_bus::EventReceiver,
    dir: tempfile::TempDir,
    lead: RunId,
    lane: crate::lanes::LaneOpened,
}
impl Fixture {
    async fn new(mode: Mode) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("artifact.txt"), b"verified contents").unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("lanes.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let bus = Arc::new(EventBus::new(4096));
        let events = bus.subscribe();
        let model = Arc::new(Model::default());
        *model.mode.lock().unwrap() = mode;
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus)),
            model.clone(),
        )
        .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
        runtime.set_project_root(dir.path().to_owned()).unwrap();
        let lead = runtime
            .delegate_chat(
                "lead",
                Role::Orchestrator,
                "coordinate".into(),
                RunConfig {
                    interactive: true,
                    keep_alive: true,
                    conversation: true,
                    ..Default::default()
                },
            )
            .unwrap();
        let mut f = Self {
            runtime,
            model,
            storage,
            events,
            dir,
            lead,
            lane: crate::lanes::LaneOpened {
                lane_id: String::new(),
                thread_id: String::new(),
                root_run_id: RunId::new(0),
                policy: LanePolicy::Attached,
                created: false,
                started: false,
            },
        };
        f.phase(lead, AgentRunPhase::Waiting).await;
        f.storage
            .handle()
            .link_pull_request(storage::pull_requests::LinkPullRequest {
                thread_id: "lead".into(),
                key: storage::pull_requests::PullRequestKey::github("owner/repo", 1).unwrap(),
                expected_revision: 0,
                at_ns: 1,
            })
            .unwrap();
        f.lane = f
            .runtime
            .open_lane(
                lead,
                "open",
                event_bus::LaneBrief {
                    verification_requirements: Default::default(),
                    objective: "independent deliverable".into(),
                    inputs: vec![],
                    deliverables: vec!["artifact.txt".into()],
                    acceptance_criteria: vec!["artifact is correct".into()],
                    constraints: vec![],
                    workspace: Default::default(),
                    report_policy: LanePolicy::Attached,
                },
            )
            .unwrap();
        f.phase(f.lane.root_run_id, AgentRunPhase::Waiting).await;
        f.report();
        f
    }
    async fn phase(&mut self, run: RunId, phase: AgentRunPhase) {
        loop {
            if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id,
                to,
                reason,
                ..
            }) = self.events.recv().await.unwrap().kind
                && run_id == run.to_string()
            {
                assert_ne!(to, AgentRunPhase::Error, "{reason:?}");
                if to == phase {
                    return;
                }
            }
        }
    }
    fn report(&self) {
        self.storage
            .handle()
            .append_lane_report_with_delivery(
                self.lane.lane_id.clone(),
                event_bus::LaneReportInput {
                    artifact_revision: Some(ArtifactRevision::Sha256(format!(
                        "{:x}",
                        Sha256::digest(b"verified contents")
                    ))),
                    failure_episode: None,
                    status: event_bus::LaneStatus::Ready,
                    summary: "ready".into(),
                    artifact_refs: vec!["artifact.txt".into()],
                    needs_human: false,
                    question_refs: vec![],
                    reason_code: None,
                    producer: event_bus::lane::LaneReportProducer::LaneAgent,
                },
                crate::run_store::review_time_ns(),
            )
            .unwrap();
    }
    async fn verify(&self) -> LaneVerification {
        let seq = self
            .storage
            .handle()
            .lane(&self.lane.lane_id)
            .unwrap()
            .unwrap()
            .latest_report_seq;
        self.runtime
            .verify_lane(self.lead, &self.lane.lane_id, seq)
            .await
            .unwrap()
    }
    async fn close(&self, v: &LaneVerification) -> Result<LaneCloseResult, LaneError> {
        self.runtime
            .close_verified_lane(
                self.lead,
                &self.lane.lane_id,
                v.target.ready_seq,
                v.verification_id,
            )
            .await
    }
    async fn cleanup(&self) {
        for run in [self.lead, self.lane.root_run_id] {
            self.runtime.cancel(run).unwrap();
            let _ = self.runtime.wait(run).await;
        }
    }
}
#[tokio::test]
async fn approved_reviewer_completes_and_close_is_idempotent_without_pr() {
    let f = Fixture::new(Mode::Approve).await;
    let v = f.verify().await;
    let reviewer = v.reviewer_run_id.parse().unwrap();
    f.runtime.wait(reviewer).await.unwrap();
    let saved = f
        .storage
        .handle()
        .lane_verification(v.verification_id)
        .unwrap()
        .unwrap();
    assert_eq!(saved.state, LaneVerificationState::Completed);
    let closed = f.close(&v).await.unwrap();
    assert_eq!(closed.outcome, LaneCloseOutcome::Closed);
    let retry = f.close(&v).await.unwrap();
    let alternate = f
        .runtime
        .close_verified_lane(
            f.lead,
            &f.lane.lane_id,
            v.target.ready_seq,
            VerificationId([0; 16]),
        )
        .await
        .unwrap();
    assert_eq!(alternate.outcome, LaneCloseOutcome::AlreadyClosed);
    assert_eq!(alternate.verification_ref, closed.verification_ref);
    assert_eq!(alternate.close_id, closed.close_id);
    assert_eq!(retry.outcome, LaneCloseOutcome::AlreadyClosed);
    assert_eq!(retry.close_id, closed.close_id);
    assert_eq!(retry.verification_ref, closed.verification_ref);
    {
        let requests = f.model.requests.lock().unwrap();
        let (_, _, tools) = requests
            .iter()
            .find(|(run, _, _)| run == &v.reviewer_run_id)
            .unwrap();
        assert!(tools.iter().any(|t| t.name == "submit_lane_verification"));
        assert!(!tools.iter().any(|t| matches!(
            t.name.as_str(),
            "delegate" | "shell" | "write" | "submit_goal_review" | "lane_close"
        )));
    }
    let submission = saved.submission.unwrap();
    assert_eq!(
        f.storage
            .handle()
            .submit_lane_verification(
                v.verification_id,
                v.reviewer_run_id.clone(),
                submission,
                crate::run_store::review_time_ns()
            )
            .unwrap_err()
            .to_string(),
        "already_submitted"
    );
    f.cleanup().await;
}
#[tokio::test]
async fn silent_done_error_cancel_and_rejection_cannot_close() {
    for mode in [Mode::Silent, Mode::Error, Mode::Hold, Mode::Reject] {
        let f = Fixture::new(mode).await;
        let v = f.verify().await;
        let reviewer = v.reviewer_run_id.parse().unwrap();
        if matches!(mode, Mode::Hold) {
            f.model.entered.notified().await;
            assert_eq!(f.close(&v).await.unwrap_err().code, "reviewer_incomplete");
            f.runtime.cancel(reviewer).unwrap();
        }
        let _ = f.runtime.wait(reviewer).await;
        assert_eq!(
            f.close(&v).await.unwrap_err().code,
            if matches!(mode, Mode::Reject) {
                "verification_rejected"
            } else {
                "reviewer_incomplete"
            }
        );
        f.cleanup().await;
    }
}
#[tokio::test]
async fn external_digest_and_new_report_invalidate_without_rewriting_evidence() {
    let f = Fixture::new(Mode::Approve).await;
    let v = f.verify().await;
    f.runtime
        .wait(v.reviewer_run_id.parse().unwrap())
        .await
        .unwrap();
    let old = f
        .storage
        .handle()
        .lane_verification(v.verification_id)
        .unwrap();
    std::fs::write(f.dir.path().join("artifact.txt"), b"unreviewed edit").unwrap();
    assert_eq!(f.close(&v).await.unwrap_err().code, "artifact_changed");
    std::fs::write(f.dir.path().join("artifact.txt"), b"verified contents").unwrap();
    f.close(&v).await.unwrap();
    f.report();
    assert!(
        f.storage
            .handle()
            .current_lane_close(f.lane.lane_id.clone())
            .unwrap()
            .is_none()
    );
    assert_eq!(f.close(&v).await.unwrap_err().code, "stale_verification");
    assert_eq!(
        f.storage
            .handle()
            .lane_verification(v.verification_id)
            .unwrap(),
        old
    );
    f.cleanup().await;
}
#[tokio::test]
async fn pr_requirement_is_explicit_and_lane_never_inherits_parent_pr() {
    let f = Fixture::new(Mode::Approve).await;
    assert!(
        f.storage
            .handle()
            .pull_request_thread_state("lead")
            .unwrap()
            .active_link
            .is_some()
    );
    assert!(
        f.storage
            .handle()
            .pull_request_thread_state(&f.lane.thread_id)
            .unwrap()
            .active_link
            .is_none()
    );
    let mut lane = f.storage.handle().lane(&f.lane.lane_id).unwrap().unwrap();
    lane.brief.verification_requirements.pull_request = true;
    // Test the enforcement entry directly; the serialized brief is also checked by
    // storage's revision CAS when a real brief update races verify/submit/close.
    assert_eq!(
        f.runtime
            .verification_pr(&lane, true)
            .await
            .unwrap_err()
            .code,
        "evidence_unavailable"
    );
    f.cleanup().await;
}
#[test]
fn artifact_paths_are_single_regular_files_inside_allowed_roots() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let roots = vec![dir.path().to_owned()];
    let file = dir.path().join("one");
    std::fs::write(&file, b"data").unwrap();
    assert_eq!(
        file_digest(&file, &roots).unwrap(),
        format!("{:x}", Sha256::digest(b"data"))
    );
    assert_eq!(
        file_digest(dir.path(), &roots).unwrap_err().code,
        "evidence_unavailable"
    );
    let escape = outside.path().join("escape");
    std::fs::write(&escape, b"data").unwrap();
    assert_eq!(
        file_digest(&escape, &roots).unwrap_err().code,
        "evidence_unavailable"
    );
    #[cfg(unix)]
    {
        let link = dir.path().join("symlink");
        std::os::unix::fs::symlink(escape, &link).unwrap();
        assert_eq!(
            file_digest(&link, &roots).unwrap_err().code,
            "evidence_unavailable"
        );
    }
}
#[test]
fn reviewer_purpose_is_required_and_wrong_roles_get_no_submit_capability() {
    for role in [
        Role::Orchestrator,
        Role::Worker,
        Role::Advisor,
        Role::Reviewer,
    ] {
        let general = crate::ExecutionPolicy::for_role(role)
            .for_run_config(&RunConfig::default(), role == Role::Orchestrator);
        assert!(general.authorize("submit_lane_verification").is_err());
        let config = RunConfig {
            purpose: RunPurpose::LaneVerification {
                verification_id: VerificationId([1; 16]),
                lead_root: RunId::new(1),
            },
            ..Default::default()
        };
        let policy = crate::ExecutionPolicy::for_role(role).for_run_config(&config, false);
        assert_eq!(
            policy.authorize("submit_lane_verification").is_ok(),
            role == Role::Reviewer
        );
        assert!(policy.authorize("shell").is_err());
        assert!(policy.authorize("lane_close").is_err());
    }
    for invalid in ["", "éééééééééééééééé", "0000000000000000000000000000000g"] {
        assert!(parse_id(invalid).is_err());
    }
    assert_eq!(
        parse_id(&VerificationId([1; 16]).to_string()).unwrap(),
        VerificationId([1; 16])
    );
}

struct NoOwner;
impl crate::pull_requests::OwnershipView for NoOwner {
    fn owned_permit(&self, _: &str) -> Option<crate::ownership::OwnerPermit> {
        None
    }
}
struct Clock(std::sync::atomic::AtomicI64);
impl crate::pull_requests::WallClock for Clock {
    fn now_ns(&self) -> i64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}
struct Environment {
    run: String,
    thread: String,
}
impl crate::pull_requests::ThreadEnvironment for Environment {
    fn thread_of_run(&self, run: &str) -> Option<String> {
        (run == self.run).then(|| self.thread.clone())
    }
    fn checkout_directories(&self, _: &str) -> Vec<PathBuf> {
        vec![]
    }
}
fn facts() -> storage::pull_requests::PullRequestFacts {
    use storage::pull_requests::*;
    PullRequestFacts {
        state: PullRequestState::Open,
        base_branch: "main".into(),
        head_repository: Some("owner/repo".into()),
        head_branch: "feature".into(),
        head_sha: "a".repeat(40),
        checks: vec![],
        checks_complete: true,
        review_decision: ReviewDecision::Approved,
    }
}
fn require_pr(f: &Fixture) -> LaneRecord {
    let lane = f.storage.handle().lane(&f.lane.lane_id).unwrap().unwrap();
    let mut brief = lane.brief;
    brief.verification_requirements.pull_request = true;
    f.storage
        .handle()
        .revise_lane_brief(&lane.lane_id, lane.brief_revision, brief, 2)
        .unwrap();
    f.report();
    f.storage.handle().lane(&lane.lane_id).unwrap().unwrap()
}
fn install_pr(
    f: &Fixture,
) -> (
    crate::pull_requests::PullRequestService,
    Arc<Clock>,
    crate::pull_requests::FakePullRequestSource,
    storage::pull_requests::PullRequestKey,
) {
    install_pr_with(f, Arc::new(NoOwner))
}
fn install_pr_with(
    f: &Fixture,
    ownership: Arc<dyn crate::pull_requests::OwnershipView>,
) -> (
    crate::pull_requests::PullRequestService,
    Arc<Clock>,
    crate::pull_requests::FakePullRequestSource,
    storage::pull_requests::PullRequestKey,
) {
    let key = storage::pull_requests::PullRequestKey::github("owner/repo", 2).unwrap();
    f.storage
        .handle()
        .link_pull_request(storage::pull_requests::LinkPullRequest {
            thread_id: f.lane.thread_id.clone(),
            key: key.clone(),
            expected_revision: 0,
            at_ns: 2,
        })
        .unwrap();
    let clock = Arc::new(Clock(std::sync::atomic::AtomicI64::new(1_000_000_000_000)));
    let source = crate::pull_requests::FakePullRequestSource::default();
    let service =
        crate::pull_requests::PullRequestService::start(crate::pull_requests::ServiceParts {
            storage: f.storage.handle(),
            source: Arc::new(source.clone()),
            ownership,
            environment: Arc::new(Environment {
                run: f.lane.root_run_id.to_string(),
                thread: f.lane.thread_id.clone(),
            }),
            bus: f.runtime.shared.bus.clone(),
            settings: config::PullRequestsConfig {
                enabled: true,
                ..Default::default()
            },
            clock: clock.clone(),
        })
        .unwrap();
    f.runtime
        .shared
        .pull_requests
        .set(service.clone())
        .unwrap_or_else(|_| panic!("already installed"));
    (service, clock, source, key)
}
#[tokio::test]
async fn fresh_pr_versions_are_bound_and_close_never_fetches_even_for_old_missing_or_failed_facts()
{
    use crate::pull_requests::WallClock;
    use storage::pull_requests::*;
    let f = Fixture::new(Mode::Approve).await;
    let lane = require_pr(&f);
    let (service, clock, source, key) = install_pr(&f);
    assert_eq!(
        f.runtime
            .verification_pr(&lane, false)
            .await
            .unwrap_err()
            .code,
        "evidence_unavailable"
    );
    assert_eq!(
        f.runtime
            .verify_lane(f.lead, &lane.lane_id, lane.latest_report_seq)
            .await
            .unwrap_err()
            .code,
        "evidence_unavailable"
    ); // NoFacts + NotOwner
    let seq = f.storage.handle().begin_pull_request_fetch(&key).unwrap();
    f.storage
        .handle()
        .commit_pull_request_fetch(&key, seq, facts(), clock.now_ns())
        .unwrap();
    let v = f.verify().await;
    assert!(v.pr_evidence.is_some());
    f.runtime
        .wait(v.reviewer_run_id.parse().unwrap())
        .await
        .unwrap();
    let original = f
        .storage
        .handle()
        .lane_verification(v.verification_id)
        .unwrap();
    // A newer same-content fresh fetch can be used by close, not by rewriting v.
    let seq = f.storage.handle().begin_pull_request_fetch(&key).unwrap();
    f.storage
        .handle()
        .commit_pull_request_fetch(&key, seq, facts(), clock.now_ns() + 1)
        .unwrap();
    f.close(&v).await.unwrap();
    clock.0.store(
        clock.now_ns() + 61_000_000_000,
        std::sync::atomic::Ordering::SeqCst,
    );
    assert_eq!(f.close(&v).await.unwrap_err().code, "evidence_unavailable"); // TooOld, no refresh
    let seq = f.storage.handle().begin_pull_request_fetch(&key).unwrap();
    f.storage
        .handle()
        .fail_pull_request_fetch(
            &key,
            seq,
            FetchFailure {
                kind: FetchErrorKind::Network,
                message: "injected".into(),
            },
            clock.now_ns(),
        )
        .unwrap();
    assert_eq!(f.close(&v).await.unwrap_err().code, "evidence_unavailable"); // LastFetchFailed
    assert!(
        source.calls().is_empty(),
        "neither close nor a NotOwner refresh fetched"
    );
    assert_eq!(
        f.storage
            .handle()
            .lane_verification(v.verification_id)
            .unwrap(),
        original
    );
    service.shutdown();
    f.cleanup().await;
}
#[tokio::test]
async fn relink_head_and_required_facts_changes_reject_old_evidence() {
    use crate::pull_requests::WallClock;
    for change in ["head", "checks", "link"] {
        let f = Fixture::new(Mode::Approve).await;
        require_pr(&f);
        let (service, clock, source, key) = install_pr(&f);
        let seq = f.storage.handle().begin_pull_request_fetch(&key).unwrap();
        f.storage
            .handle()
            .commit_pull_request_fetch(&key, seq, facts(), clock.now_ns())
            .unwrap();
        let v = f.verify().await;
        f.runtime
            .wait(v.reviewer_run_id.parse().unwrap())
            .await
            .unwrap();
        if change == "link" {
            f.storage
                .handle()
                .link_pull_request(storage::pull_requests::LinkPullRequest {
                    thread_id: f.lane.thread_id.clone(),
                    key: storage::pull_requests::PullRequestKey::github("owner/repo", 3).unwrap(),
                    expected_revision: 1,
                    at_ns: 3,
                })
                .unwrap();
        } else {
            let mut changed = facts();
            if change == "head" {
                changed.head_sha = "b".repeat(40);
            } else {
                changed.checks_complete = false;
            }
            let seq = f.storage.handle().begin_pull_request_fetch(&key).unwrap();
            f.storage
                .handle()
                .commit_pull_request_fetch(&key, seq, changed, clock.now_ns())
                .unwrap();
        }
        assert_eq!(f.close(&v).await.unwrap_err().code, "evidence_unavailable");
        assert!(source.calls().is_empty());
        service.shutdown();
        f.cleanup().await;
    }
}
#[tokio::test]
async fn only_current_lead_can_verify_and_detached_lanes_are_not_closable() {
    let f = Fixture::new(Mode::Hold).await;
    let v = f.verify().await;
    f.model.entered.notified().await;
    assert_eq!(
        f.runtime
            .verify_lane(f.lane.root_run_id, &f.lane.lane_id, 1)
            .await
            .unwrap_err()
            .code,
        "not_current_lead"
    );
    assert_eq!(
        f.runtime
            .verify_lane(f.lead, &f.lane.lane_id, 0)
            .await
            .unwrap_err()
            .code,
        "stale_ready"
    );
    let lane = f.storage.handle().lane(&f.lane.lane_id).unwrap().unwrap();
    f.storage
        .handle()
        .change_lane_management(
            &lane.lane_id,
            lane.management_revision,
            storage::lanes::LaneManagementChange {
                lead_thread_id: None,
                policy: LanePolicy::Detached,
            },
            3,
        )
        .unwrap();
    assert_eq!(f.close(&v).await.unwrap_err().code, "not_attached");
    f.runtime
        .cancel(v.reviewer_run_id.parse().unwrap())
        .unwrap();
    let _ = f.runtime.wait(v.reviewer_run_id.parse().unwrap()).await;
    f.cleanup().await;
}

#[tokio::test]
async fn forged_lead_worker_advisor_and_different_verification_reviewer_cannot_submit() {
    let mut f = Fixture::new(Mode::Hold).await;
    let v = f.verify().await;
    f.model.entered.notified().await;
    let prompt = format!("FORGED_SUBMIT {}", v.verification_id);
    f.runtime.send_message(f.lead, prompt.clone()).unwrap();
    f.phase(f.lead, AgentRunPhase::Waiting).await;
    for role in [Role::Worker, Role::Advisor, Role::Reviewer] {
        let run = f
            .runtime
            .delegate_background_as_child(f.lead, role, prompt.clone(), RunConfig::default())
            .unwrap();
        f.runtime.wait(run).await.unwrap();
    }
    assert_eq!(
        f.storage
            .handle()
            .lane_verification(v.verification_id)
            .unwrap()
            .unwrap()
            .state,
        LaneVerificationState::Pending
    );
    *f.model.mode.lock().unwrap() = Mode::Approve;
    *f.model.override_id.lock().unwrap() = Some(v.verification_id.to_string());
    let other = f.verify().await;
    f.runtime
        .wait(other.reviewer_run_id.parse().unwrap())
        .await
        .unwrap();
    assert_ne!(other.verification_id, v.verification_id);
    for id in [v.verification_id, other.verification_id] {
        let saved = f.storage.handle().lane_verification(id).unwrap().unwrap();
        assert_eq!(saved.state, LaneVerificationState::Pending);
        assert!(saved.submission.is_none());
    }
    {
        let requests = f.model.requests.lock().unwrap();
        let wrong = requests
            .iter()
            .rfind(|(run, _, _)| run == &other.reviewer_run_id)
            .unwrap();
        assert!(wrong.1.iter().any(|m|m.content.iter().any(|b|matches!(b,ContentBlock::ToolResult{content,is_error:true,..} if serde_json::to_string(content).unwrap().contains("not_responder")))));
    }
    f.runtime
        .cancel(v.reviewer_run_id.parse().unwrap())
        .unwrap();
    let _ = f.runtime.wait(v.reviewer_run_id.parse().unwrap()).await;
    f.cleanup().await;
}
#[tokio::test]
async fn a_submission_without_normal_reviewer_termination_is_not_complete() {
    let f = Fixture::new(Mode::Hold).await;
    let v = f.verify().await;
    f.model.entered.notified().await;
    let result = LaneVerificationSubmission {
        verdict: LaneVerificationVerdict::Approved,
        checks: vec![LaneCriterionCheck {
            criterion_index: 0,
            passed: true,
            evidence_refs: vec!["artifact.txt".into()],
        }],
        result_ref: "runtime-test-result".into(),
        result_digest: "a".repeat(64),
    };
    f.storage
        .handle()
        .submit_lane_verification(
            v.verification_id,
            v.reviewer_run_id.clone(),
            result,
            crate::run_store::review_time_ns(),
        )
        .unwrap();
    assert_eq!(f.close(&v).await.unwrap_err().code, "reviewer_incomplete");
    f.runtime
        .cancel(v.reviewer_run_id.parse().unwrap())
        .unwrap();
    let _ = f.runtime.wait(v.reviewer_run_id.parse().unwrap()).await;
    assert_eq!(f.close(&v).await.unwrap_err().code, "reviewer_incomplete");
    f.cleanup().await;
}

#[tokio::test]
async fn lane_input_report_and_brief_changes_win_the_submission_cas() {
    for change in ["input", "report", "brief"] {
        let mut f = Fixture::new(Mode::Hold).await;
        let v = f.verify().await;
        f.model.entered.notified().await;
        match change {
            "input" => {
                f.runtime
                    .send_message(f.lane.root_run_id, "new lane-specific request".into())
                    .unwrap();
                f.phase(f.lane.root_run_id, AgentRunPhase::Waiting).await;
            }
            "report" => f.report(),
            "brief" => {
                let lane = f.storage.handle().lane(&f.lane.lane_id).unwrap().unwrap();
                let mut brief = lane.brief;
                brief.constraints.push("new constraint".into());
                f.storage
                    .handle()
                    .revise_lane_brief(&lane.lane_id, lane.brief_revision, brief, 3)
                    .unwrap();
            }
            _ => unreachable!(),
        }
        *f.model.mode.lock().unwrap() = Mode::Approve;
        f.model.release.notify_one();
        f.runtime
            .wait(v.reviewer_run_id.parse().unwrap())
            .await
            .unwrap();
        let saved = f
            .storage
            .handle()
            .lane_verification(v.verification_id)
            .unwrap()
            .unwrap();
        assert!(saved.submission.is_none());
        assert_eq!(saved.state, LaneVerificationState::Pending);
        assert!(f.close(&v).await.is_err());
        f.cleanup().await;
    }
}
#[tokio::test]
async fn untargeted_lead_input_preserves_evidence_but_lane_input_reopens_current_close() {
    let mut f = Fixture::new(Mode::Approve).await;
    let v = f.verify().await;
    f.runtime
        .wait(v.reviewer_run_id.parse().unwrap())
        .await
        .unwrap();
    let before = f
        .storage
        .handle()
        .lane_verification_target(f.lane.lane_id.clone())
        .unwrap();
    f.runtime
        .send_message(f.lead, "unassigned new request for lead".into())
        .unwrap();
    f.phase(f.lead, AgentRunPhase::Waiting).await;
    assert_eq!(
        f.storage
            .handle()
            .lane_verification_target(f.lane.lane_id.clone())
            .unwrap(),
        before
    );
    let closed = f.close(&v).await.unwrap();
    let saved = f
        .storage
        .handle()
        .lane_verification(v.verification_id)
        .unwrap();
    f.runtime
        .send_message(f.lane.root_run_id, "new lane requirement".into())
        .unwrap();
    f.phase(f.lane.root_run_id, AgentRunPhase::Waiting).await;
    assert!(
        f.storage
            .handle()
            .current_lane_close(f.lane.lane_id.clone())
            .unwrap()
            .is_none()
    );
    assert!(f.close(&v).await.is_err());
    assert_eq!(
        f.storage
            .handle()
            .lane_verification(v.verification_id)
            .unwrap(),
        saved
    );
    assert_ne!(closed.close_id, "");
    f.cleanup().await;
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}
/// Commits `artifact.txt` on `feature` in the fixture directory and reports that commit as
/// the lane's Ready artifact. Returns `(head, tree)`.
fn commit_fixture(f: &Fixture) -> (String, String) {
    let dir = f.dir.path();
    git(dir, &["init", "-q"]);
    git(dir, &["symbolic-ref", "HEAD", "refs/heads/feature"]);
    git(
        dir,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/owner/repo.git",
        ],
    );
    std::fs::write(dir.join(".gitignore"), "lanes.db*\n").unwrap();
    git(dir, &["add", "--", ".gitignore", "artifact.txt"]);
    let tree = git(dir, &["write-tree"]);
    let head = git(dir, &["commit-tree", &tree, "-m", "fixture"]);
    git(dir, &["update-ref", "refs/heads/feature", &head]);
    let lane = f.storage.handle().lane(&f.lane.lane_id).unwrap().unwrap();
    let mut brief = lane.brief;
    brief.workspace.branch = Some("feature".into());
    brief.workspace.worktree = Some(dir.to_string_lossy().into());
    f.storage
        .handle()
        .revise_lane_brief(&lane.lane_id, lane.brief_revision, brief, 3)
        .unwrap();
    f.storage
        .handle()
        .append_lane_report_with_delivery(
            lane.lane_id,
            event_bus::LaneReportInput {
                artifact_revision: Some(ArtifactRevision::CommitSha(head.clone())),
                failure_episode: None,
                status: event_bus::LaneStatus::Ready,
                summary: "committed".into(),
                artifact_refs: vec!["artifact.txt".into()],
                needs_human: false,
                question_refs: vec![],
                reason_code: None,
                producer: event_bus::lane::LaneReportProducer::LaneAgent,
            },
            crate::run_store::review_time_ns(),
        )
        .unwrap();
    (head, tree)
}
#[tokio::test]
async fn commit_evidence_uses_resolved_clean_checkout_and_rejects_dirty_or_moved_head() {
    let f = Fixture::new(Mode::Approve).await;
    let (head, tree) = commit_fixture(&f);
    let dir = f.dir.path();
    let v = f.verify().await;
    f.runtime
        .wait(v.reviewer_run_id.parse().unwrap())
        .await
        .unwrap();
    std::fs::write(dir.join("artifact.txt"), b"dirty").unwrap();
    assert_eq!(f.close(&v).await.unwrap_err().code, "artifact_changed");
    std::fs::write(dir.join("artifact.txt"), b"verified contents").unwrap();
    let new_head = git(
        dir,
        &["commit-tree", &tree, "-p", &head, "-m", "next fixture"],
    );
    git(dir, &["update-ref", "refs/heads/feature", &new_head]);
    assert_eq!(f.close(&v).await.unwrap_err().code, "artifact_changed");
    f.cleanup().await;
}
#[tokio::test]
async fn index_flags_that_hide_edits_from_status_reject_commit_evidence() {
    for flag in ["assume-unchanged", "skip-worktree"] {
        let f = Fixture::new(Mode::Approve).await;
        commit_fixture(&f);
        let dir = f.dir.path();
        let v = f.verify().await;
        f.runtime
            .wait(v.reviewer_run_id.parse().unwrap())
            .await
            .unwrap();
        git(dir, &["update-index", &format!("--{flag}"), "artifact.txt"]);
        std::fs::write(dir.join("artifact.txt"), b"hidden edit").unwrap();
        assert!(
            git(dir, &["status", "--porcelain=v1"]).is_empty(),
            "{flag} must hide the edit from status, which is the bypass under test"
        );
        assert_eq!(f.close(&v).await.unwrap_err().code, "artifact_changed");
        // A flagged file is refused even when its contents match the commit again.
        std::fs::write(dir.join("artifact.txt"), b"verified contents").unwrap();
        assert_eq!(f.close(&v).await.unwrap_err().code, "artifact_changed");
        git(
            dir,
            &["update-index", &format!("--no-{flag}"), "artifact.txt"],
        );
        f.close(&v).await.unwrap();
        f.cleanup().await;
    }
}
#[tokio::test]
async fn a_brief_and_ready_update_between_the_lane_and_target_reads_is_rejected() {
    let f = Fixture::new(Mode::Approve).await;
    let handle = f.storage.handle();
    let lane = handle.lane(&f.lane.lane_id).unwrap().unwrap();
    let guessed_next_seq = lane.latest_report_seq + 1;
    let (lane_id, revision) = (lane.lane_id.clone(), lane.brief_revision);
    let mut revised = lane.brief.clone();
    revised.objective = "a different deliverable".into();
    let updater = handle.clone();
    let digest = format!("{:x}", Sha256::digest(b"verified contents"));
    AFTER_LANE_READ.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            updater
                .revise_lane_brief(&lane_id, revision, revised, 5)
                .unwrap();
            updater
                .append_lane_report_with_delivery(
                    lane_id,
                    event_bus::LaneReportInput {
                        artifact_revision: Some(ArtifactRevision::Sha256(digest)),
                        failure_episode: None,
                        status: event_bus::LaneStatus::Ready,
                        summary: "ready for the revised brief".into(),
                        artifact_refs: vec!["artifact.txt".into()],
                        needs_human: false,
                        question_refs: vec![],
                        reason_code: None,
                        producer: event_bus::lane::LaneReportProducer::LaneAgent,
                    },
                    crate::run_store::review_time_ns(),
                )
                .unwrap();
        }));
    });
    let error = f
        .runtime
        .verify_lane(f.lead, &f.lane.lane_id, guessed_next_seq)
        .await
        .unwrap_err();
    assert_eq!(error.code, "stale_ready");
    // Without the revision check this call would have matched the new Ready and
    // started a Reviewer on the old brief.
    let target = handle
        .lane_verification_target(f.lane.lane_id.clone())
        .unwrap();
    assert_eq!(target.ready_seq, guessed_next_seq);
    assert!(target.brief_revision > revision);
    f.cleanup().await;
}
#[tokio::test]
async fn an_owned_lane_refreshes_pr_facts_on_verify_and_close_never_fetches() {
    let f = Fixture::new(Mode::Approve).await;
    require_pr(&f);
    let host = Arc::new(
        crate::ownership::OwnerHost::open(
            &f.dir.path().join("owners"),
            config::OwnershipConfig::default(),
            f.runtime.shared.bus.clone(),
        )
        .unwrap(),
    );
    host.start(&f.lane.thread_id).unwrap();
    let (service, clock, source, key) = install_pr_with(&f, host);
    source.push_ok(
        &key,
        crate::pull_requests::FetchedPullRequest {
            requested: key.clone(),
            canonical: key.clone(),
            url: "https://github.com/owner/repo/pull/2".into(),
            display: Default::default(),
            facts: facts(),
            fetched_at_ns: crate::pull_requests::WallClock::now_ns(&*clock),
        },
    );
    let v = f.verify().await;
    assert!(v.pr_evidence.is_some());
    let fetches_by_verify = source.calls().len();
    assert!(fetches_by_verify >= 1, "verify refreshed through the owner");
    f.runtime
        .wait(v.reviewer_run_id.parse().unwrap())
        .await
        .unwrap();
    f.close(&v).await.unwrap();
    assert_eq!(
        source.calls().len(),
        fetches_by_verify,
        "close reads the accessor's snapshot and never fetches"
    );
    service.shutdown();
    f.cleanup().await;
}

#[tokio::test(start_paused = true)]
async fn verify_returns_evidence_unavailable_when_owned_pr_refresh_times_out() {
    let f = Fixture::new(Mode::Approve).await;
    require_pr(&f);
    let owner = Arc::new(
        crate::ownership::OwnerHost::open(
            &f.dir.path().join("owners"),
            Default::default(),
            f.runtime.shared.bus.clone(),
        )
        .unwrap(),
    );
    owner.start(&f.lane.thread_id).unwrap();
    let (service, _, source, key) = install_pr_with(&f, owner);
    let gate = source.push_gate(&key);
    let ready_seq = f
        .storage
        .handle()
        .lane(&f.lane.lane_id)
        .unwrap()
        .unwrap()
        .latest_report_seq;
    let verify = f.runtime.verify_lane(f.lead, &f.lane.lane_id, ready_seq);
    let expire = async {
        gate.arrived().await;
        tokio::time::advance(std::time::Duration::from_secs(
            config::PullRequestsConfig::default().review_refresh_timeout_seconds,
        ))
        .await;
    };
    let (result, ()) = tokio::join!(verify, expire);
    assert_eq!(result.unwrap_err().code, "evidence_unavailable");
    assert_eq!(source.calls().len(), 1);
    service.shutdown();
    drop(gate);
    f.cleanup().await;
}
