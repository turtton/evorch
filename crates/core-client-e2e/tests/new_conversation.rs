//! The desktop's new conversation against the production core composition: the plus button
//! creates nothing, the first message creates the thread and reaches the model, and the
//! thread's own screen takes over for the next one. Every wait is on a change the
//! connection reported or on a frame the screen asked for at once.
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use egui::Key;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use gui::core_client::{
    CoreClientScreen, Shared,
    connection::{Action, serve},
};
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
use workbench_protocol::commands::Command;
use workbench_protocol::namespaces::ConversationCommand;

/// Hands every request to the test, then answers at once.
struct Recorder(tokio::sync::mpsc::UnboundedSender<Vec<providers::Message>>);

#[async_trait]
impl runtime::AgentModel for Recorder {
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
        messages: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        self.0.send(messages.to_vec()).unwrap();
        Ok(providers::ChatResponse {
            usage_reported: true,
            message: providers::Message {
                role: providers::Role::Assistant,
                content: vec![providers::ContentBlock::Text { text: "ok".into() }],
            },
            usage: Default::default(),
            finish_reason: providers::FinishReason::Stop,
        })
    }
    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "recording-model".into()
    }
}

/// The runs' sandbox is not what is under test, and bwrap is not available everywhere.
struct Unisolated;

impl runtime::SandboxFactory for Unisolated {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}

/// One drawn frame lasts as a running app's, so a delayed repaint is not read as one asked
/// for at once.
const FRAME: f32 = 1.0 / 60.0;

struct Desktop {
    harness: Harness<'static>,
    shared: Arc<Mutex<Shared>>,
    actions: tokio::sync::mpsc::Sender<Action>,
    inbox: tokio::sync::mpsc::Receiver<Action>,
    /// What the screen asked the core to do, in order, as it left the screen.
    commands: Vec<Command>,
}

impl Desktop {
    fn new(shared: Arc<Mutex<Shared>>, actions: tokio::sync::mpsc::Sender<Action>) -> Self {
        let (outbox, inbox) = tokio::sync::mpsc::channel(64);
        let mut screen = CoreClientScreen::new(shared.clone(), outbox, None, None);
        let harness = Harness::builder()
            .with_size(egui::vec2(1280.0, 800.0))
            .with_step_dt(FRAME)
            .build_ui(move |ui| screen.ui(ui));
        Self {
            harness,
            shared,
            actions,
            inbox,
            commands: Vec::new(),
        }
    }

    async fn flush(&mut self) {
        self.harness.run_steps(2);
        while let Ok(action) = self.inbox.try_recv() {
            if let Action::Command { command, .. } = &action {
                self.commands.push(command.clone());
            }
            self.actions.send(action).await.unwrap();
        }
    }

    fn wants_frame(&self) -> bool {
        self.harness
            .output()
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .is_some_and(|viewport| viewport.repaint_delay.is_zero())
    }

    fn shows_part(&self, part: &str) -> bool {
        self.harness
            .query_all_by_label_contains(part)
            .next()
            .is_some()
    }

    /// Draws again on each change the connection reports, and at once while the screen asks
    /// for another frame, until `done`.
    async fn until(&mut self, done: impl Fn(&Desktop) -> bool) {
        let changed = self.shared.lock().unwrap().changed.clone();
        loop {
            let notification = changed.notified();
            tokio::pin!(notification);
            notification.as_mut().enable();
            self.flush().await;
            if done(self) {
                return;
            }
            if !self.wants_frame() {
                notification.await;
            }
        }
    }

    async fn type_message(&mut self, text: &str) {
        self.harness.get_by_label("Message").focus();
        self.harness.run_steps(2);
        self.harness.get_by_label("Message").type_text(text);
        self.flush().await;
        self.harness.key_press(Key::Enter);
        self.flush().await;
    }

    fn creations(&self) -> usize {
        self.commands
            .iter()
            .filter(|command| {
                matches!(
                    command,
                    Command::Conversation(ConversationCommand::CreateExecution(_))
                )
            })
            .count()
    }
}

fn user_text(messages: &[providers::Message]) -> Vec<&str> {
    messages
        .iter()
        .filter(|message| message.role == providers::Role::User)
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            providers::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

// Given: a desktop on a core with a project and no thread
// When: the plus button is pressed and a message typed, and then sent
// Then: nothing reaches the core before the send; the send creates the thread once, the
//       model is asked with the message, the thread's own screen replaces the new one, and
//       the next message is sent to that thread, not created again
#[tokio::test(flavor = "multi_thread")]
async fn the_first_message_creates_the_thread_and_the_next_one_reaches_it() {
    let dir = test_socket_dir::SplitDir::new();
    let root_dir = dir.path().canonicalize().unwrap();
    std::fs::write(
        root_dir.join("sidebar.json"),
        serde_json::json!({"version":workspace_state::SIDEBAR_SCHEMA_VERSION,"projects":[],"threads":[]}).to_string(),
    )
    .unwrap();
    let (requests, mut model) = tokio::sync::mpsc::unbounded_channel();
    let boot = BootstrappedHost::open_with_test_seams(
        BootstrapOptions {
            config_dir: root_dir.clone(),
            project_root: root_dir.clone(),
            ownership_root: dir.sockets().to_path_buf(),
        },
        TestSeams {
            model: Arc::new(Recorder(requests)),
            sandbox: Arc::new(Unisolated),
        },
    )
    .await
    .unwrap();
    let identity = core_instance::ConfigIdentity::new(&root_dir.join("gui-identity")).unwrap();
    let instance = evorch_core::instance::Instance::acquire(
        &core_instance::LaunchGuard::acquire(&identity).unwrap(),
        dir.sockets(),
    )
    .unwrap();
    let backend = Arc::new(evorch_core::backend::HostBackend::new(
        boot.host.clone(),
        instance.identity.clone(),
    ));
    let listener = instance.bind().unwrap();
    listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::UnixListener::from_std(listener).unwrap();
    let (stop, stopped) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let _keep = stop;
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (backend, stopped) = (backend.clone(), stopped.clone());
            tokio::spawn(async move {
                let _ = evorch_core::unix::connection(stream, backend, stopped).await;
            });
        }
    });
    let launcher = core_launcher::Launcher::with_socket(
        identity,
        core_launcher::CoreBinary::explicit(&std::env::current_exe().unwrap()).unwrap(),
        gui::core_client::client_config(),
        instance.socket.clone(),
    )
    .unwrap();
    let (actions, rx) = tokio::sync::mpsc::channel(64);
    let shared = Arc::new(Mutex::new(Shared::default()));
    tokio::spawn(serve(
        launcher,
        rx,
        shared.clone(),
        egui::Context::default(),
    ));

    let mut desktop = Desktop::new(shared.clone(), actions);
    desktop.until(|d| d.shows_part("New thread in")).await;
    let name = {
        let state = shared.lock().unwrap();
        assert!(state.cache.threads.is_empty());
        state
            .cache
            .projects
            .values()
            .next()
            .unwrap()
            .display_name
            .clone()
    };
    let heading = format!("What should we do in {name}?");

    desktop
        .harness
        .query_all_by_label_contains("New thread in")
        .next()
        .unwrap()
        .click();
    desktop.until(|d| d.shows_part(&heading)).await;
    desktop.harness.get_by_label("Message").focus();
    desktop.harness.run_steps(2);
    desktop
        .harness
        .get_by_label("Message")
        .type_text("first message");
    desktop.flush().await;
    assert!(
        desktop.commands.is_empty(),
        "opening and typing in a new conversation asked the core for something: {:?}",
        desktop.commands
    );
    assert!(shared.lock().unwrap().cache.threads.is_empty());

    desktop.harness.key_press(Key::Enter);
    desktop.until(|d| d.creations() == 1).await;
    let thread = desktop
        .commands
        .iter()
        .find_map(|command| match command {
            Command::Conversation(ConversationCommand::CreateExecution(chat)) => {
                Some(chat.thread.clone())
            }
            _ => None,
        })
        .unwrap();
    let first = model.recv().await.unwrap();
    assert!(
        user_text(&first)
            .iter()
            .any(|t| t.contains("first message"))
    );

    let id = thread.to_string();
    desktop
        .until(|d| {
            let state = d.shared.lock().unwrap();
            state.new_conversations.is_empty()
                && !state.sending.contains(&id)
                && state.cache.threads.contains_key(&id)
                && state
                    .drafts
                    .get(&id)
                    .is_none_or(|draft| draft.text.is_empty())
        })
        .await;
    desktop.flush().await;
    assert!(!desktop.shows_part(&heading));
    assert!(desktop.shows_part("Thread: "));
    assert_eq!(desktop.creations(), 1);

    desktop.type_message("second message").await;
    desktop
        .until(|d| {
            d.commands.iter().any(|command| {
                matches!(
                    command,
                    Command::Conversation(ConversationCommand::SendChat(chat))
                        if chat.thread == thread
                )
            })
        })
        .await;
    let second = model.recv().await.unwrap();
    assert!(
        user_text(&second)
            .iter()
            .any(|t| t.contains("second message"))
    );
    assert_eq!(
        second[..first.len()],
        first[..],
        "the first turn is the prefix"
    );
    assert_eq!(desktop.creations(), 1);
}
