//! The record of what each review was given: it names the inputs by ID and
//! hash, matches what the model received, and never changes the review.

use super::*;
use crate::rules::{ProjectTrust, RulesSettings, RulesSource};
use crate::skill::{SkillScope, append_skill_attachment};
use event_bus::event::diagnostic_codes;
use event_bus::{DiagnosticEvent, EventBus, EventKind, EventReceiver};
use providers::FinishReason;
use support::{ScriptedModel, text_response};
use tools::tools::shell_escalation::{FileAccessRequest, ShellAccess};

const USER_WORDS: &str = "USER-WORDS-needle: CI通ったらマージしていいよ";
const SKILL_BODY: &str = "SKILL-BODY-needle: remove the task worktree after merging.";
const RULES_TEXT: &str = "RULES-TEXT-needle: sign every commit.";
const APPROVE: &str = r#"{"approve":true,"reason":"fine","risk_level":"low","authorization_level":"high","procedure_step":"defined"}"#;
const EVIDENCE_ID: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

fn rules_source(project: &std::path::Path) -> RulesSource {
    RulesSource::new(
        ProjectTrust::Approved,
        RulesSettings {
            context_window_tokens: 0,
            response_headroom_tokens: 0,
            max_injection_bytes: 0,
        },
        None,
        Some(project.to_path_buf()),
        None,
    )
}

fn request(text: &str, id: &str) -> UserRequest {
    UserRequest {
        target_run_id: "run-1".into(),
        text: text.into(),
        in_reply_to: None,
        reference_context: None,
        evidence: EvidenceRef {
            id: Some(id.into()),
            kind: Some("host_chat".into()),
        },
    }
}

struct Fixture {
    bus: Arc<EventBus>,
    events: EventReceiver,
    _project: tempfile::TempDir,
}

impl Fixture {
    fn new(rules: &str) -> Self {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("AGENTS.md"), rules).unwrap();
        let bus = Arc::new(EventBus::new(64));
        Self {
            events: bus.subscribe(),
            bus,
            _project: project,
        }
    }

    fn context(&self, run: ReviewRunContext) -> ReviewContext {
        let rules = rules_source(self._project.path());
        let (context, _) = review_context(run, Some(&rules)).unwrap();
        context.recorded_to(self.sink())
    }

    fn sink(&self) -> ManifestSink {
        ManifestSink::new(
            self.bus.clone(),
            "run-1",
            Some("thread-1".into()),
            Some("call-1".into()),
        )
    }

    fn diagnostics(&mut self) -> Vec<DiagnosticEvent> {
        self.events
            .drain_pending_snapshot()
            .into_iter()
            .filter_map(|event| match event.kind {
                EventKind::Diagnostic(diagnostic)
                    if diagnostic.code == diagnostic_codes::ESCALATION_REVIEW_MANIFEST =>
                {
                    Some(diagnostic)
                }
                _ => None,
            })
            .collect()
    }

    fn manifest(&mut self) -> ReviewManifest {
        let mut records = self.diagnostics();
        assert_eq!(records.len(), 1, "one review leaves one record");
        let record = records.remove(0);
        assert_eq!(record.source, "sandbox");
        assert_eq!(record.severity, event_bus::DiagnosticSeverity::Info);
        assert_eq!(record.run_id.as_deref(), Some("run-1"));
        assert_eq!(record.call_id.as_deref(), Some("call-1"));
        serde_json::from_str(&record.detail).unwrap()
    }
}

fn run_with(skills: &[(&str, SkillScope, &str)]) -> ReviewRunContext {
    let mut run = ReviewRunContext::empty("run-1");
    run.add_request(request(USER_WORDS, EVIDENCE_ID));
    for (name, scope, text) in skills {
        run.record_loaded_skill(LoadedSkill {
            name: (*name).into(),
            scope: *scope,
            loaded_by: LoadedBy::SkillLoad,
            text: (*text).into(),
            truncated: false,
        });
    }
    run
}

fn scripted(answer: &str) -> Arc<ScriptedModel> {
    Arc::new(ScriptedModel::new([Ok(text_response(
        answer,
        FinishReason::Stop,
    ))]))
}

/// The system and user text the model received.
async fn received(model: &ScriptedModel) -> (String, String) {
    let observed = model.observed().await;
    assert_eq!(observed.len(), 1, "one review is one model request");
    let [system, user] = observed[0].as_slice() else {
        panic!("a review sends exactly a system and a user message");
    };
    let text = |message: &Message| match message.content.as_slice() {
        [ContentBlock::Text { text }] => text.clone(),
        other => panic!("expected one text block, got {other:?}"),
    };
    assert_eq!(system.role, MessageRole::System);
    assert_eq!(user.role, MessageRole::User);
    (text(system), text(user))
}

fn digest_of(text: &str) -> Digest {
    Digest {
        sha256: sha256_hex(text.as_bytes()),
        bytes: text.len() as u64,
    }
}

async fn review_each_kind(fixture: &Fixture, model: &Arc<ScriptedModel>, kind: &str) {
    let reviewer = QuickModelReviewer::new(model.clone());
    let context = Some(fixture.context(run_with(&[(
        "worktree-pr",
        SkillScope::Builtin,
        SKILL_BODY,
    )])));
    let verdict = match kind {
        "shell_host" => {
            reviewer
                .review_scoped_with_context(
                    "run-1",
                    "git status",
                    "check",
                    context,
                    ShellAccess::Host,
                )
                .await
        }
        "shell_network" => {
            reviewer
                .review_scoped_with_context(
                    "run-1",
                    "git fetch",
                    "sync",
                    context,
                    ShellAccess::Network,
                )
                .await
        }
        "file_read" | "file_write" => {
            let request = FileAccessRequest {
                tool: "read".into(),
                paths: vec!["/outside/notes.txt".into()],
                write: kind == "file_write",
                justification: "docs".into(),
                change: None,
            };
            reviewer
                .review_file_access("run-1", &request, context)
                .await
        }
        "web" => {
            reviewer
                .review_web_request(
                    "run-1",
                    "web_fetch",
                    &serde_json::json!({"url": "https://example.com/docs"}),
                    context,
                )
                .await
        }
        "upload" => {
            reviewer
                .review_upload(
                    "run-1",
                    serde_json::json!({"form_origin": "https://example.com", "files": []}),
                    context,
                )
                .await
        }
        other => panic!("unknown kind {other}"),
    };
    assert_eq!(verdict, Ok(ReviewVerdict::Approve), "{kind}");
}

#[tokio::test]
async fn every_review_kind_records_a_manifest_matching_what_the_model_received() {
    let cases = [
        (
            "shell_host",
            ManifestKind::Shell,
            "host_unsandboxed",
            "shell",
        ),
        (
            "shell_network",
            ManifestKind::Shell,
            "network_only",
            "shell",
        ),
        ("file_read", ManifestKind::FileAccess, "file_read", "read"),
        ("file_write", ManifestKind::FileAccess, "file_write", "read"),
        ("web", ManifestKind::Web, "web", "web_fetch"),
        (
            "upload",
            ManifestKind::Upload,
            "upload",
            crate::network::UPLOAD_TOOL,
        ),
    ];
    for (name, kind, scope, tool) in cases {
        let mut fixture = Fixture::new(RULES_TEXT);
        let model = scripted(APPROVE);
        review_each_kind(&fixture, &model, name).await;
        let manifest = fixture.manifest();
        let (system, user) = received(&model).await;

        assert_eq!(manifest.version, MANIFEST_VERSION, "{name}");
        assert_eq!(manifest.kind, kind, "{name}");
        assert_eq!(manifest.scope, scope, "{name}");
        assert_eq!(manifest.tool.as_deref(), Some(tool), "{name}");
        assert_eq!(manifest.instruction, Some(digest_of(&system)), "{name}");
        assert_eq!(manifest.payload, Some(digest_of(&user)), "{name}");
        assert!(manifest.schema.is_some(), "{name}");
        let request = manifest.model_request.as_ref().unwrap();
        assert!(request.finished_at_ns.unwrap() >= request.requested_at_ns);
        let verdict = manifest.outcome.verdict.as_ref().unwrap();
        assert_eq!(manifest.outcome.status, OutcomeStatus::Decided, "{name}");
        assert_eq!(verdict.risk_level, Some(RiskLevel::Low), "{name}");
        assert_eq!(
            verdict.authorization_level,
            Some(AuthorizationLevel::High),
            "{name}"
        );
        assert_eq!(verdict.procedure_step, ProcedureStep::Defined, "{name}");
        assert_eq!(verdict.decision, Decision::Approve, "{name}");
        let context = manifest.context.as_ref().unwrap();
        assert_eq!(context.root_run_id, "run-1");
        assert_eq!(context.lineage_run_ids, ["run-1"]);
    }
}

#[tokio::test]
async fn the_instruction_digest_names_which_instruction_was_used() {
    let digests = |instruction: &str| digest_of(instruction);
    let mut seen = Vec::new();
    for kind in ["shell_host", "file_read", "web", "upload"] {
        let mut fixture = Fixture::new(RULES_TEXT);
        let model = scripted(APPROVE);
        review_each_kind(&fixture, &model, kind).await;
        let manifest = fixture.manifest();
        let expected = match kind {
            "shell_host" => digests(REVIEW_INSTRUCTION),
            "file_read" => digests(FILE_ACCESS_REVIEW_INSTRUCTION),
            "web" => digests(WEB_REVIEW_INSTRUCTION),
            _ => digests(UPLOAD_REVIEW_INSTRUCTION),
        };
        assert_eq!(manifest.instruction, Some(expected), "{kind}");
        seen.push(manifest.instruction.unwrap().sha256);
    }
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 4, "each instruction has its own digest");
}

#[tokio::test]
async fn user_words_are_named_by_evidence_id_and_no_body_is_stored() {
    let mut fixture = Fixture::new(RULES_TEXT);
    let run = run_with(&[("worktree-pr", SkillScope::Builtin, SKILL_BODY)]);
    run.add_request(UserRequest {
        in_reply_to: Some(UserQuestionContext {
            id: "q-9".into(),
            title: "QUESTION-TITLE-needle".into(),
        }),
        evidence: EvidenceRef {
            id: Some("answer-id".into()),
            kind: Some("user_answer".into()),
        },
        ..request("ANSWER-needle: yes, proceed", "unused")
    });
    let model = scripted(APPROVE);
    QuickModelReviewer::new(model)
        .review_scoped_with_context(
            "run-1",
            "git status",
            "check",
            Some(fixture.context(run)),
            ShellAccess::Host,
        )
        .await
        .unwrap();

    let records = fixture.diagnostics();
    let detail = &records[0].detail;
    for body in ["needle", "git status", "QUESTION-TITLE", "yes, proceed"] {
        assert!(!detail.contains(body), "{body} leaked into the record");
    }
    let manifest: ReviewManifest = serde_json::from_str(detail).unwrap();
    let context = manifest.context.unwrap();
    assert_eq!(context.user_request_count, 2);
    assert_eq!(context.user_requests_omitted, 0);
    assert_eq!(
        context.user_requests,
        [
            UserRequestRef {
                evidence_id: Some(EVIDENCE_ID.into()),
                kind: Some("host_chat".into()),
                target_run_id: "run-1".into(),
                answers_question_id: None,
            },
            UserRequestRef {
                evidence_id: Some("answer-id".into()),
                kind: Some("user_answer".into()),
                target_run_id: "run-1".into(),
                answers_question_id: Some("q-9".into()),
            },
        ]
    );
}

#[tokio::test]
async fn skill_and_rules_digests_are_the_text_the_reviewer_received() {
    let mut fixture = Fixture::new(RULES_TEXT);
    let model = scripted(APPROVE);
    review_each_kind(&fixture, &model, "shell_host").await;
    let manifest = fixture.manifest();
    let (_, user) = received(&model).await;
    let sent: serde_json::Value = serde_json::from_str(&user).unwrap();
    let context = manifest.context.unwrap();

    let rules = context.project_rules.unwrap();
    let sent_rules = sent["context"]["project_rules"]["text"].as_str().unwrap();
    assert_eq!(sent_rules, RULES_TEXT);
    assert_eq!(rules.text.digest, digest_of(sent_rules));
    assert!(!rules.text.truncated);
    assert!(rules.path.ends_with("AGENTS.md"));
    assert_eq!(rules.trust, "approved");

    let [skill] = context.loaded_skills.as_slice() else {
        panic!("one skill");
    };
    let sent_skill = &sent["context"]["loaded_skills"][0];
    assert_eq!(sent_skill["text"], SKILL_BODY);
    assert_eq!(skill.text.digest, digest_of(SKILL_BODY));
    assert!(!skill.text.truncated);
    assert_eq!(
        (
            skill.name.as_str(),
            skill.scope.as_str(),
            skill.trust.as_str()
        ),
        ("worktree-pr", "builtin", "builtin")
    );
    assert_eq!(skill.loaded_by, "skill_load");
}

#[tokio::test]
async fn truncation_is_visible_in_the_digest_of_what_was_sent() {
    let long_rules = "r".repeat(usize::try_from(PROJECT_RULES_BYTES).unwrap() + 100);
    let long_skill = "s".repeat(loaded_skills::MAX_SKILL_BYTES + 100);
    let mut fixture = Fixture::new(&long_rules);
    let model = scripted(APPROVE);
    let context = fixture.context(run_with(&[("big", SkillScope::Builtin, &long_skill)]));
    QuickModelReviewer::new(model.clone())
        .review_scoped_with_context("run-1", "pwd", "check", Some(context), ShellAccess::Host)
        .await
        .unwrap();
    let manifest = fixture.manifest();
    let (_, user) = received(&model).await;
    let sent: serde_json::Value = serde_json::from_str(&user).unwrap();
    let context = manifest.context.unwrap();

    let sent_rules = sent["context"]["project_rules"]["text"].as_str().unwrap();
    let rules = context.project_rules.unwrap();
    assert_eq!(sent_rules.len() as u64, PROJECT_RULES_BYTES);
    assert!(rules.text.truncated);
    assert_eq!(rules.text.digest, digest_of(sent_rules));

    let sent_skill = sent["context"]["loaded_skills"][0]["text"]
        .as_str()
        .unwrap();
    assert!(sent_skill.ends_with("…[truncated]"));
    assert!(context.loaded_skills[0].text.truncated);
    assert_eq!(context.loaded_skills[0].text.digest, digest_of(sent_skill));
}

#[tokio::test]
async fn a_rules_file_of_exactly_the_limit_is_not_marked_truncated() {
    let exact = "r".repeat(usize::try_from(PROJECT_RULES_BYTES).unwrap());
    let mut fixture = Fixture::new(&exact);
    let context = fixture.context(run_with(&[]));
    QuickModelReviewer::new(scripted(APPROVE))
        .review_scoped_with_context("run-1", "pwd", "check", Some(context), ShellAccess::Host)
        .await
        .unwrap();
    let rules = fixture.manifest().context.unwrap().project_rules.unwrap();
    assert!(!rules.text.truncated);
    assert_eq!(rules.text.digest.bytes, PROJECT_RULES_BYTES);
}

#[tokio::test]
async fn an_attached_skill_is_hashed_as_detached_and_restored_not_as_typed() {
    let body = "step one\n</skill>\nstep two\n\n<skill name=\"x\">\nnested";
    let mut text = USER_WORDS.to_owned();
    append_skill_attachment(&mut text, "worktree-pr", body);
    let run = ReviewRunContext::empty("run-1");
    run.add_request(request(&text, EVIDENCE_ID));
    let mut fixture = Fixture::new(RULES_TEXT);
    let model = scripted(APPROVE);
    QuickModelReviewer::new(model.clone())
        .review_scoped_with_context(
            "run-1",
            "pwd",
            "check",
            Some(fixture.context(run)),
            ShellAccess::Host,
        )
        .await
        .unwrap();
    let manifest = fixture.manifest();
    let (_, user) = received(&model).await;
    let sent: serde_json::Value = serde_json::from_str(&user).unwrap();

    assert_eq!(sent["context"]["real_user_requests"][0]["text"], USER_WORDS);
    let skill = &manifest.context.unwrap().loaded_skills[0];
    assert_eq!(skill.loaded_by, "user_mention");
    assert_eq!(skill.trust, "user-attached");
    assert_eq!(skill.text.digest, digest_of(body));
    assert_ne!(
        skill.text.digest,
        digest_of(&text),
        "not the expanded message"
    );
    assert_eq!(skill.attached_to_evidence_id.as_deref(), Some(EVIDENCE_ID));
}

#[tokio::test]
async fn the_request_sent_to_the_model_carries_nothing_from_the_record() {
    let mut fixture = Fixture::new(RULES_TEXT);
    let model = scripted(APPROVE);
    review_each_kind(&fixture, &model, "shell_host").await;
    let _ = fixture.manifest();
    let (system, user) = received(&model).await;
    assert_eq!(system, REVIEW_INSTRUCTION);
    let sent: serde_json::Value = serde_json::from_str(&user).unwrap();
    let keys = |value: &serde_json::Value| -> Vec<String> {
        let mut keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        keys
    };
    assert_eq!(
        keys(&sent["context"]),
        [
            "delegation_chain",
            "loaded_skills",
            "project_rules",
            "real_user_requests",
            "root_run_id"
        ]
    );
    assert_eq!(
        keys(&sent["context"]["real_user_requests"][0]),
        ["target_run_id", "text"]
    );
    assert_eq!(
        keys(&sent["context"]["loaded_skills"][0]),
        ["loaded_by", "name", "scope", "text", "trust"]
    );
    assert_eq!(
        keys(&sent["context"]["project_rules"]),
        ["source", "text", "trust"]
    );
    assert!(!user.contains(EVIDENCE_ID));
}

#[tokio::test]
async fn a_record_that_cannot_be_made_leaves_a_warning_and_the_review_is_unchanged() {
    fn refuse(_: &ReviewManifest) -> Result<String, String> {
        Err("disk on fire".into())
    }
    let mut fixture = Fixture::new(RULES_TEXT);
    let run = run_with(&[]);
    let rules = rules_source(fixture._project.path());
    let (context, _) = review_context(run, Some(&rules)).unwrap();
    let context = context.recorded_to(fixture.sink().with_encoder(refuse));
    let model = scripted(APPROVE);
    let verdict = QuickModelReviewer::new(model.clone())
        .review_scoped_with_context("run-1", "pwd", "check", Some(context), ShellAccess::Host)
        .await;
    assert_eq!(verdict, Ok(ReviewVerdict::Approve));

    let (_, user) = received(&model).await;
    let records = fixture.diagnostics();
    let [warning] = records.as_slice() else {
        panic!("one warning");
    };
    assert_eq!(warning.severity, event_bus::DiagnosticSeverity::Warning);
    assert!(warning.detail.contains("disk on fire"));
    assert!(
        warning
            .detail
            .contains(&format!("payload_sha256={}", digest_of(&user).sha256))
    );
}

#[tokio::test]
async fn failed_reviews_are_recorded_with_the_payload_that_was_sent() {
    for (script, expected) in [
        (
            Err(crate::RuntimeError::Model {
                reason: "down".into(),
            }),
            OutcomeStatus::ModelError,
        ),
        (
            Ok(text_response("not json", FinishReason::Stop)),
            OutcomeStatus::InvalidVerdict,
        ),
        (
            // A verdict without risk and authorization is invalid in a run context.
            Ok(text_response(r#"{"approve":true}"#, FinishReason::Stop)),
            OutcomeStatus::InvalidVerdict,
        ),
    ] {
        let mut fixture = Fixture::new(RULES_TEXT);
        let model = Arc::new(ScriptedModel::new([script]));
        let context = fixture.context(run_with(&[]));
        let verdict = QuickModelReviewer::new(model.clone())
            .review_scoped_with_context("run-1", "pwd", "check", Some(context), ShellAccess::Host)
            .await;
        assert!(verdict.is_err());
        let manifest = fixture.manifest();
        let (_, user) = received(&model).await;
        assert_eq!(manifest.outcome.status, expected);
        assert_eq!(manifest.outcome.verdict, None);
        assert_eq!(manifest.payload, Some(digest_of(&user)));
    }
}

#[tokio::test]
async fn a_denial_decided_by_the_threshold_records_both_the_verdict_and_the_decision() {
    let mut fixture = Fixture::new(RULES_TEXT);
    let context = fixture.context(run_with(&[]));
    let high_without_authorization = r#"{"approve":true,"reason":"push","risk_level":"high","authorization_level":"low","procedure_step":"none"}"#;
    let verdict = QuickModelReviewer::new(scripted(high_without_authorization))
        .review_scoped_with_context(
            "run-1",
            "git push",
            "ship",
            Some(context),
            ShellAccess::Host,
        )
        .await
        .unwrap();
    assert!(matches!(verdict, ReviewVerdict::Deny { .. }));
    let verdict = fixture.manifest().outcome.verdict.unwrap();
    assert!(verdict.approve, "the model approved");
    assert_eq!(verdict.risk_level, Some(RiskLevel::High));
    assert_eq!(verdict.decision, Decision::Deny);
}

#[tokio::test]
async fn reviews_that_never_reach_the_model_leave_no_record() {
    let mut fixture = Fixture::new(RULES_TEXT);
    let model = Arc::new(ScriptedModel::new([]));
    let empty = review_context(ReviewRunContext::empty("run-1"), None)
        .unwrap()
        .0;
    let verdict = QuickModelReviewer::new(model.clone())
        .review_scoped_with_context(
            "run-1",
            "pwd",
            "check",
            Some(empty.recorded_to(fixture.sink())),
            ShellAccess::Host,
        )
        .await;
    assert!(matches!(verdict, Ok(ReviewVerdict::Deny { .. })));
    assert!(model.observed().await.is_empty());
    assert!(fixture.diagnostics().is_empty());
}

#[tokio::test]
async fn a_review_dropped_after_sending_is_recorded_as_cancelled() {
    let mut fixture = Fixture::new(RULES_TEXT);
    let gate = Arc::new(tokio::sync::Notify::new());
    let model = Arc::new(ScriptedModel::gated([], gate));
    let context = fixture.context(run_with(&[]));
    let reviewer = QuickModelReviewer::new(model.clone());
    {
        let review = reviewer.review_scoped_with_context(
            "run-1",
            "pwd",
            "check",
            Some(context),
            ShellAccess::Host,
        );
        tokio::select! {
            biased;
            _ = model.wait_for_request(0) => {}
            _ = review => panic!("the gated model never answers"),
        }
    }
    let (_, user) = received(&model).await;
    let manifest = fixture.manifest();
    assert_eq!(manifest.outcome.status, OutcomeStatus::Cancelled);
    assert_eq!(manifest.payload, Some(digest_of(&user)));
}

#[tokio::test]
async fn a_live_answer_has_the_evidence_id_storage_gave_it() {
    struct Pending;
    #[async_trait::async_trait]
    impl crate::AgentModel for Pending {
        fn selected_model(&self, _: Role, _: Option<&str>) -> String {
            "pending".into()
        }
        async fn complete(
            &self,
            _: &AgentInvocationContext,
            _: Role,
            _: &[Message],
            _: &[providers::ToolSpec],
        ) -> Result<providers::ChatResponse, crate::RuntimeError> {
            std::future::pending().await
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("answer.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(32));
    let runtime = crate::AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        Arc::new(Pending),
    )
    .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
    let root = runtime.reserve_run_id();
    runtime.spawn_reserved(
        root,
        None,
        Role::Orchestrator,
        "Investigate",
        crate::RunConfig::default(),
    );
    let question = runtime
        .request_user_question(root, "May I clean up?".into(), vec![], true)
        .unwrap();
    runtime
        .answer_user_question(&question.id, "Yes, clean up")
        .unwrap();

    let live = {
        let runs = runtime.shared.review_runs.lock().unwrap();
        runs[&root].requests().0
    };
    let answer = live
        .iter()
        .find(|request| request.in_reply_to.is_some())
        .unwrap();
    let stored = runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .review_evidence_for_lineage(&[root.to_string()])
        .unwrap();
    let row = stored.iter().find(|row| row.kind == "user_answer").unwrap();
    assert_eq!(answer.evidence.id.as_deref(), Some(row.id.as_str()));
    assert_eq!(answer.evidence.kind.as_deref(), Some("user_answer"));
    let _ = runtime.cancel(root);
}

#[tokio::test]
async fn the_gate_records_a_shell_review_with_the_call_it_decided() {
    use tools::tools::shell_escalation::{EscalationDecision, ShellEscalationGate};
    let bus = Arc::new(EventBus::new(64));
    let model = scripted(APPROVE);
    let runtime = crate::AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus.clone())),
        model.clone(),
    );
    let id = runtime.reserve_run_id();
    let run = ReviewRunContext::empty(&id.to_string());
    run.add_user_request(&id.to_string(), "Check the repository status");
    runtime.shared.review_runs.lock().unwrap().insert(id, run);
    let gate = SandboxEscalationGate::new(
        Arc::new(Mutex::new((config::EscalationApproval::Auto, false))),
        Some(Arc::new(QuickModelReviewer::new(model.clone()))),
        bus.clone(),
    )
    .with_runtime(Arc::downgrade(&runtime.shared));
    let mut events = bus.subscribe();
    let ctx = tools::ToolExecutionContext {
        run_id: id.to_string(),
        thread_id: Some("thread-7".into()),
        call_id: Some("call-7".into()),
        ..Default::default()
    };

    let decision = gate.decide(&ctx, "git status", "inspect").await;
    assert!(matches!(decision, EscalationDecision::Approve));

    let diagnostics: Vec<_> = events
        .drain_pending_snapshot()
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::Diagnostic(diagnostic) => Some(diagnostic),
            _ => None,
        })
        .collect();
    let codes: Vec<_> = diagnostics.iter().map(|d| d.code.as_str()).collect();
    assert_eq!(
        codes,
        [
            diagnostic_codes::ESCALATION_REVIEW_MANIFEST,
            diagnostic_codes::ESCALATION_REVIEW
        ],
        "the record precedes the decision"
    );
    for diagnostic in &diagnostics {
        assert_eq!(diagnostic.call_id.as_deref(), Some("call-7"));
        assert_eq!(diagnostic.thread_id.as_deref(), Some("thread-7"));
    }
    let manifest: ReviewManifest = serde_json::from_str(&diagnostics[0].detail).unwrap();
    assert_eq!(manifest.scope, "host_unsandboxed");
    let (_, user) = received(&model).await;
    assert_eq!(manifest.payload, Some(digest_of(&user)));
    assert_eq!(manifest.context.unwrap().lineage_run_ids, [id.to_string()]);
}
