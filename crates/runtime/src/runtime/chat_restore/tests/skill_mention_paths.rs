//! The real input paths around `@skill` mentions: restarts, forks and rewinds,
//! follow-ups queued behind a running turn, and refusals.

use super::skill_mentions::{BODY, Skills, attached, write_skill};
use super::*;
use crate::ChatForkSeed;

/// Completes every request with "done" and records the user text it was sent.
/// The first request waits until the test releases it, so a run stays
/// `Running` while follow-ups are queued behind it.
struct GatedModel {
    gate_first: bool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    requests: std::sync::Mutex<Vec<Vec<String>>>,
    completed: tokio::sync::mpsc::UnboundedSender<()>,
}

impl GatedModel {
    fn new(gate_first: bool) -> (Arc<Self>, tokio::sync::mpsc::UnboundedReceiver<()>) {
        let (completed, receiver) = tokio::sync::mpsc::unbounded_channel();
        let model = Arc::new(Self {
            gate_first,
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            requests: std::sync::Mutex::new(Vec::new()),
            completed,
        });
        (model, receiver)
    }

    fn requests(&self) -> Vec<Vec<String>> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl AgentModel for GatedModel {
    async fn complete(
        &self,
        invocation: &crate::AgentInvocationContext,
        role: Role,
        messages: &[providers::Message],
        tools: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        let first = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(
                messages
                    .iter()
                    .filter(|message| message.role == providers::Role::User)
                    .flat_map(|message| &message.content)
                    .filter_map(|block| match block {
                        providers::ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect(),
            );
            requests.len() == 1
        };
        if self.gate_first && first {
            self.entered.notify_one();
            self.release.notified().await;
        }
        let response = CompletingModel
            .complete(invocation, role, messages, tools)
            .await;
        let _ = self.completed.send(());
        response
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test".into()
    }
}

fn chat_config() -> RunConfig {
    RunConfig {
        interactive: true,
        keep_alive: true,
        ..RunConfig::default()
    }
}

async fn stop(runtime: &AgentRuntime, run: RunId) {
    runtime.stop(run, crate::StopScope::SelfOnly).unwrap();
    runtime.wait(run).await.unwrap();
}

fn saved(skills: &Skills, run: RunId) -> storage::RunContextRecord {
    skills
        .runtime()
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_record(run)
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn editing_or_deleting_a_skill_after_it_was_attached_changes_nothing_saved() {
    let skills = Skills::new();
    let typed = "run @demo first";
    let run = skills
        .runtime()
        .delegate_chat(
            "restart",
            Role::Worker,
            typed.into(),
            Skills::mentioning(&["demo"], "first"),
        )
        .unwrap();
    skills.runtime().wait(run).await.unwrap();
    let before = saved(&skills, run);
    let evidence_before = skills.evidence(run);
    // The skill changes and another disappears, and the runtime is rebuilt.
    write_skill(skills.skills_dir(), "demo", "Changed steps.\n");
    std::fs::remove_dir_all(skills.skills_dir().join("other")).unwrap();
    let restarted = skills.restarted(Arc::new(CompletingModel));

    // A plain follow-up restores the saved history without expanding anything.
    restarted
        .continue_goal(run, "plain follow-up".into(), chat_config())
        .unwrap();
    super::admission_retry::waiting(&restarted, run).await;
    stop(&restarted, run).await;
    let texts = skills.user_texts(run);
    assert_eq!(texts[0], attached(typed, &[("demo", BODY)]));
    assert_eq!(texts[1..], ["plain follow-up"]);
    let after = saved(&skills, run);
    let prefix: Vec<providers::Message> = serde_json::from_str(&before.messages_json).unwrap();
    let restored: Vec<providers::Message> = serde_json::from_str(&after.messages_json).unwrap();
    assert_eq!(restored[..prefix.len()], prefix[..]);
    assert_eq!(
        skills.evidence(run)[..evidence_before.len()],
        evidence_before[..]
    );
    assert_eq!(skills.evidence(run)[0].attachments[0].body, BODY.trim_end());

    // A new mention reads the skill as it is now, once; the old message keeps its body.
    restarted
        .continue_goal(
            run,
            "again @demo".into(),
            Skills::mentioning(&["demo"], "second"),
        )
        .unwrap();
    super::admission_retry::waiting(&restarted, run).await;
    stop(&restarted, run).await;
    let texts = skills.user_texts(run);
    assert_eq!(
        texts[2],
        attached("again @demo", &[("demo", "Changed steps.\n")])
    );
    assert_eq!(texts[0], attached(typed, &[("demo", BODY)]));
    assert_eq!(
        texts
            .iter()
            .filter(|text| text.contains("<skill name="))
            .count(),
        2
    );

    // A mention of the deleted skill is refused and writes nothing.
    let history = saved(&skills, run).messages_json;
    let error = restarted
        .continue_goal(
            run,
            "gone @other".into(),
            Skills::mentioning(&["other"], "third"),
        )
        .unwrap_err();
    assert!(
        matches!(error, RuntimeError::SkillAttachment { .. }),
        "{error:?}"
    );
    assert_eq!(saved(&skills, run).messages_json, history);
}

async fn turn_boundary(events: &mut event_bus::EventReceiver, run: RunId) -> u64 {
    loop {
        let event = events.recv().await.expect("event bus open");
        if let event_bus::EventKind::Lifecycle(LifecycleEvent::TurnCompleted {
            run_id,
            context_len,
        }) = event.kind
            && run_id == run.to_string()
        {
            return context_len;
        }
    }
}

#[tokio::test]
async fn forks_and_rewinds_keep_the_old_expansion_and_expand_only_the_new_input() {
    let mut skills = Skills::new();
    let source = skills
        .runtime()
        .delegate_chat(
            "source",
            Role::Worker,
            "start @demo".into(),
            RunConfig {
                skill_mentions: vec!["demo".into()],
                ..chat_config()
            },
        )
        .unwrap();
    let first = turn_boundary(&mut skills.events, source).await;
    skills
        .runtime()
        .send_message(source, "second turn".into())
        .unwrap();
    turn_boundary(&mut skills.events, source).await;
    // The skill is edited before the branches are taken.
    write_skill(skills.skills_dir(), "demo", "Changed steps.\n");
    let seed = || {
        Some(ChatForkSeed {
            source_run_id: source.to_string(),
            context_len: first,
        })
    };

    // A branch with a new mention expands it once and keeps the old block as it was.
    let with_mention = skills
        .runtime()
        .delegate_chat_seeded(
            "fork-mention",
            Role::Worker,
            "branch @demo".into(),
            RunConfig {
                skill_mentions: vec!["demo".into()],
                ..chat_config()
            },
            seed(),
        )
        .unwrap();
    super::admission_retry::waiting(skills.runtime(), with_mention).await;
    let texts = skills.user_texts(with_mention);
    assert_eq!(
        texts,
        [
            attached("start @demo", &[("demo", BODY)]),
            attached("branch @demo", &[("demo", "Changed steps.\n")]),
        ]
    );

    // A branch without a mention adds no block at all: the old one is not expanded again.
    let plain = skills
        .runtime()
        .delegate_chat_seeded(
            "fork-plain",
            Role::Worker,
            "branch words".into(),
            chat_config(),
            seed(),
        )
        .unwrap();
    super::admission_retry::waiting(skills.runtime(), plain).await;
    let texts = skills.user_texts(plain);
    assert_eq!(
        texts,
        [
            attached("start @demo", &[("demo", BODY)]),
            "branch words".to_owned()
        ]
    );
    assert_eq!(
        texts
            .iter()
            .map(|text| text.matches("<skill name=").count())
            .sum::<usize>(),
        1
    );
    for run in [with_mention, plain, source] {
        stop(skills.runtime(), run).await;
    }
}

#[tokio::test]
async fn a_follow_up_queued_behind_a_running_turn_reads_the_skill_when_it_is_sent() {
    let (model, mut completed) = GatedModel::new(true);
    let skills = Skills::with_model(model.clone());
    let run = skills
        .runtime()
        .delegate_chat("queue", Role::Worker, "start".into(), chat_config())
        .unwrap();
    model.entered.notified().await;
    // The run is busy: this follow-up waits in its inbox with the skill already read.
    skills
        .runtime()
        .continue_goal(
            run,
            "queued @demo".into(),
            Skills::mentioning(&["demo"], "queued"),
        )
        .unwrap();
    // The skill changes, then the queued message is withdrawn to be edited and sent again.
    write_skill(skills.skills_dir(), "demo", "Changed steps.\n");
    skills.runtime().withdraw_follow_up(run, "queued").unwrap();
    skills
        .runtime()
        .continue_goal(
            run,
            "edited @demo".into(),
            Skills::mentioning(&["demo"], "edited"),
        )
        .unwrap();
    // Each submission keeps its own record: the words, the skill and the body it saw.
    let origins = skills
        .runtime()
        .host_input_origins(&["queued".into(), "edited".into()])
        .unwrap();
    let summary: Vec<_> = origins
        .into_iter()
        .map(|origin| (origin.text, origin.skills))
        .collect();
    assert_eq!(
        summary,
        [
            ("queued @demo".to_owned(), vec!["demo".to_owned()]),
            ("edited @demo".to_owned(), vec!["demo".to_owned()]),
        ]
    );
    let bodies = |text: &str| {
        skills
            .evidence(run)
            .into_iter()
            .find(|row| row.text == text)
            .map(|row| row.attachments[0].body.clone())
    };
    assert_eq!(bodies("queued @demo").as_deref(), Some(BODY.trim_end()));
    assert_eq!(bodies("edited @demo").as_deref(), Some("Changed steps."));

    model.release.notify_one();
    completed.recv().await.unwrap();
    completed.recv().await.unwrap();
    // The agent saw only the edited message, expanded with the skill as it was then.
    let requests = model.requests();
    assert_eq!(
        requests[1],
        [
            "start".to_owned(),
            attached("edited @demo", &[("demo", "Changed steps.\n")])
        ]
    );
    stop(skills.runtime(), run).await;
}

fn secret_body() -> String {
    format!("Use this key: sk-{}\n", "a".repeat(40))
}

#[tokio::test]
async fn refused_attachments_leave_no_evidence_receipt_or_snapshot() {
    let (model, _completed) = GatedModel::new(false);
    let skills = Skills::with_model(model.clone());
    write_skill(skills.skills_dir(), "leaky", &secret_body());
    write_skill(skills.skills_dir(), "huge", &"x".repeat(256 * 1024 + 16));
    let run = skills.waiting_chat().await;
    let requests_before = model.requests().len();
    let snapshot = |skills: &Skills| {
        let record = saved(skills, run);
        (record.messages_json, record.host_inputs_json)
    };

    for (name, id) in [
        ("missing", "unknown"),
        ("leaky", "secret"),
        ("huge", "size"),
    ] {
        let evidence = skills.evidence(run);
        let history = snapshot(&skills);
        // A running chat (live path) refuses the message whole.
        let error = skills
            .runtime()
            .continue_goal(run, format!("use @{name}"), Skills::mentioning(&[name], id))
            .unwrap_err();
        assert!(
            matches!(
                error,
                RuntimeError::SkillAttachment { .. } | RuntimeError::ReviewEvidence { .. }
            ),
            "{name}: {error:?}"
        );
        assert_eq!(skills.evidence(run), evidence, "{name}");
        assert_eq!(snapshot(&skills), history, "{name}");
        assert!(
            skills
                .runtime()
                .host_input_receipts(&[id.into()])
                .unwrap()
                .is_empty(),
            "{name}"
        );
    }
    stop(skills.runtime(), run).await;
    // Nothing reached the agent, not even after the run had time to drain its inbox.
    assert_eq!(model.requests().len(), requests_before);

    // The same refusals against a stopped chat (restore path) and a new chat.
    let history = snapshot(&skills);
    let evidence = skills.evidence(run);
    for (name, id) in [
        ("missing", "unknown-2"),
        ("leaky", "secret-2"),
        ("huge", "size-2"),
    ] {
        let error = skills
            .runtime()
            .continue_goal(run, format!("use @{name}"), Skills::mentioning(&[name], id))
            .unwrap_err();
        assert!(
            matches!(
                error,
                RuntimeError::SkillAttachment { .. } | RuntimeError::ReviewEvidence { .. }
            ),
            "{name}: {error:?}"
        );
        assert_eq!(snapshot(&skills), history, "{name}");
        assert_eq!(skills.evidence(run), evidence, "{name}");
        assert!(
            skills
                .runtime()
                .host_input_receipts(&[id.into()])
                .unwrap()
                .is_empty(),
            "{name}"
        );
        let error = skills
            .runtime()
            .delegate_chat(
                &format!("fresh-{name}"),
                Role::Worker,
                format!("use @{name}"),
                Skills::mentioning(&[name], &format!("fresh-{id}")),
            )
            .unwrap_err();
        assert!(
            matches!(
                error,
                RuntimeError::SkillAttachment { .. } | RuntimeError::ReviewEvidence { .. }
            ),
            "{name}: {error:?}"
        );
        assert!(
            skills
                .runtime()
                .host_input_receipts(&[format!("fresh-{id}")])
                .unwrap()
                .is_empty(),
            "{name}"
        );
    }
    assert_eq!(skills.runtime().list_agents().len(), 1);
}
