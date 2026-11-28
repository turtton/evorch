//! `@skill` mentions: the completion offers the skills the core lists for the
//! project, and a message names the listed skills it mentions.
use egui::Key;
use egui_kittest::kittest::Queryable;
use workbench_protocol::commands::{Command, SendChat};
use workbench_protocol::ids::ProjectId;
use workbench_protocol::namespaces::workspace::SkillSummary;
use workbench_protocol::namespaces::{ConversationCommand, WorkspaceQuery, WorkspaceReply};
use workbench_protocol::queries::QueryReply;

use super::*;
use crate::core_client::features::FeatureError;

const WAITING: &str = "Reading the project's skills for the @mentions. Send again in a moment.";
const UNLISTED: &str =
    "The skill list could not be read, so the @mentions were sent as plain text.";

fn summary(name: &str) -> SkillSummary {
    SkillSummary {
        name: name.into(),
        description: format!("{name} procedure"),
    }
}

impl Screen {
    fn write(&mut self, text: &str) {
        self.harness.get_by_label("Message").focus();
        self.harness.run_steps(2);
        self.harness.get_by_label("Message").type_text(text);
        self.harness.run_steps(2);
    }

    fn submit(&mut self) {
        self.harness.key_press(Key::Enter);
        self.harness.run_steps(2);
    }

    fn draft_text(&self, thread: &str) -> String {
        let shared = self.shared.lock().unwrap();
        shared
            .drafts
            .get(thread)
            .map_or_else(String::new, |draft| draft.text.clone())
    }

    /// The commands the screen sent, after the workspace queries are answered.
    fn sent(&mut self) -> Vec<Command> {
        self.settle()
            .into_iter()
            .filter_map(|action| match action {
                Action::Command { command, .. } => Some(command),
                _ => None,
            })
            .collect()
    }

    fn sent_chat(&mut self) -> SendChat {
        match self.sent().as_slice() {
            [Command::Conversation(ConversationCommand::SendChat(chat))] => chat.clone(),
            other => panic!("one chat expected, got {other:?}"),
        }
    }

    /// Answers every workspace query but the skill lists, whose tags it collects
    /// unanswered, and returns them with the commands sent meanwhile. The window is
    /// not settled: a held list keeps it waiting.
    fn pump_holding_skills(&mut self) -> (Vec<(u64, ProjectId)>, Vec<Command>) {
        use workbench_protocol::namespaces::settings::{
            ProjectModelChoices, RoleModelChoice, SettingsQuery, SettingsReply,
        };
        let (mut held, mut commands) = (Vec::new(), Vec::new());
        for _ in 0..8 {
            self.harness.run_steps(1);
            while let Ok(action) = self.actions.try_recv() {
                let reply = match action {
                    Action::Command { command, .. } => {
                        commands.push(command);
                        continue;
                    }
                    Action::Feature(FeatureAction {
                        tag,
                        request:
                            FeatureRequest::Query(Query::Workspace(WorkspaceQuery::ListSkills(target))),
                        ..
                    }) => {
                        held.push((tag, target.project));
                        continue;
                    }
                    Action::Feature(FeatureAction {
                        feature: FeatureId::Workspace,
                        tag,
                        request: FeatureRequest::Query(Query::Workspace(query)),
                    }) => (tag, QueryReply::Workspace(answer(&query))),
                    Action::Feature(FeatureAction {
                        feature: FeatureId::Workspace,
                        tag,
                        request:
                            FeatureRequest::Query(Query::Settings(SettingsQuery::ModelChoices {
                                project,
                            })),
                    }) => {
                        let role = RoleModelChoice {
                            model: "fixture/default".into(),
                            reasoning_effort: None,
                        };
                        let catalog = ProjectModelChoices {
                            project,
                            profiles: Vec::new(),
                            worker: role.clone(),
                            orchestrator: role,
                        };
                        (
                            tag,
                            QueryReply::Settings(SettingsReply::ModelChoices(catalog)),
                        )
                    }
                    _ => continue,
                };
                self.shared.lock().unwrap().features.reply(
                    FeatureId::Workspace,
                    reply.0,
                    FeatureReply::Query(Ok(reply.1)),
                );
            }
        }
        (held, commands)
    }

    fn reply_skills(&mut self, tag: u64, result: Result<&[&str], &str>) {
        let reply = result
            .map(|names| {
                QueryReply::Workspace(WorkspaceReply::Skills(
                    names.iter().map(|name| summary(name)).collect(),
                ))
            })
            .map_err(FeatureError::new);
        self.shared.lock().unwrap().features.reply(
            FeatureId::Workspace,
            tag,
            FeatureReply::Query(reply),
        );
        self.harness.run_steps(2);
    }
}

/// A window whose project root on this machine holds `files`, with the skills the
/// fixture core lists (review and deploy).
fn project_holding(files: &[&str]) -> (tempfile::TempDir, Screen) {
    let root = tempfile::tempdir().unwrap();
    for file in files {
        let path = root.path().join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "").unwrap();
    }
    let path = root.path().to_string_lossy().into_owned();
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle_with(&move |_, reply| {
        if let WorkspaceReply::ProjectAccess(access) = reply {
            access.root.clone_from(&path);
        }
    });
    (root, screen)
}

fn plain_window() -> Screen {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    screen
}

// Given: the core lists review and deploy for the project, and the project has no
//        folder on this machine
// When: "@rev" is typed, then Tab
// Then: the matching skill is offered with its description and completed; the other is not
#[test]
fn at_mentions_offer_the_skills_the_core_lists_without_a_local_root() {
    let mut screen = plain_window();
    screen.write("run @rev");
    screen.harness.get_by_label("@review");
    assert!(screen.harness.query_by_label("@deploy").is_none());
    screen.capture("skill-mention");

    // Tab as the window delivers it: through the hook, which takes it from egui.
    let mut raw = egui::RawInput {
        events: vec![egui::Event::Key {
            key: Key::Tab,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }],
        ..egui::RawInput::default()
    };
    screen.harness.state_mut().raw_input_hook(&mut raw);
    screen.harness.run_steps(2);
    assert_eq!(screen.draft_text("thr-1"), "run @review ");
}

// Given: a project folder holding README.md, and the core's two skills
// When: "@" is typed and the file index lands
// Then: the files and the skills are offered together, whichever came first
#[test]
fn the_file_index_landing_late_keeps_the_skills() {
    let (_root, mut screen) = project_holding(&["README.md"]);
    screen.write("open @");
    screen.harness.get_by_label("@review");
    screen.harness.state_mut().conversation.wait_mentions();
    screen.harness.run_steps(2);
    for label in ["@README.md", "@review", "@deploy"] {
        screen.harness.get_by_label(label);
    }
}

// Given: the core's skills review and deploy
// When: a message mentions both with marks, one twice, a file and an unknown name
// Then: it is sent with the two skills once each in the order written, the text
//       untouched, and the draft kept until the core accepts it
#[test]
fn a_message_names_the_listed_skills_it_mentions() {
    let mut screen = plain_window();
    let text = "use @deploy, then @review! @deploy again, @src/lib.rs and @unknown";
    screen.write(text);
    screen.submit();
    let chat = screen.sent_chat();
    assert_eq!(chat.skills, ["deploy", "review"]);
    assert_eq!(chat.text, text);
    assert_eq!(screen.draft_text("thr-1"), text);
}

// Given: a draft that arrived by paste rather than by typing
// When: it is sent
// Then: its mentions are read the same way
#[test]
fn a_pasted_mention_is_named_like_a_typed_one() {
    let mut screen = plain_window();
    screen.harness.get_by_label("Message").focus();
    screen.harness.run_steps(2);
    screen
        .harness
        .input_mut()
        .events
        .push(egui::Event::Paste("see @review".into()));
    screen.harness.run_steps(2);
    screen.submit();
    assert_eq!(screen.sent_chat().skills, ["review"]);
}

// Given: a project folder with a file named like a skill, and no index of it yet
// When: "@review" is sent
// Then: the skill is named: the folder's files never decide it
#[test]
fn a_skill_name_beside_a_project_file_is_still_a_skill() {
    let (_root, mut screen) = project_holding(&["review/notes.md", "deploy"]);
    screen.write("@review and @deploy");
    screen.submit();
    assert_eq!(screen.sent_chat().skills, ["review", "deploy"]);
}

// Given: the list of the project has not arrived
// When: a message with a mention is sent, the list arrives, and it is sent again
// Then: the first send goes nowhere and keeps the draft with a notice; the second
//       names the skill. A message without a mention was never held back
#[test]
fn a_mention_waits_for_the_list_and_sends_once_it_has_arrived() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    let (held, _) = screen.pump_holding_skills();
    let [(tag, project)] = held.as_slice() else {
        panic!("one list asked for, got {held:?}");
    };
    assert_eq!(project.as_str(), "proj-1");

    screen.write("hello");
    screen.submit();
    let (_, sent) = screen.pump_holding_skills();
    let [Command::Conversation(ConversationCommand::SendChat(chat))] = sent.as_slice() else {
        panic!("one chat expected, got {sent:?}");
    };
    assert!(chat.skills.is_empty());
    screen.shared.lock().unwrap().sending.clear();
    screen.shared.lock().unwrap().drafts.clear();

    screen.write("@review please");
    screen.submit();
    let (_, sent) = screen.pump_holding_skills();
    assert!(sent.is_empty());
    assert_eq!(screen.draft_text("thr-1"), "@review please");
    screen.harness.get_by_label(WAITING);

    screen.reply_skills(*tag, Ok(&["review"]));
    screen.submit();
    let chat = screen.sent_chat();
    assert_eq!(
        (chat.text.as_str(), chat.skills.as_slice()),
        ("@review please", ["review".to_owned()].as_slice())
    );
}

// Given: the core cannot list the project's skills
// When: a message with a mention is sent
// Then: it goes without skills and says so
#[test]
fn a_failed_list_sends_without_skills_and_says_so() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    let (held, _) = screen.pump_holding_skills();
    screen.reply_skills(held[0].0, Err("the core cannot list skills"));
    screen.write("@review please");
    screen.submit();
    let chat = screen.sent_chat();
    assert!(chat.skills.is_empty());
    assert_eq!(chat.text, "@review please");
    screen.harness.get_by_label(UNLISTED);
}

// Given: the core's list held for the project
// When: the connection is replaced
// Then: the old list is not used: the mention waits and the list is asked for again
#[test]
fn a_new_connection_drops_the_list_and_asks_again() {
    let mut screen = plain_window();
    screen
        .shared
        .lock()
        .unwrap()
        .features
        .begin_session(&[], Vec::new());
    screen.write("@review");
    screen.submit();
    let (held, sent) = screen.pump_holding_skills();
    assert_eq!(held.len(), 1, "{held:?}");
    assert!(sent.is_empty());
    assert_eq!(screen.draft_text("thr-1"), "@review");
    screen.reply_skills(held[0].0, Ok(&["review"]));
    screen.submit();
    assert_eq!(screen.sent_chat().skills, ["review"]);
}

// Given: the window with the list held
// When: a mention starts, and again after the first was typed
// Then: the list is asked for once when the mention starts
#[test]
fn a_mention_starting_asks_for_the_skills_again() {
    let mut screen = plain_window();
    let asked = std::cell::Cell::new(0);
    let count = |query: &WorkspaceQuery, _: &mut WorkspaceReply| {
        if matches!(query, WorkspaceQuery::ListSkills(_)) {
            asked.set(asked.get() + 1);
        }
    };
    screen.write("see @");
    screen.settle_with(&count);
    assert_eq!(asked.get(), 1);
    screen.write("rev");
    screen.settle_with(&count);
    assert_eq!(asked.get(), 1);
}

// Given: each project's core list holds one skill of its own
// When: another project's thread is selected
// Then: only that project's skill is offered
#[test]
fn another_projects_skills_are_not_mixed_in() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    let own = |query: &WorkspaceQuery, reply: &mut WorkspaceReply| {
        if let (WorkspaceQuery::ListSkills(target), WorkspaceReply::Skills(list)) = (query, reply) {
            *list = vec![summary(&format!("only-{}", target.project.as_str()))];
        }
    };
    screen.settle_with(&own);
    screen.harness.get_by_label("Landing page copy").click();
    screen.settle_with(&own);
    screen.harness.run_steps(2);
    screen.settle_with(&own);
    screen.write("@only");
    screen.harness.get_by_label("@only-proj-2");
    assert!(screen.harness.query_by_label("@only-proj-1").is_none());
}
