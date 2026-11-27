//! The new-conversation screen: opened by the plus button without asking the core for
//! anything, one unsent draft per project that comes back, and the thread made only by its
//! first message.
use egui::Key;
use egui_kittest::kittest::Queryable;
use workbench_protocol::commands::Command;
use workbench_protocol::namespaces::ConversationCommand;

use super::*;

const HEADING: &str = "What should we do in evorch?";

impl Screen {
    fn open_new_conversation(&mut self, project: &str) -> Vec<Action> {
        self.harness
            .get_by_label(&format!("New thread in {project}"))
            .click();
        let actions = self.settle();
        self.harness.run_steps(2);
        actions
    }

    fn select_thread(&mut self, title: &str) {
        self.harness.get_by_label(title).click();
        self.settle();
        self.harness.run_steps(2);
    }

    fn type_in_composer(&mut self, text: &str) {
        self.harness.get_by_label("Message").focus();
        self.harness.run_steps(2);
        self.harness.get_by_label("Message").type_text(text);
        self.harness.run_steps(2);
    }

    fn new_thread_of(&self, project: &str) -> String {
        let shared = self.shared.lock().unwrap();
        shared.new_conversations[&ProjectId::new(project).unwrap()].to_string()
    }

    fn draft_of(&self, thread: &str) -> String {
        let shared = self.shared.lock().unwrap();
        shared
            .drafts
            .get(thread)
            .map_or_else(String::new, |draft| draft.text.clone())
    }
}

fn commands_of(actions: Vec<Action>) -> Vec<Command> {
    actions
        .into_iter()
        .filter_map(|action| match action {
            Action::Command { command, .. } => Some(command),
            _ => None,
        })
        .collect()
}

// Given: the plus button of evorch pressed, "draft A" typed, then website's plus pressed and
//        "draft B" typed, and an existing thread opened in between
// When: each project's plus is pressed again
// Then: each returns to its own draft in the same unsent conversation, and the core was
//       never asked for anything on the way
#[test]
fn the_plus_button_returns_to_each_projects_unsent_draft_and_asks_for_nothing() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    let mut asked = commands_of(screen.settle());
    asked.extend(commands_of(screen.open_new_conversation("evorch")));
    screen.harness.get_by_label(HEADING);
    let evorch = screen.new_thread_of("proj-1");
    screen.type_in_composer("draft A");

    asked.extend(commands_of(screen.open_new_conversation("website")));
    screen.harness.get_by_label("What should we do in website?");
    let website = screen.new_thread_of("proj-2");
    assert_ne!(evorch, website);
    screen.type_in_composer("draft B");

    screen.select_thread("Refactor storage writer");
    screen
        .harness
        .get_by_label("Thread: Refactor storage writer");
    assert!(screen.harness.query_by_label(HEADING).is_none());

    asked.extend(commands_of(screen.open_new_conversation("evorch")));
    screen.harness.get_by_label(HEADING);
    assert_eq!(screen.new_thread_of("proj-1"), evorch);
    asked.extend(commands_of(screen.open_new_conversation("evorch")));
    asked.extend(commands_of(screen.open_new_conversation("website")));
    screen.harness.get_by_label("What should we do in website?");
    assert_eq!(screen.new_thread_of("proj-2"), website);

    assert_eq!(screen.shared.lock().unwrap().new_conversations.len(), 2);
    assert_eq!(screen.draft_of(&evorch), "draft A");
    assert_eq!(screen.draft_of(&website), "draft B");
    assert!(asked.is_empty(), "the core was asked: {asked:?}");
}

// Given: a new conversation
// When: the window is large, and when it is only as tall as a few lines
// Then: the heading is above the composer, which is near the middle (a little below it) of
//       the large window, and in the small one the composer and Send stay inside the window
#[test]
fn the_composer_sits_below_the_heading_near_the_middle_and_stays_reachable() {
    for (size, small) in [
        (egui::vec2(1440.0, 900.0), false),
        (egui::vec2(900.0, 420.0), true),
    ] {
        let mut screen = screen(size);
        screen.settle();
        screen.open_new_conversation("evorch");
        screen.type_in_composer("line one");
        screen.type_in_composer("\nline two");
        screen.harness.run_steps(3);
        screen.capture(if small { "small" } else { "large" });
        let heading = screen.harness.get_by_label(HEADING).rect();
        let input = screen.harness.get_by_label("Message").rect();
        let send = screen.harness.get_by_label("Send").rect();
        assert!(heading.max.y < input.min.y, "{heading:?} {input:?}");
        if small {
            for rect in [heading, input, send] {
                assert!(rect.min.y >= 0.0 && rect.max.y <= size.y, "{rect:?}");
            }
        } else {
            let middle = input.center().y / size.y;
            assert!((0.45..=0.75).contains(&middle), "composer at {middle}");
            assert!(input.width() <= 720.0, "{input:?}");
        }
    }
}

// Given: a new conversation whose first message is sent
// When: the core's thread list already shows the thread, the person leaves for another
//       thread and comes back, and only then the core accepts the message
// Then: the thread list alone changes nothing and the draft's conversation stays new, even
//       after leaving and returning; what the core accepted becomes the thread's own screen
#[test]
fn the_new_conversation_becomes_the_thread_only_once_the_core_accepted_it() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    screen.open_new_conversation("evorch");
    let thread = screen.new_thread_of("proj-1");
    screen.type_in_composer("hello");
    screen.harness.key_press(Key::Enter);
    let sent = screen.settle();
    let sent = commands_of(sent);
    let [Command::Conversation(ConversationCommand::CreateExecution(chat))] = sent.as_slice()
    else {
        panic!("one creation expected");
    };
    assert_eq!(
        (chat.thread.as_str(), chat.text.as_str()),
        (&*thread, "hello")
    );
    {
        let mut shared = screen.shared.lock().unwrap();
        let mut summary = shared.cache.threads["thr-2"].clone();
        summary.id = ThreadId::new(thread.as_str()).unwrap();
        summary.title = "Hello thread".into();
        shared.cache.threads.insert(thread.clone(), summary);
        assert!(shared.sending.contains(&thread));
    }
    screen.harness.run_steps(2);
    screen.harness.get_by_label(HEADING);

    // The fixture core cannot answer for a thread it never had, so the frames are drawn
    // without answering the sidebar's questions about it.
    screen
        .harness
        .get_by_label("Refactor storage writer")
        .click();
    screen.harness.run_steps(3);
    screen
        .harness
        .get_by_label("Thread: Refactor storage writer");
    screen.harness.get_by_label("New thread in evorch").click();
    screen.harness.run_steps(3);
    screen.harness.get_by_label(HEADING);
    assert_eq!(screen.new_thread_of("proj-1"), thread);

    {
        let mut shared = screen.shared.lock().unwrap();
        shared.new_conversations.clear();
        shared.sending.remove(&thread);
    }
    screen.harness.run_steps(2);
    assert!(screen.harness.query_by_label(HEADING).is_none());
    screen.harness.get_by_label("Thread: Hello thread");
}

// Given: a new conversation
// When: a goal is the first message
// Then: the goal creates the thread, as it did before the new screen
#[test]
fn a_goal_as_the_first_message_creates_the_thread() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    screen.open_new_conversation("evorch");
    let thread = screen.new_thread_of("proj-1");
    screen.type_in_composer("/goal ship it");
    screen.harness.key_press(Key::Enter);
    let sent = commands_of(screen.settle());
    let [Command::Conversation(ConversationCommand::CreateGoal(goal))] = sent.as_slice() else {
        panic!("one goal creation expected: {sent:?}");
    };
    assert_eq!(
        (goal.thread.as_str(), goal.objective.as_str()),
        (&*thread, "ship it")
    );
}
