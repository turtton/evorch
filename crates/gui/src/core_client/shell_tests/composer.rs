//! The composer's slash commands, `@` file mentions, keys and attachments, as the
//! embedded workbench's composer offers them.
use std::sync::mpsc;

use egui::Key;
use egui_kittest::kittest::{NodeT, Queryable};
use workbench_protocol::commands::{AttachmentRef, Command, ComposerRole, Delegation};
use workbench_protocol::ids::AttachmentId;
use workbench_protocol::namespaces::{ConversationCommand, ExecutionCommand};

use super::*;
use crate::attachments::{AttachmentFile, ClipboardBackend, ClipboardRgba};
use crate::core_client::ClientAttachmentDraft;

const HELP: &str = "/team <delegation value> | <goal> — Submit a team goal with explicit delegation value\n\
/goal <text> — Track an objective in this conversation\n\
/help — Show available commands\n\
/new — Start a new thread\n\
/compaction — Request context compaction at the next turn boundary\n\
/continue — Resume the current conversation after a stop or error";

fn composer() -> Screen {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    screen
}

impl Screen {
    fn type_message(&mut self, text: &str) {
        self.harness.get_by_label("Message").focus();
        self.harness.run_steps(2);
        self.harness.get_by_label("Message").type_text(text);
        self.harness.run_steps(2);
    }

    /// Tab as the window delivers it: through the hook, which must take it from egui.
    fn tab(&mut self) {
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
        self.harness.state_mut().raw_input_hook(&mut raw);
        assert!(raw.events.is_empty());
        self.harness.run_steps(2);
    }

    fn press(&mut self, key: Key) {
        self.harness.key_press(key);
        self.harness.run_steps(2);
    }

    fn draft(&self, thread: &str) -> String {
        let shared = self.shared.lock().unwrap();
        shared
            .drafts
            .get(thread)
            .map_or_else(String::new, |draft| draft.text.clone())
    }

    /// What the actor does when the core accepts what `thread` sent.
    fn accept(&mut self, thread: &str) {
        let mut shared = self.shared.lock().unwrap();
        shared.sending.remove(thread);
        shared.drafts.remove(thread);
        drop(shared);
        self.harness.run_steps(1);
    }

    fn commands(&mut self) -> Vec<Command> {
        self.settle()
            .into_iter()
            .filter_map(|action| match action {
                Action::Command { command, .. } => Some(command),
                _ => None,
            })
            .collect()
    }
}

fn file(id: &str, ready: bool, uploading: bool) -> ClientAttachmentDraft {
    let mut draft = ClientAttachmentDraft::new(
        ProjectId::new("proj-1").unwrap(),
        AttachmentFile {
            draft_id: id.into(),
            filename: format!("{id}.txt"),
            media_type: "text/plain".into(),
            bytes: b"hello".to_vec(),
        },
    );
    draft.uploading = uploading;
    draft.upload_id = Some(AttachmentId::new(format!("up-{id}")).unwrap());
    draft.reference = ready.then(|| AttachmentRef {
        id: AttachmentId::new(format!("up-{id}")).unwrap(),
        filename: format!("{id}.txt"),
        media_type: "text/plain".into(),
        byte_length: 5,
        sha256: String::new(),
    });
    draft
}

// Given: the composer of thr-1
// When: "/" is typed
// Then: the commands the core client carries out are offered, and /btw and /run,
//       which it cannot, are not
#[test]
fn a_slash_offers_the_commands_the_core_client_carries_out() {
    let mut screen = composer();
    screen.type_message("/");
    for offered in [
        "/team <delegation value> | <goal>",
        "/goal <text>",
        "/help",
        "/new",
        "/compaction",
        "/continue",
    ] {
        screen.harness.get_by_label(offered);
    }
    for absent in ["/btw <question>", "/run <text>"] {
        assert!(screen.harness.query_by_label(absent).is_none(), "{absent}");
    }
    screen.capture("slash");
}

// Given: "/" typed in the focused composer
// When: Tab reaches the window, then Enter is pressed
// Then: the hook takes the Tab from egui, it moves the highlight to /goal, and
//       Enter accepts it
#[test]
fn tab_moves_the_highlight_and_enter_accepts_it() {
    let mut screen = composer();
    screen.type_message("/");
    screen.tab();

    screen.press(Key::Enter);

    assert_eq!(screen.draft("thr-1"), "/goal ");
    assert!(screen.commands().is_empty());
}

// Given: "/go" typed
// When: Enter is pressed, and then Enter on the finished "/new"
// Then: the first completes the command; the second runs it, opening a new
//       conversation in the project with thr-1's draft cleared
#[test]
fn enter_completes_a_command_and_runs_a_finished_one() {
    let mut screen = composer();
    screen.type_message("/go");
    screen.press(Key::Enter);
    assert_eq!(screen.draft("thr-1"), "/goal ");

    screen.shared.lock().unwrap().drafts.clear();
    screen.harness.run_steps(1);
    screen.type_message("/new");
    screen.press(Key::Enter);
    screen.harness.run_steps(2);

    let state = screen.harness.state();
    assert_eq!(
        state.project.as_ref().map(ProjectId::as_str),
        Some("proj-1")
    );
    let selected = state.selected.clone().expect("a new conversation");
    assert_ne!(selected.as_str(), "thr-1");
    assert_eq!(screen.draft("thr-1"), "");
    screen.harness.get_by_label("What should we do in evorch?");
}

// Given: thr-1 with a file ready to send
// When: "/goal ship it" is sent
// Then: the goal is submitted to the core's thread tied to the draft, which is
//       kept until the core accepts it; the file waits for the next message
#[test]
fn goal_submits_to_the_thread_and_keeps_the_pending_files() {
    let mut screen = composer();
    screen
        .shared
        .lock()
        .unwrap()
        .attachment_drafts
        .insert("thr-1".into(), vec![file("a", true, false)]);
    screen.type_message("/goal ship it");
    screen.press(Key::Enter);

    let actions = screen.settle();
    let [
        Action::Command {
            command: Command::Execution(ExecutionCommand::SubmitGoal(goal)),
            draft: Some((thread, _)),
        },
    ] = actions.as_slice()
    else {
        panic!("one goal, got {} actions", actions.len());
    };
    assert_eq!(
        (
            thread.as_str(),
            goal.thread.as_str(),
            goal.objective.as_str()
        ),
        ("thr-1", "thr-1", "ship it")
    );
    assert_eq!(goal.delegation, None::<Delegation>);
    assert_eq!(screen.draft("thr-1"), "/goal ship it");
    assert_eq!(
        screen.shared.lock().unwrap().attachment_drafts["thr-1"].len(),
        1
    );
}

// Given: the composer of thr-1
// When: "/team review | ship it" is sent, and then "/compaction"
// Then: a team goal with that delegation value is submitted, then the thread's
//       compaction is requested
#[test]
fn team_and_compaction_reach_the_core() {
    let mut screen = composer();
    screen.type_message("/team review | ship it");
    screen.press(Key::Enter);
    let commands = screen.commands();
    let [Command::Execution(ExecutionCommand::SubmitGoal(goal))] = commands.as_slice() else {
        panic!("one goal: {commands:?}");
    };
    assert_eq!(goal.objective, "ship it");
    assert_eq!(
        goal.delegation,
        Some(Delegation::Team {
            value: "review".into()
        })
    );
    screen.accept("thr-1");

    screen.type_message("/compaction");
    screen.press(Key::Enter);
    let commands = screen.commands();
    assert!(
        matches!(
            commands.as_slice(),
            [Command::Conversation(ConversationCommand::CompactChat(target))]
                if target.thread.as_str() == "thr-1"
        ),
        "{commands:?}"
    );
}

// Given: thr-1, with the composer switched to the Orchestrator role
// When: "/continue now" is sent, then "/continue", then "/continue" in a new
//       conversation the core does not have yet
// Then: the arguments get the usage and keep the draft; the bare command
//       continues thr-1 as the composer's role; the new conversation has no
//       thread to continue
#[test]
fn continue_resumes_the_thread_as_the_composers_role() {
    let mut screen = composer();
    screen.harness.get_by_label("Role: Worker").click();
    screen.harness.run_steps(2);
    screen.type_message("/continue now");
    screen.press(Key::Enter);
    screen.harness.get_by_label("usage: /continue");
    assert_eq!(screen.draft("thr-1"), "/continue now");
    assert!(screen.commands().is_empty());

    screen.shared.lock().unwrap().drafts.clear();
    screen.harness.run_steps(1);
    screen.type_message("/continue");
    screen.press(Key::Enter);
    let commands = screen.commands();
    let [Command::Conversation(ConversationCommand::ContinueChat(chat))] = commands.as_slice()
    else {
        panic!("one continuation: {commands:?}");
    };
    assert_eq!(
        (chat.project.as_str(), chat.thread.as_str()),
        ("proj-1", "thr-1")
    );
    assert_eq!(chat.composer_role, ComposerRole::Orchestrator);
    assert_eq!(chat.model_preference, None);
    screen.accept("thr-1");

    screen.type_message("/new");
    screen.press(Key::Enter);
    screen.harness.run_steps(2);
    screen.harness.get_by_label("What should we do in evorch?");
    screen.type_message("/continue");
    screen.press(Key::Enter);
    screen
        .harness
        .get_by_label("Select or start a thread first");
    assert!(screen.commands().is_empty());
}

// Given: thr-1 with a file ready to send
// When: plain text is typed and Enter pressed
// Then: it is sent as a chat message carrying the file, tied to the draft
#[test]
fn plain_text_is_sent_as_a_chat_message() {
    let mut screen = composer();
    screen
        .shared
        .lock()
        .unwrap()
        .attachment_drafts
        .insert("thr-1".into(), vec![file("a", true, false)]);
    screen.type_message("hello there");
    screen.press(Key::Enter);

    let actions = screen.settle();
    let [
        Action::Command {
            command: Command::Conversation(ConversationCommand::SendChat(chat)),
            draft: Some((thread, _)),
        },
    ] = actions.as_slice()
    else {
        panic!("one chat, got {} actions", actions.len());
    };
    assert_eq!(
        (thread.as_str(), chat.text.as_str()),
        ("thr-1", "hello there")
    );
    let attachments: Vec<_> = chat.attachments.iter().map(AttachmentId::as_str).collect();
    assert_eq!(attachments, ["up-a"]);
}

// Given: the composer of thr-1
// When: "/team bad" is sent, and then "/help"
// Then: the usage shows in the transcript and the draft is kept to correct; /help
//       lists the offered commands and clears the draft; nothing reaches the core
#[test]
fn usage_and_help_show_in_the_transcript_without_sending() {
    let mut screen = composer();
    screen.type_message("/team bad");
    screen.press(Key::Enter);
    screen
        .harness
        .get_by_label("usage: /team <delegation value> | <goal>");
    assert_eq!(screen.draft("thr-1"), "/team bad");

    screen.shared.lock().unwrap().drafts.clear();
    screen.harness.run_steps(1);
    screen.type_message("/help");
    screen.press(Key::Enter);
    screen.harness.get_by_label(HELP);
    assert_eq!(screen.draft("thr-1"), "");
    assert!(screen.commands().is_empty());
    screen.capture("help");
}

// Given: thr-1, whose run waits on the person, with "/" typed
// When: Esc is pressed, then pressed again
// Then: while the completions are open the composer offers neither Stop nor Queue;
//       the first Esc hides them without stopping and brings both back; the second
//       stops the run
#[test]
fn esc_hides_the_completions_before_it_stops_the_run() {
    let mut screen = composer();
    let stops = |screen: &Screen| screen.harness.query_all_by_label("Stop").count();
    screen.type_message("/");
    screen.harness.get_by_label("/help");
    let header_stops = stops(&screen);
    assert!(screen.harness.query_by_label("Queue").is_none());

    screen.press(Key::Escape);
    screen.harness.run_steps(2);
    assert!(screen.harness.query_by_label("/help").is_none());
    assert!(screen.commands().is_empty());
    assert_eq!(stops(&screen), header_stops + 1);
    screen.harness.get_by_label("Queue");

    screen.press(Key::Escape);
    let commands = screen.commands();
    assert!(
        matches!(
            commands.as_slice(),
            [Command::Execution(ExecutionCommand::StopChat(target))] if target.thread.as_str() == "thr-1"
        ),
        "{commands:?}"
    );
}

// Given: "/" typed in thr-1 and its completions hidden with Esc
// When: /new opens a new conversation and "/" is typed there
// Then: the new conversation offers the commands again
#[test]
fn esc_hides_the_completions_of_one_thread_only() {
    let mut screen = composer();
    screen.type_message("/");
    screen.press(Key::Escape);
    assert!(screen.harness.query_by_label("/help").is_none());

    screen.shared.lock().unwrap().drafts.clear();
    screen.harness.run_steps(1);
    screen.type_message("/new");
    screen.press(Key::Enter);
    screen.harness.run_steps(2);
    screen.harness.get_by_label("What should we do in evorch?");

    screen.type_message("/");
    screen.harness.get_by_label("/help");
}

// Given: proj-1's root on this machine holds README.md and src/lib.rs
// When: "open @" is typed, then ArrowDown and Enter, then Enter
// Then: the files and folders are offered, the folder is entered, and the file is
//       completed
#[test]
fn at_mentions_complete_the_projects_files_and_folders() {
    let (_root, mut screen) = project_files();
    screen.type_message("open @");
    screen.harness.state_mut().conversation.wait_mentions();
    screen.harness.run_steps(2);
    screen.harness.get_by_label("@README.md");
    screen.harness.get_by_label("@src/");
    screen.capture("mention");

    screen.press(Key::ArrowDown);
    screen.press(Key::Enter);
    assert_eq!(screen.draft("thr-1"), "open @src/");
    screen.harness.get_by_label("@src/lib.rs");

    screen.press(Key::Enter);
    assert_eq!(screen.draft("thr-1"), "open @src/lib.rs ");
    assert!(screen.harness.query_by_label("@src/lib.rs").is_none());
}

/// A composer whose project root on this machine holds README.md and src/lib.rs.
fn project_files() -> (tempfile::TempDir, Screen) {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/lib.rs"), "").unwrap();
    std::fs::write(root.path().join("README.md"), "").unwrap();
    let path = root.path().to_string_lossy().into_owned();
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle_with(&move |_, reply| {
        if let WorkspaceReply::ProjectAccess(access) = reply {
            access.root.clone_from(&path);
        }
    });
    (root, screen)
}

// Given: proj-1's files on this machine and "open @READ" typed
// When: Tab arrives through the hook; then "@" is typed and Esc pressed
// Then: Tab accepts the only candidate; Esc hides the list and keeps the text
#[test]
fn tab_accepts_a_single_mention_and_esc_hides_the_list() {
    let (_root, mut screen) = project_files();
    screen.type_message("open @READ");
    screen.harness.state_mut().conversation.wait_mentions();
    screen.harness.run_steps(2);
    screen.harness.get_by_label("@README.md");
    screen.tab();
    assert_eq!(screen.draft("thr-1"), "open @README.md ");

    screen.harness.get_by_label("Message").type_text("@");
    screen.harness.run_steps(2);
    screen.harness.get_by_label("@src/");
    screen.press(Key::Escape);
    assert!(screen.harness.query_by_label("@src/").is_none());
    assert_eq!(screen.draft("thr-1"), "open @README.md @");
    assert!(screen.commands().is_empty());
}

/// Hands out a clipboard image only once released.
struct HeldClipboard(mpsc::Receiver<()>);

impl ClipboardBackend for HeldClipboard {
    fn image(&mut self) -> Result<ClipboardRgba, String> {
        self.0.recv().map_err(|_| "released".to_owned())?;
        Ok(ClipboardRgba {
            width: 1,
            height: 1,
            bytes: vec![0, 0, 0, 255],
        })
    }
}

// Given: the composer focused on thr-1 and a clipboard holding an image
// When: Ctrl+V arrives as the platform's empty paste
// Then: the paste is not text, and the image is being prepared for thr-1
#[test]
fn an_image_paste_prepares_an_attachment() {
    let mut screen = composer();
    let (release, held) = mpsc::channel();
    screen
        .harness
        .state_mut()
        .conversation
        .attachment_io
        .set_clipboard_backend(Box::new(HeldClipboard(held)));
    screen.type_message("see");
    let mut raw = egui::RawInput {
        events: vec![egui::Event::Paste(String::new())],
        ..egui::RawInput::default()
    };
    screen.harness.state_mut().raw_input_hook(&mut raw);
    assert!(raw.events.is_empty());
    screen.harness.run_steps(2);

    assert_eq!(
        screen
            .shared
            .lock()
            .unwrap()
            .loading_attachments
            .get("thr-1"),
        Some(&1)
    );
    screen.harness.get_by_label("Preparing 1 attachment(s)…");
    assert_eq!(screen.draft("thr-1"), "see");
    drop(release);
}

// Given: thr-1 with an uploaded file, one whose upload awaits its reply and one
//        the core did not take
// When: Remove is pressed on the uploaded one
// Then: each row shows its state, the uploading one cannot be removed, and the
//       removed one is gone with its upload discarded in the core
#[test]
fn remove_drops_one_file_and_discards_its_upload() {
    let mut screen = composer();
    screen.shared.lock().unwrap().attachment_drafts.insert(
        "thr-1".into(),
        vec![
            file("ready", true, false),
            file("busy", false, true),
            file("failed", false, false),
        ],
    );
    screen.harness.run_steps(2);
    screen.harness.get_by_label("ready.txt");
    screen.harness.get_by_label("Ready");
    screen.harness.get_by_label("Uploading…");
    screen.harness.get_by_label("Upload failed");
    screen.harness.get_by_label("Retry upload");
    screen.capture("images");
    let removes: Vec<_> = screen.harness.get_all_by_label("Remove").collect();
    assert_eq!(removes.len(), 3);
    assert!(removes[1].accesskit_node().is_disabled());
    assert!(!removes[2].accesskit_node().is_disabled());
    removes[0].click();
    screen.harness.run_steps(2);

    let commands = screen.commands();
    assert!(
        matches!(
            commands.as_slice(),
            [Command::Conversation(ConversationCommand::DiscardAttachment { attachment_id })]
                if attachment_id.as_str() == "up-ready"
        ),
        "{commands:?}"
    );
    let shared = screen.shared.lock().unwrap();
    let left: Vec<_> = shared.attachment_drafts["thr-1"]
        .iter()
        .map(|file| file.id.as_str())
        .collect();
    assert_eq!(left, ["busy", "failed"]);
}

// Given: a narrow window, thr-2 with one file whose chip is drawn, and thr-7 (idle as
//        well, so its composer is laid out as thr-2's) with three files never drawn, one
//        of them long-named; both with the same one-line draft
// When: thr-7 is selected
// Then: in the first frame that draws thr-7's composer, its chips already wrap inside it
//       and Send is where it stays, instead of being pushed out of the pane for a frame
#[test]
fn attachment_chips_wrap_inside_a_narrow_composer_from_the_first_frame() {
    let mut screen = screen(egui::vec2(900.0, 600.0));
    screen.settle();
    screen
        .harness
        .get_by_label("Refactor storage writer")
        .click();
    screen.settle();
    {
        let mut shared = screen.shared.lock().unwrap();
        // A one-line draft, so the input is as tall at any width and only the chips
        // could change the composer's layout.
        for thread in ["thr-2", "thr-7"] {
            shared.drafts.insert(
                thread.into(),
                crate::core_client::Draft {
                    text: "see".into(),
                    revision: 1,
                },
            );
        }
        shared
            .attachment_drafts
            .insert("thr-2".into(), vec![file("one", true, false)]);
        shared.attachment_drafts.insert(
            "thr-7".into(),
            vec![
                file("a-long-screenshot-name-from-the-clipboard", true, false),
                file("two", true, false),
                file("three", true, false),
            ],
        );
    }
    screen.harness.run_steps(2);
    screen.harness.get_by_label("one.txt");
    screen
        .harness
        .get_by_label("Migrate the lane table")
        .click();
    // Its first chip marks the first frame that draws thr-7's composer, whose Send ends
    // the input row (with or without its icon in the name, as it can send or not).
    let send = |screen: &Screen| {
        screen
            .harness
            .get_all_by_label_contains("Send")
            .map(|node| node.rect().max.x)
            .fold(f32::MIN, f32::max)
    };
    let mut first = None;
    for _ in 0..16 {
        screen.harness.run_steps(1);
        if screen.harness.query_by_label("two.txt").is_some() {
            first = Some(send(&screen));
            break;
        }
    }
    let first = first.expect("thr-7's composer is drawn");
    let removes: Vec<_> = screen
        .harness
        .get_all_by_label("Remove")
        .map(|node| node.rect())
        .collect();
    assert_eq!(removes.len(), 3);
    assert!(
        removes.iter().all(|r| r.max.x <= first),
        "{removes:?} {first}"
    );
    assert!(removes[2].min.y > removes[0].max.y, "{removes:?}");

    screen.settle();
    screen.harness.run_steps(4);
    screen.capture("narrow-chips");
    let settled = send(&screen);
    assert!((first - settled).abs() < 0.5, "{first} {settled}");
}

#[derive(Debug)]
struct DroppedPath(std::path::PathBuf);

impl egui::DroppedFile for DroppedPath {
    fn path(&self) -> &std::path::Path {
        &self.0
    }

    fn bytes(&self) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }
}

// Given: "/" typed in the focused composer
// When: Browser comes to the front of the conversation's leaf, then Tab and a dropped
//       file reach the window, and the conversation comes back
// Then: the hidden composer leaves both to the window, and no Tab is replayed into it
#[test]
fn a_hidden_composer_leaves_tab_and_dropped_files_to_the_window() {
    let mut screen = composer();
    screen.type_message("/");
    shell::focus(
        &mut screen.harness.state_mut().dock,
        CoreTab::Feature(FeatureId::Browser),
    );
    screen.harness.run_steps(2);

    let mut raw = egui::RawInput {
        events: vec![egui::Event::Key {
            key: Key::Tab,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }],
        dropped_files: vec![Arc::new(DroppedPath("/tmp/a.png".into()))],
        ..egui::RawInput::default()
    };
    screen.harness.state_mut().raw_input_hook(&mut raw);
    assert_eq!(raw.events.len(), 1);
    assert_eq!(raw.dropped_files.len(), 1);

    shell::focus(&mut screen.harness.state_mut().dock, CoreTab::Conversation);
    screen.harness.run_steps(2);
    screen.harness.get_by_label("Message").focus();
    screen.harness.run_steps(2);
    screen.press(Key::Enter);
    assert_eq!(screen.draft("thr-1"), "/team ");
    assert!(
        screen
            .shared
            .lock()
            .unwrap()
            .loading_attachments
            .get("thr-1")
            .is_none_or(|count| *count == 0)
    );
}

// Given: the composer of thr-1
// When: the attach "+" gets keyboard focus
// Then: its tooltip says files can also be pasted or dropped, without hovering
#[test]
fn the_attach_button_mentions_paste_and_drop_on_focus() {
    let mut screen = composer();
    screen.harness.get_by_label("Attach files").focus();
    screen.harness.run_steps(2);
    screen.harness.get_by_label_contains("paste or drop");
    screen.capture("attach-focus");
}

// Given: thr-1's message being sent, with the composer focused
// When: Ctrl+V arrives as the platform's empty paste
// Then: nothing is prepared, the person is told to wait for the send, and the attach
//       "+" waits too
#[test]
fn a_paste_while_sending_is_refused_with_a_status() {
    let mut screen = composer();
    screen.type_message("see");
    screen.shared.lock().unwrap().sending.insert("thr-1".into());
    screen.harness.run_steps(2);
    assert!(
        screen
            .harness
            .get_by_label("Attach files")
            .accesskit_node()
            .is_disabled()
    );

    let mut raw = egui::RawInput {
        events: vec![egui::Event::Paste(String::new())],
        ..egui::RawInput::default()
    };
    screen.harness.state_mut().raw_input_hook(&mut raw);
    screen.harness.run_steps(2);

    let shared = screen.shared.lock().unwrap();
    assert!(
        shared
            .loading_attachments
            .get("thr-1")
            .is_none_or(|count| *count == 0)
    );
    assert_eq!(shared.status, crate::core_client::WAIT_FOR_SEND_TO_ATTACH);
}

// Given: thr-1 with a file ready to send, and a client queue that is closed
// When: "/goal ship it" is sent
// Then: nothing is sent, the draft and the file are kept, and the person is told so
#[test]
fn a_refused_command_keeps_its_draft_and_the_pending_files() {
    let mut screen = composer();
    screen
        .shared
        .lock()
        .unwrap()
        .attachment_drafts
        .insert("thr-1".into(), vec![file("a", true, false)]);
    screen.actions.close();
    screen.type_message("/goal ship it");
    screen.press(Key::Enter);

    assert!(screen.commands().is_empty());
    assert_eq!(screen.draft("thr-1"), "/goal ship it");
    let shared = screen.shared.lock().unwrap();
    assert!(!shared.sending.contains("thr-1"));
    assert_eq!(shared.attachment_drafts["thr-1"].len(), 1);
    assert_eq!(shared.status, crate::core_client::QUEUE_FULL);
}

// Given: proj-1's files on this machine and "see @READ" typed, so @README.md is offered
// When: Shift+Enter is pressed
// Then: it reaches the editor: a newline is inserted and the mention is left as typed
#[test]
fn shift_enter_with_the_list_open_inserts_a_newline() {
    let (_root, mut screen) = project_files();
    screen.type_message("see @READ");
    screen.harness.state_mut().conversation.wait_mentions();
    screen.harness.run_steps(2);
    screen.harness.get_by_label("@README.md");

    screen
        .harness
        .key_press_modifiers(egui::Modifiers::SHIFT, Key::Enter);
    screen.harness.run_steps(2);
    assert_eq!(screen.draft("thr-1"), "see @READ\n");
    assert!(screen.commands().is_empty());
}
