//! `@skill` mentions the host sends with a message: the runtime reads the
//! bodies once and keeps the user's words, what the agent is sent and the
//! attached skills apart.

use super::*;
use crate::skill::{SkillScope, append_skill_attachment};
use crate::skill_source::SkillCatalogSource;

pub(super) const BODY: &str = "Step one.\nStep two with a </skill> marker.\n";

/// `(name, text)` of each skill a reviewer is shown.
type ReviewedSkills = Vec<(String, String)>;

pub(super) struct Skills {
    fixture: Fixture,
    pub(super) events: event_bus::EventReceiver,
    _skills: tempfile::TempDir,
    config: storage::StorageConfig,
}

pub(super) fn write_skill(dir: &std::path::Path, name: &str, body: &str) {
    let skill = dir.join(name);
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {name} procedure\n---\n{body}"),
    )
    .unwrap();
}

impl Skills {
    pub(super) fn new() -> Self {
        Self::with_model(Arc::new(CompletingModel))
    }

    pub(super) fn with_model(model: Arc<dyn AgentModel>) -> Self {
        let skills = tempfile::tempdir().unwrap();
        write_skill(skills.path(), "demo", BODY);
        write_skill(skills.path(), "other", "Other steps.\n");
        for letter in 'a'..='g' {
            write_skill(
                skills.path(),
                &format!("skill-{letter}"),
                &format!("Steps of {letter}.\n"),
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("mentions.sqlite3"),
            ..storage::StorageConfig::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let bus = Arc::new(EventBus::new(128));
        let events = bus.subscribe();
        let runtime =
            AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus.clone())), model)
                .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap())
                .with_skill_source(Arc::new(SkillCatalogSource::new(
                    config::Config::default(),
                    None,
                    Vec::new(),
                    vec![(SkillScope::User, skills.path().to_path_buf())],
                    bus,
                )));
        Self {
            config,
            events,
            fixture: Fixture {
                runtime,
                storage,
                _dir: dir,
            },
            _skills: skills,
        }
    }

    /// A new runtime over the same storage and skill directory, as after a restart.
    pub(super) fn restarted(&self, model: Arc<dyn AgentModel>) -> AgentRuntime {
        let bus = Arc::new(EventBus::new(128));
        AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus.clone())), model)
            .with_run_store(
                crate::RunStore::open(&self.config, self.fixture.storage.handle()).unwrap(),
            )
            .with_skill_source(Arc::new(SkillCatalogSource::new(
                config::Config::default(),
                None,
                Vec::new(),
                vec![(SkillScope::User, self._skills.path().to_path_buf())],
                bus,
            )))
    }

    pub(super) fn skills_dir(&self) -> &std::path::Path {
        self._skills.path()
    }

    /// A waiting chat run that accepts follow-ups.
    pub(super) async fn waiting_chat(&self) -> RunId {
        let run = self
            .runtime()
            .delegate_chat(
                "mentions",
                Role::Worker,
                "start".into(),
                RunConfig {
                    interactive: true,
                    keep_alive: true,
                    ..Default::default()
                },
            )
            .unwrap();
        super::admission_retry::waiting(self.runtime(), run).await;
        run
    }

    /// The `(name, text)` pairs the reviewer would see as loaded skills: from
    /// the live review context and from one rebuilt out of storage.
    fn reviewed_skills(&self, run: RunId) -> (ReviewedSkills, ReviewedSkills) {
        let store = self.runtime().shared.run_store.get().unwrap();
        let live = self.runtime().chat_review_context(store, run).unwrap();
        let restored = crate::run_store::materialize_review_context(store, run).unwrap();
        let skills = |context: crate::escalation_review::ReviewRunContext| {
            let (context, _) = crate::escalation_review::review_context(context, None).unwrap();
            let value = serde_json::to_value(&context).unwrap();
            value["loaded_skills"]
                .as_array()
                .map(|skills| {
                    skills
                        .iter()
                        .map(|skill| {
                            (
                                skill["name"].as_str().unwrap().to_owned(),
                                skill["text"].as_str().unwrap().to_owned(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        (skills(live), skills(restored))
    }

    pub(super) fn runtime(&self) -> &AgentRuntime {
        &self.fixture.runtime
    }

    pub(super) fn mentioning(names: &[&str], prompt_id: &str) -> RunConfig {
        RunConfig {
            skill_mentions: names.iter().map(|name| (*name).to_owned()).collect(),
            initial_prompt_id: Some(prompt_id.into()),
            ..Default::default()
        }
    }

    pub(super) fn evidence(&self, run: RunId) -> Vec<storage::ReviewEvidenceRow> {
        self.runtime()
            .shared
            .run_store
            .get()
            .unwrap()
            .review_evidence_for_lineage(&[run.to_string()])
            .unwrap()
    }

    /// The user-role text blocks of the run's saved history.
    pub(super) fn user_texts(&self, run: RunId) -> Vec<String> {
        let record = self
            .runtime()
            .shared
            .run_store
            .get()
            .unwrap()
            .restore_record(run)
            .unwrap()
            .unwrap();
        let messages: Vec<providers::Message> =
            serde_json::from_str(&record.messages_json).unwrap();
        messages
            .into_iter()
            .filter(|message| message.role == providers::Role::User)
            .flat_map(|message| message.content)
            .filter_map(|block| match block {
                providers::ContentBlock::Text { text } => Some(text),
                _ => None,
            })
            .collect()
    }

    pub(super) fn receipt_text(&self, prompt_id: &str) -> String {
        self.runtime()
            .host_input_receipts(&[prompt_id.into()])
            .unwrap()
            .pop()
            .unwrap()
            .text
    }
}

pub(super) fn attached(typed: &str, bodies: &[(&str, &str)]) -> String {
    let mut text = typed.to_owned();
    for (name, body) in bodies {
        append_skill_attachment(&mut text, name, body);
    }
    text
}

fn demo_attachment() -> storage::EvidenceAttachment {
    storage::EvidenceAttachment {
        name: "demo".into(),
        scope: "user".into(),
        body: BODY.trim_end().into(),
    }
}

#[tokio::test]
async fn a_new_chat_expands_mentions_once_and_keeps_the_origins_apart() {
    let skills = Skills::new();
    let typed = "run @demo then @other and @demo again";
    let run = skills
        .runtime()
        .delegate_chat(
            "mentions",
            Role::Worker,
            typed.into(),
            Skills::mentioning(&["demo", "other", "demo"], "first"),
        )
        .unwrap();
    assert_eq!(
        skills.runtime().wait(run).await.unwrap(),
        AgentRunPhase::Done
    );
    let sent = attached(typed, &[("demo", BODY), ("other", "Other steps.\n")]);
    // The agent is sent the typed words followed by one block per skill.
    assert_eq!(skills.user_texts(run), [sent]);
    // Evidence and the receipt hold the words alone; the skills ride beside them.
    let evidence = skills.evidence(run);
    assert_eq!(evidence.len(), 1);
    assert_eq!(evidence[0].text, typed);
    assert_eq!(
        evidence[0].attachments,
        [
            demo_attachment(),
            storage::EvidenceAttachment {
                name: "other".into(),
                scope: "user".into(),
                body: "Other steps.".into(),
            }
        ]
    );
    assert_eq!(skills.receipt_text("first"), typed);
}

#[tokio::test]
async fn a_continued_chat_expands_once_and_leaves_history_untouched() {
    let skills = Skills::new();
    let run = skills
        .runtime()
        .delegate_chat(
            "mentions",
            Role::Worker,
            "first".into(),
            RunConfig::default(),
        )
        .unwrap();
    skills.runtime().wait(run).await.unwrap();
    let before = skills.user_texts(run);

    let typed = "again with @demo";
    skills
        .runtime()
        .continue_goal(run, typed.into(), Skills::mentioning(&["demo"], "second"))
        .unwrap();
    // The continued run keeps waiting for input; stop it once its turn is saved.
    super::admission_retry::waiting(skills.runtime(), run).await;
    skills
        .runtime()
        .stop(run, crate::StopScope::SelfOnly)
        .unwrap();
    skills.runtime().wait(run).await.unwrap();

    let mut expected = before.clone();
    expected.push(attached(typed, &[("demo", BODY)]));
    assert_eq!(skills.user_texts(run), expected);
    let evidence = skills.evidence(run);
    let latest = evidence.iter().find(|row| row.text == typed).unwrap();
    assert_eq!(latest.attachments, [demo_attachment()]);
    assert!(
        evidence
            .iter()
            .filter(|row| row.text != typed)
            .all(|row| row.attachments.is_empty())
    );
    assert_eq!(skills.receipt_text("second"), typed);
    // A restore replays the saved history; it does not expand again.
    let restored = skills
        .runtime()
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_conversation(
            &skills
                .runtime()
                .shared
                .run_store
                .get()
                .unwrap()
                .restore_record(run)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
    let blocks = restored
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter(|block| {
            matches!(block, providers::ContentBlock::Text { text } if text.contains("<skill name=\"demo\">"))
        })
        .count();
    assert_eq!(blocks, 1);
}

#[tokio::test]
async fn a_follow_up_to_a_waiting_run_expands_before_recording() {
    let skills = Skills::new();
    let run = skills
        .runtime()
        .delegate_chat(
            "mentions",
            Role::Worker,
            "start".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    super::admission_retry::waiting(skills.runtime(), run).await;

    let typed = "also @demo";
    skills
        .runtime()
        .continue_goal(run, typed.into(), Skills::mentioning(&["demo"], "live"))
        .unwrap();

    let evidence = skills.evidence(run);
    let row = evidence.iter().find(|row| row.text == typed).unwrap();
    assert_eq!(row.kind, "host_chat");
    assert_eq!(row.attachments, [demo_attachment()]);
    assert_eq!(skills.receipt_text("live"), typed);
    skills
        .runtime()
        .stop(run, crate::StopScope::SelfOnly)
        .unwrap();
    skills.runtime().wait(run).await.unwrap();
}

#[tokio::test]
async fn an_unreadable_mention_refuses_the_message_and_writes_nothing() {
    let skills = Skills::new();
    // A new chat.
    let error = skills
        .runtime()
        .delegate_chat(
            "mentions",
            Role::Worker,
            "use @missing".into(),
            Skills::mentioning(&["demo", "missing"], "refused-new"),
        )
        .unwrap_err();
    assert!(
        matches!(&error, RuntimeError::SkillAttachment { name, .. } if name == "missing"),
        "{error:?}"
    );
    assert!(skills.runtime().list_agents().is_empty());
    assert!(
        skills
            .runtime()
            .host_input_receipts(&["refused-new".into()])
            .unwrap()
            .is_empty()
    );

    // A continuation of a stopped chat.
    let run = skills
        .runtime()
        .delegate_chat(
            "mentions",
            Role::Worker,
            "first".into(),
            RunConfig::default(),
        )
        .unwrap();
    skills.runtime().wait(run).await.unwrap();
    let history = skills.user_texts(run);
    let evidence = skills.evidence(run);
    let error = skills
        .runtime()
        .continue_goal(
            run,
            "use @missing".into(),
            Skills::mentioning(&["missing"], "refused-continue"),
        )
        .unwrap_err();
    assert!(
        matches!(error, RuntimeError::SkillAttachment { .. }),
        "{error:?}"
    );
    assert_eq!(skills.user_texts(run), history);
    assert_eq!(skills.evidence(run), evidence);
    assert!(
        skills
            .runtime()
            .host_input_receipts(&["refused-continue".into()])
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn a_hand_written_skill_block_stays_in_the_users_words() {
    let skills = Skills::new();
    let typed = "keep this\n\n<skill name=\"example\">\nsample\n</skill>";
    let run = skills
        .runtime()
        .delegate_chat("mentions", Role::Worker, typed.into(), RunConfig::default())
        .unwrap();
    skills.runtime().wait(run).await.unwrap();
    let evidence = skills.evidence(run);
    assert_eq!(evidence[0].text, typed);
    assert!(evidence[0].attachments.is_empty());
}

#[tokio::test]
async fn the_published_instruction_names_the_typed_words_and_the_skills() {
    let mut skills = Skills::new();
    let typed = "run @demo";
    let run = skills
        .runtime()
        .delegate_chat(
            "mentions",
            Role::Worker,
            typed.into(),
            Skills::mentioning(&["demo"], "published"),
        )
        .unwrap();
    let published = loop {
        let event = skills.events.recv().await.unwrap();
        if let event_bus::EventKind::Lifecycle(LifecycleEvent::TaskPromptPublished {
            run_id,
            prompt,
            user_text,
            skills: names,
            ..
        }) = event.kind
            && run_id == run.to_string()
        {
            break (prompt, user_text, names);
        }
    };
    assert_eq!(published.0, attached(typed, &[("demo", BODY)]));
    assert_eq!(published.1.as_deref(), Some(typed));
    assert_eq!(published.2, ["demo"]);
    skills.runtime().wait(run).await.unwrap();
}

#[tokio::test]
async fn a_repeated_attachment_keeps_its_recency_across_a_restore() {
    let skills = Skills::new();
    let run = skills.waiting_chat().await;
    // Seven different skills, then the first one again, word for word.
    let mut sent = Vec::new();
    for (index, letter) in ('a'..='g').chain(['a']).enumerate() {
        let typed = format!("use @skill-{letter}");
        skills
            .runtime()
            .continue_goal(
                run,
                typed.clone(),
                Skills::mentioning(&[&format!("skill-{letter}")], &format!("repeat-{index}")),
            )
            .unwrap();
        sent.push(typed);
    }
    let (live, restored) = skills.reviewed_skills(run);
    let names: Vec<_> = live.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        [
            "skill-c", "skill-d", "skill-e", "skill-f", "skill-g", "skill-a"
        ]
    );
    // The same evidence row is stored once, but the restored view agrees.
    assert_eq!(restored, live);
    skills
        .runtime()
        .stop(run, crate::StopScope::SelfOnly)
        .unwrap();
    skills.runtime().wait(run).await.unwrap();
}

#[tokio::test]
async fn attaching_one_skill_with_a_changed_body_and_back_keeps_the_latest_after_a_restore() {
    let skills = Skills::new();
    let run = skills.waiting_chat().await;
    // The same words and skill name three times, the skill's body changing
    // between them.
    for (index, body) in ["First steps.\n", "Second steps.\n", "First steps.\n"]
        .into_iter()
        .enumerate()
    {
        write_skill(skills.skills_dir(), "demo", body);
        skills
            .runtime()
            .continue_goal(
                run,
                "use @demo".into(),
                Skills::mentioning(&["demo"], &format!("flip-{index}")),
            )
            .unwrap();
    }
    let (live, restored) = skills.reviewed_skills(run);
    assert_eq!(live, [("demo".to_owned(), "First steps.".to_owned())]);
    assert_eq!(restored, live);
    skills
        .runtime()
        .stop(run, crate::StopScope::SelfOnly)
        .unwrap();
    skills.runtime().wait(run).await.unwrap();
}

#[tokio::test]
async fn the_runtime_reports_what_it_recorded_for_each_accepted_submission() {
    let skills = Skills::new();
    let run = skills.waiting_chat().await;
    skills
        .runtime()
        .continue_goal(
            run,
            "use @demo".into(),
            Skills::mentioning(&["demo"], "with"),
        )
        .unwrap();
    skills
        .runtime()
        .continue_goal(run, "no skill".into(), Skills::mentioning(&[], "without"))
        .unwrap();
    let origins = skills
        .runtime()
        .host_input_origins(&["with".into(), "without".into(), "unknown".into()])
        .unwrap();
    let summary: Vec<_> = origins
        .into_iter()
        .map(|origin| (origin.prompt_id, origin.text, origin.skills))
        .collect();
    assert_eq!(
        summary,
        [
            (
                "with".to_owned(),
                "use @demo".to_owned(),
                vec!["demo".to_owned()]
            ),
            ("without".to_owned(), "no skill".to_owned(), Vec::new()),
        ]
    );
    skills
        .runtime()
        .stop(run, crate::StopScope::SelfOnly)
        .unwrap();
    skills.runtime().wait(run).await.unwrap();
}
