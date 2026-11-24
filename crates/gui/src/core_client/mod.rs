//! Opt-in conversation-only desktop. This module does not open a database,
//! construct a runtime, read a sidebar, or own an agent lifecycle.
pub mod attachments;
mod attention;
pub mod auto_reconnect;
pub mod cache;
pub mod restart;
use crate::attachments::AttachmentFile;
pub use attachments::ClientAttachmentDraft;
pub mod connection;
pub mod conversation;
pub mod features;
pub mod local;
pub mod notifications;
pub mod persistence;
pub mod run_tabs;
pub mod shell;
mod subagent_panes;
mod transcript;
use features::{FeatureFrame, FeatureView, Navigation, Selection};

use connection::Action;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use workbench_client::ClientConfig;
use workbench_protocol::namespaces::ConversationCommand;
use workbench_protocol::{
    commands::*,
    handshake::{BuildInfo, Capability},
    ids::*,
};
use workspace_ui::KeyAction;

#[derive(Default)]
pub struct Draft {
    pub text: String,
    pub revision: u64,
}

#[derive(Default)]
pub struct Shared {
    pub cache: cache::Cache,
    pub drafts: BTreeMap<String, Draft>,
    pub attachment_drafts: BTreeMap<String, Vec<ClientAttachmentDraft>>,
    pub sending: std::collections::BTreeSet<String>,
    pub loading_attachments: BTreeMap<String, usize>,
    pub outcome_unknown: std::collections::BTreeSet<String>,
    pub connected: bool,
    pub status: String,
    pub changed: Arc<tokio::sync::Notify>,
    /// What the connected core offers; features are shown only for what it does.
    pub capabilities: Vec<Capability>,
    /// The per-area features, fed by the shared connection (`features/`).
    pub features: features::Features,
    /// Counts the projections [`Self::replace_projection`] installed, so what was
    /// asked of an earlier one (message bodies) is asked of the new one again.
    pub projection: u64,
}

impl Shared {
    pub fn take_attachment_drafts(&mut self, thread: &str) -> Option<Vec<ClientAttachmentDraft>> {
        if !self.can_discard_attachments(thread) {
            return None;
        }
        self.attachment_drafts.remove(thread)
    }
    pub fn can_discard_attachments(&self, thread: &str) -> bool {
        can_discard_attachments(
            &self.sending,
            &self.loading_attachments,
            &self.attachment_drafts,
            thread,
        )
    }

    pub fn can_send(&self, thread: &str) -> bool {
        can_send(
            &self.sending,
            &self.loading_attachments,
            &self.attachment_drafts,
            thread,
        )
    }

    /// A control-session transition, not a render-time observation. Call once so
    /// a collapsed/minimized feature cannot retain core-derived bodies or textures.
    fn disconnect(&mut self) {
        if self.connected {
            self.connected = false;
            self.features.disconnected();
        }
    }

    fn attachment_draft(
        &mut self,
        project: ProjectId,
        thread: &str,
        file: AttachmentFile,
    ) -> &mut ClientAttachmentDraft {
        let drafts = self.attachment_drafts.entry(thread.to_owned()).or_default();
        let position = drafts
            .iter()
            .position(|draft| draft.id == file.draft_id)
            .unwrap_or_else(|| {
                drafts.push(ClientAttachmentDraft::new(project, file));
                drafts.len() - 1
            });
        &mut drafts[position]
    }

    /// A snapshot cut arrived (connect, reconnect or resync): replace the
    /// projection, then restart the features on it. Features see their reset
    /// first, then the events that arrived during the cut, then the `held`
    /// replies (resolved outcomes of commands whose reply was lost), so a reset
    /// never discards a resolved reply.
    pub fn begin_session(
        &mut self,
        cache: cache::Cache,
        capabilities: Vec<Capability>,
        delivered: &[workbench_protocol::events::UiEvent],
        held: Vec<features::HeldReply>,
    ) {
        self.replace_projection(cache);
        self.capabilities = capabilities;
        self.features.begin_session(delivered, held);
        self.connected = true;
        self.status = "core-client: 接続済み".into();
    }

    /// Replace a disposable cut without discarding human input or uncertainty.
    pub fn replace_projection(&mut self, cache: cache::Cache) {
        if self.cache.cursor().is_some()
            && self.cache.cursor().map(|c| c.core_epoch) != cache.cursor().map(|c| c.core_epoch)
        {
            for image in self.attachment_drafts.values_mut().flatten() {
                if !image.uncertain {
                    image.reference = None;
                    image.upload_id = None;
                    image.next_offset = 0;
                    image.uploading = false;
                }
            }
        }
        self.cache = cache;
        self.projection += 1;
    }
}

fn can_discard_attachments(
    sending: &std::collections::BTreeSet<String>,
    loading: &BTreeMap<String, usize>,
    drafts: &BTreeMap<String, Vec<ClientAttachmentDraft>>,
    thread: &str,
) -> bool {
    !sending.contains(thread)
        && loading.get(thread).copied().unwrap_or(0) == 0
        && !drafts
            .get(thread)
            .into_iter()
            .flatten()
            .any(|i| i.uploading)
}

fn can_send(
    sending: &std::collections::BTreeSet<String>,
    loading: &BTreeMap<String, usize>,
    drafts: &BTreeMap<String, Vec<ClientAttachmentDraft>>,
    thread: &str,
) -> bool {
    !sending.contains(thread)
        && loading.get(thread).copied().unwrap_or(0) == 0
        && !drafts
            .get(thread)
            .into_iter()
            .flatten()
            .any(|i| i.reference.is_none() || i.uploading)
}

pub fn client_config() -> ClientConfig {
    let mut config = ClientConfig::new(
        ClientId::new(format!("desktop-{}", uuid::Uuid::new_v4())).expect("uuid fits"),
        BuildInfo {
            name: "evorch-gui".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            revision: None,
        },
    );
    config.required_capabilities = vec![
        Capability::Commands,
        Capability::Queries,
        Capability::Conversation,
        Capability::SnapshotEvents,
        Capability::InputReceipts,
        Capability::Questions,
        Capability::Approvals,
        Capability::CommittedBodies,
    ];
    config
}

pub fn run(config_dir: PathBuf, title: String) -> Result<(), String> {
    let identity = core_instance::ConfigIdentity::new(&config_dir)
        .map_err(|e| format!("configuration directory {}: {e}", config_dir.display()))?;
    // The launcher starts only the evorch-core installed beside this executable. Without
    // one there is nothing to connect to or start, and no other runtime takes its place.
    let binary = core_launcher::CoreBinary::sibling().map_err(|e| {
        format!(
            "evorch-core is not available beside evorch-gui ({e}); \
             evorch-gui does not run its own runtime in its place"
        )
    })?;
    let launcher = core_launcher::Launcher::new(identity, binary, client_config())
        .map_err(|e| e.to_string())?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let options = crate::window::native_options(&title);
    let settings = ui_settings(&config_dir);
    let keybinds = crate::keymap::configured_panel_keybinds(&settings.keybinds, &config_dir);
    eframe::run_native(
        &title,
        options,
        Box::new(move |cc| {
            crate::theme::style::install_preset(&cc.egui_ctx, settings.theme_preset.into());
            let shared = Arc::new(Mutex::new(Shared::default()));
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            let task = runtime.spawn(connection::serve(
                launcher,
                rx,
                shared.clone(),
                cc.egui_ctx.clone(),
            ));
            let local = local::LocalPanes::new(
                Arc::new(crate::diff::GitCliDiffSource),
                Some(Arc::new(crate::pty::ShellSpawner::from_env())),
            );
            Ok(Box::new(CoreClientApp {
                screen: CoreClientScreen::new(shared, tx, None, None)
                    .with_local_panes(local)
                    .with_keymap(core_keymap(&keybinds))
                    .with_state_store(persistence::ClientStateStore::in_config_dir(&config_dir)),
                _runtime: runtime,
                task,
            }))
        }),
    )
    .map_err(|e| e.to_string())
}

/// The workbench's UI settings (theme and keymap), shared with the embedded one and only
/// read here: a file that does not load gives the defaults, as in the embedded workbench.
fn ui_settings(config_dir: &std::path::Path) -> workspace_ui::UiSettings {
    let path = config_dir.join("ui.toml");
    if !path.exists() {
        return workspace_ui::UiSettings::default();
    }
    workspace_ui::load_settings(&path).unwrap_or_else(|error| {
        tracing::warn!(%error, "UI settings load failed; using Graphite defaults");
        workspace_ui::UiSettings::default()
    })
}

/// The embedded workbench's keymap without the composer role switch, which the core
/// client has no use for: a chord bound to it is left to the terminal and the widgets.
/// `settings` are those of `ui.toml` with `config.toml`'s `[panel.keybinds]` over them,
/// as the embedded workbench merges them.
fn core_keymap(settings: &workspace_ui::KeybindSettings) -> crate::keymap::Keymap {
    let mut settings = settings.clone();
    settings
        .bindings
        .remove(&workspace_ui::KeyAction::CycleAgentRole);
    crate::keymap::Keymap::from_settings(&settings)
}

pub struct CoreClientApp {
    screen: CoreClientScreen,
    _runtime: tokio::runtime::Runtime,
    task: tokio::task::JoinHandle<()>,
}

/// How many run tabs ask for their history in one frame.
const RUN_READS_PER_FRAME: usize = 8;

/// What the window draws, apart from the runtime that serves the connection: the features, the
/// selector and the conversation controls, over the shared projection. It is the whole page,
/// so a test can draw exactly what the window draws.
pub struct CoreClientScreen {
    shared: Arc<Mutex<Shared>>,
    actions: tokio::sync::mpsc::Sender<Action>,
    selected: Option<ThreadId>,
    project: Option<ProjectId>,
    conversation: conversation::ConversationPane,
    dock: egui_dock::DockState<shell::CoreTab>,
    notifications: notifications::NotificationLog,
    run_tabs: run_tabs::RunTabs,
    subagents: subagent_panes::SubagentWatch,
    attention: attention::TabAttention,
    local: local::LocalPanes,
    keymap: crate::keymap::Keymap,
    store: Option<persistence::ClientStateStore>,
    window_rects: persistence::WindowRects,
    /// The file's state, applied when the first frame is drawn.
    restore: Option<persistence::SavedState>,
    /// The file's selection, applied once the first snapshot is in.
    pending_selection: Option<persistence::SavedSelection>,
    /// Whether the restored run tabs still wait for the first snapshot, which drops
    /// those of threads the core no longer has.
    pending_run_tab_prune: bool,
}

impl CoreClientScreen {
    pub fn new(
        shared: Arc<Mutex<Shared>>,
        actions: tokio::sync::mpsc::Sender<Action>,
        project: Option<ProjectId>,
        selected: Option<ThreadId>,
    ) -> Self {
        Self {
            shared,
            actions,
            selected,
            project,
            conversation: conversation::ConversationPane::default(),
            dock: shell::default_dock(),
            notifications: notifications::NotificationLog::default(),
            run_tabs: run_tabs::RunTabs::default(),
            subagents: subagent_panes::SubagentWatch::default(),
            attention: attention::TabAttention::default(),
            local: local::LocalPanes::default(),
            keymap: core_keymap(&workspace_ui::KeybindSettings::default()),
            store: None,
            window_rects: persistence::WindowRects::default(),
            restore: None,
            pending_selection: None,
            pending_run_tab_prune: false,
        }
    }

    /// The chords from the UI settings; without this, the default ones.
    #[must_use]
    pub fn with_keymap(mut self, keymap: crate::keymap::Keymap) -> Self {
        self.keymap = keymap;
        self
    }

    /// The session file the layout and the selection are restored from and saved to;
    /// without one nothing is read or written.
    #[must_use]
    pub fn with_state_store(mut self, store: persistence::ClientStateStore) -> Self {
        self.restore = store.load();
        self.store = Some(store);
        self
    }

    /// Saves the layout, the run tabs and the selection to the session file, if any, as
    /// the window closes. A file this build could not read is left as it is.
    pub fn save_state(&self) {
        let shared = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.save_with(&shared, false);
    }

    /// `explicit` for the save chord, which also replaces a file this build could not read.
    fn save_with(&self, shared: &Shared, explicit: bool) {
        // Before the first frame the file was not applied, and stays as it is.
        let Some(store) = self.store.as_ref().filter(|_| self.restore.is_none()) else {
            return;
        };
        let state = persistence::SavedState {
            layout: persistence::extract(&self.dock, &self.local.files, &self.window_rects),
            run_tabs: self.run_tabs.panes(),
            selection: self.saved_selection(shared),
        };
        let saved = if explicit {
            store.save(&state).map(|()| true)
        } else {
            store.autosave(&state)
        };
        match saved {
            Ok(true) => {}
            Ok(false) => tracing::info!(
                path = %store.path().display(),
                "kept the layout file this build could not read; save the layout to replace it"
            ),
            Err(error) => {
                tracing::warn!(path = %store.path().display(), %error, "failed to save layout");
            }
        }
    }

    /// The restored selection until it is applied; then the selected project, with its
    /// thread only when the core has it (a new conversation is not kept).
    fn saved_selection(&self, shared: &Shared) -> Option<persistence::SavedSelection> {
        if let Some(pending) = &self.pending_selection {
            return Some(pending.clone());
        }
        let project = self.project.clone()?;
        let thread = self.selected.clone().filter(|thread| {
            shared
                .cache
                .threads
                .get(thread.as_str())
                .is_some_and(|summary| summary.project == project)
        });
        Some(persistence::SavedSelection { project, thread })
    }

    fn follow_subagents(
        subagents: &mut subagent_panes::SubagentWatch,
        run_tabs: &mut run_tabs::RunTabs,
        dock: &mut egui_dock::DockState<shell::CoreTab>,
        selected: Option<&ThreadId>,
        shared: &mut Shared,
    ) {
        for change in subagents.observe(&shared.cache.runs) {
            run_tabs.apply(dock, &change, selected);
        }
        let Some(execution) = shared.features.execution_mut() else {
            return;
        };
        let unnamed: Vec<_> = run_tabs
            .shown_tabs()
            .filter(|(_, tab)| tab.name.is_none())
            .map(|(id, tab)| (id, tab.thread.clone(), tab.run.clone()))
            .collect();
        for (id, thread, run) in unnamed {
            match execution.run_name(&run) {
                Some(name) => run_tabs.set_name(id, name),
                None => execution.want_name(thread, run),
            }
        }
    }

    fn apply_restore(&mut self, state: persistence::SavedState) {
        self.dock = persistence::restore_dock(state.layout.as_ref(), &mut self.local.files);
        self.window_rects = persistence::WindowRects::default();
        self.pending_run_tab_prune = !state.run_tabs.is_empty();
        self.run_tabs.restore_hidden(state.run_tabs);
        self.pending_selection = state.selection;
    }

    /// The Diff's source and the Terminal's shell; without this the Diff reads git and
    /// the Terminal starts no shell.
    #[must_use]
    pub fn with_local_panes(mut self, local: local::LocalPanes) -> Self {
        self.local = local;
        self
    }

    /// Before egui reads the input of a frame: a focused terminal takes the keys.
    pub fn raw_input_hook(&mut self, raw_input: &mut egui::RawInput) {
        let keymap = &self.keymap;
        self.local.capture_input(raw_input, |key, modifiers| {
            keymap.passes_through_terminal(key, modifiers)
        });
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let results = self.conversation.attachment_io.poll().collect::<Vec<_>>();
        for result in results {
            let Some((project, thread)) = self
                .conversation
                .attachment_imports
                .remove(&result.request_id)
            else {
                continue;
            };
            let mut shared = self
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(count) = shared.loading_attachments.get_mut(thread.as_str()) {
                *count = count.saturating_sub(1);
            }
            match result.files {
                Ok(files) => {
                    for file in files {
                        enqueue(
                            &self.actions,
                            &mut shared,
                            Action::Upload {
                                project: project.clone(),
                                thread: thread.clone(),
                                file,
                            },
                        );
                    }
                }
                Err(error) => shared.status = error,
            }
        }
        self.draw(ui);
    }
}

impl Drop for CoreClientApp {
    fn drop(&mut self) {
        let actions = self.screen.actions.clone();
        // Give the actor a bounded best-effort chance to discard known unused
        // attachments. This is resource cleanup, never an agent stop command.
        self._runtime.block_on(async {
            let _ = tokio::time::timeout(std::time::Duration::from_millis(500), async {
                let _ = actions.send(Action::Shutdown).await;
                let _ = (&mut self.task).await;
            })
            .await;
        });
        self.task.abort();
    }
}

/// Apply what the features asked for in a frame: send their requests and make
/// the selection changes they named, as a click on the built-in selector would.
fn apply_frame(
    tx: &tokio::sync::mpsc::Sender<Action>,
    shared: &mut Shared,
    project: &mut Option<ProjectId>,
    selected: &mut Option<ThreadId>,
    frame: FeatureFrame,
) {
    for action in frame.data {
        enqueue(tx, shared, Action::Data(action));
    }
    for target in frame.observations {
        shared.features.observe_run(target);
    }
    for artifact in frame.previews {
        if shared.features.is_shown(
            features::FeatureId::Browser,
            shared.connected,
            &shared.capabilities,
        ) {
            shared.features.open_preview(artifact);
        } else {
            shared.status = "このコアはプレビューを提供していないため、開けません".into();
        }
    }
    for action in frame.actions {
        enqueue(tx, shared, Action::Feature(action));
    }
    for intent in frame.lanes {
        // A confirmation chosen from a thread's menu is drawn by the Lanes tab.
        if matches!(intent, features::lanes::LaneIntent::Close { .. }) {
            shared.features.reveal(features::FeatureId::Lanes);
        }
        if let Some(lanes) = shared.features.lanes_mut() {
            lanes.enqueue_sidebar(intent);
        }
    }
    for intent in frame.pull_requests {
        // The "Link PR…" dialog chosen from a thread's menu is drawn by the Pull requests tab.
        if matches!(
            intent,
            features::pull_requests::PullRequestIntent::OpenLink(_)
        ) {
            shared.features.reveal(features::FeatureId::PullRequests);
        }
        if let Some(pull_requests) = shared.features.pull_requests_mut() {
            pull_requests.enqueue_sidebar(intent);
        }
    }
    for navigation in frame.navigation {
        apply_navigation(tx, shared, project, selected, navigation);
    }
}

/// Change the selection as a click on the built-in selector would.
fn apply_navigation(
    tx: &tokio::sync::mpsc::Sender<Action>,
    shared: &mut Shared,
    project: &mut Option<ProjectId>,
    selected: &mut Option<ThreadId>,
    navigation: Navigation,
) {
    match navigation {
        Navigation::SelectProject(id) => {
            *project = Some(id);
            *selected = None;
        }
        Navigation::SelectThread {
            project: id,
            thread,
        } => {
            *project = Some(id);
            *selected = Some(thread.clone());
            enqueue(tx, shared, Action::History(thread, None, false));
        }
        Navigation::NewConversation(id) => {
            *project = Some(id);
            *selected = Some(ThreadId::new(uuid::Uuid::new_v4().to_string()).unwrap());
        }
    }
}

/// Once connected with nothing selected, select what the session file kept: its thread
/// when the core still has it in that project, else its project, else nothing. A
/// selection made first wins, and the kept one is dropped.
fn restore_selection(
    tx: &tokio::sync::mpsc::Sender<Action>,
    shared: &mut Shared,
    project: &mut Option<ProjectId>,
    selected: &mut Option<ThreadId>,
    pending: &mut Option<persistence::SavedSelection>,
) {
    if project.is_some() || selected.is_some() {
        *pending = None;
        return;
    }
    if !shared.connected {
        return;
    }
    let Some(saved) = pending.take() else {
        return;
    };
    let thread = saved.thread.filter(|thread| {
        shared
            .cache
            .threads
            .get(thread.as_str())
            .is_some_and(|summary| summary.project == saved.project)
    });
    let navigation = match thread {
        Some(thread) => Navigation::SelectThread {
            project: saved.project,
            thread,
        },
        None if shared.cache.projects.contains_key(saved.project.as_str()) => {
            Navigation::SelectProject(saved.project)
        }
        None => return,
    };
    apply_navigation(tx, shared, project, selected, navigation);
}

/// Register local input before making it visible to the asynchronous actor.
pub fn enqueue(tx: &tokio::sync::mpsc::Sender<Action>, shared: &mut Shared, action: Action) {
    match &action {
        Action::Command {
            draft: Some((thread, _)),
            ..
        } => {
            if !shared.can_send(thread) {
                return;
            }
            shared.sending.insert(thread.clone());
        }
        Action::Upload {
            project,
            thread,
            file,
            ..
        } => {
            if shared.sending.contains(thread.as_str()) {
                return;
            }
            let drafts = shared.attachment_drafts.get(thread.as_str());
            let fresh =
                drafts.is_none_or(|drafts| drafts.iter().all(|draft| draft.id != file.draft_id));
            if file.bytes.len() > workbench_protocol::conversation::ATTACHMENT_FILE_BYTES as usize
                || (fresh
                    && drafts.is_some_and(|drafts| {
                        drafts.len() >= 32
                            || drafts
                                .iter()
                                .map(|draft| draft.file.bytes.len())
                                .sum::<usize>()
                                + file.bytes.len()
                                > 64 * 1024 * 1024
                    }))
            {
                shared.status = "添付の上限は 8 MiB/件、32件・64 MiB/thread です".into();
                return;
            }
            let image = shared.attachment_draft(project.clone(), thread.as_str(), file.clone());
            if image.uploading || image.reference.is_some() {
                return;
            }
            image.uploading = true;
        }
        _ => (),
    }
    if let Err(error) = tx.try_send(action) {
        match error.into_inner() {
            Action::Data(action) => {
                if shared.features.data.current(&action) {
                    shared.features.data_notice(
                        action.feature,
                        action.tag,
                        features::data::DataNotice::Failed(features::FeatureError::new(
                            "client queue is full or closed",
                        )),
                    );
                }
            }
            Action::Feature(action) => {
                shared
                    .features
                    .refuse(&action, "client queue is full or closed");
            }
            Action::Command {
                draft: Some((thread, _)),
                ..
            } => {
                shared.sending.remove(&thread);
            }
            Action::Upload {
                project,
                thread,
                file,
                ..
            } => {
                shared
                    .attachment_draft(project, thread.as_str(), file)
                    .uploading = false;
            }
            // The run's first page was not asked for: the next frame asks again.
            Action::History(_, Some(run), false) => shared.cache.forget_run(run.as_str()),
            _ => (),
        }
        shared.status =
            "クライアントの送信キューが満杯または切断中です。入力は保持されています。".into();
    }
}

impl eframe::App for CoreClientApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.screen.logic(ctx);
    }

    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        self.screen.raw_input_hook(raw_input);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.screen.ui(ui);
    }

    /// As the window closes, before the connection is shut down.
    fn on_exit(&mut self) {
        self.screen.save_state();
    }
}

impl CoreClientScreen {
    /// eframe calls logic without any UI pass while the native window is minimized.
    /// Keep the two maintenance paths disjoint, including a minimized root whose
    /// visible child viewport causes eframe to call both logic and ui.
    fn logic(&mut self, ctx: &egui::Context) {
        if !ctx.input(|input| input.viewport().minimized.unwrap_or(false)) {
            return;
        }
        let mut shared = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let selection = Selection {
            project: self.project.clone(),
            thread: self.selected.clone(),
        };
        let s = &mut *shared;
        let view = FeatureView {
            connected: s.connected,
            capabilities: &s.capabilities,
            cache: &s.cache,
            selection: &selection,
        };
        let frame = s.features.maintain_minimized(view);
        // The log diffs the cache once per pass, so it sees what changes while minimized.
        self.notifications.observe(&s.cache);
        apply_frame(
            &self.actions,
            &mut shared,
            &mut self.project,
            &mut self.selected,
            frame,
        );
    }

    fn draw(&mut self, ui: &mut egui::Ui) {
        if let Some(state) = self.restore.take() {
            self.apply_restore(state);
        }
        let minimized = ui.input(|input| input.viewport().minimized.unwrap_or(false));
        let mut effects = Vec::new();
        let mut shared = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // The snapshot is in the cache before `connected` is set, and the selection is
        // read below, so the features see it in this frame.
        restore_selection(
            &self.actions,
            &mut shared,
            &mut self.project,
            &mut self.selected,
            &mut self.pending_selection,
        );
        if self.pending_run_tab_prune && shared.connected {
            self.pending_run_tab_prune = false;
            let threads = &shared.cache.threads;
            self.run_tabs
                .prune_hidden(|thread| threads.contains_key(thread.as_str()));
        }
        if shared.connected {
            Self::follow_subagents(
                &mut self.subagents,
                &mut self.run_tabs,
                &mut self.dock,
                self.selected.as_ref(),
                &mut shared,
            );
        }
        for feature in shared.features.take_focus() {
            shell::focus(&mut self.dock, shell::CoreTab::Feature(feature));
        }
        self.conversation.follow_projection(shared.projection);
        let titles = shared.features.titles();
        let placeholders = shared.features.placeholders();
        let unsupported = shared.features.unsupported_notes();
        egui::Panel::bottom("core-footer")
            .resizable(false)
            .frame(egui::Frame::NONE.inner_margin(egui::vec2(6.0, 3.0)))
            .show(ui, |ui| {
                shell::footer(
                    ui,
                    shell::FooterState {
                        status: &shared.status,
                        connected: shared.connected,
                        outcome_unknown: &shared.outcome_unknown,
                        unsupported: &unsupported,
                        titles: &titles,
                        placeholders: &placeholders,
                    },
                    &mut effects,
                );
            });
        let mut save_now = false;
        match ui.input(|input| self.keymap.action_for_input(input)) {
            Some(KeyAction::FocusAgentPane) => {
                effects.push(shell::Effect::Open(shell::CoreTab::Conversation));
            }
            // The Terminal starts folded, so its chord also opens it.
            Some(KeyAction::FocusTerminalPane) => {
                effects.push(shell::Effect::Open(shell::CoreTab::Terminal));
            }
            Some(KeyAction::ResetLayout) => effects.push(shell::Effect::ResetLayout),
            Some(KeyAction::SaveLayout) => save_now = true,
            Some(KeyAction::CycleAgentRole) | None => {}
        }
        let selection = Selection {
            project: self.project.clone(),
            thread: self.selected.clone(),
        };
        self.notifications.observe(&shared.cache);
        let roots = local::LocalRoots::resolve(
            &selection,
            shared
                .features
                .workspace()
                .map(|workspace| workspace.model()),
            &shared.cache,
        );
        // Shells are ended against the projects the sidebar lists, and only once a
        // listing completed: the snapshot's projects miss those added since.
        let projects = shared
            .connected
            .then(|| {
                shared
                    .features
                    .workspace()
                    .and_then(|workspace| workspace.model().projects.as_deref())
            })
            .flatten();
        self.local.begin_frame(ui.ctx(), roots, projects);
        let link_base = self.local.workspace().map(std::path::Path::to_path_buf);
        let frame = {
            let s = &mut *shared;
            let view = FeatureView {
                connected: s.connected,
                capabilities: &s.capabilities,
                cache: &s.cache,
                selection: &selection,
            };
            let mut pass = s.features.begin_dock_pass(minimized, view);
            let root = shell::selected_root(&pass, &selection);
            let thread = selection.thread.as_ref();
            let current = thread
                .and_then(|thread| conversation::current_root(&s.cache, thread, root.as_ref()));
            self.attention
                .observe(thread, current, &s.cache, &self.run_tabs);
            let mut viewer = shell::Viewer {
                pass: &mut pass,
                titles: &titles,
                conversation: &mut self.conversation,
                io: conversation::ConversationIo {
                    cache: &s.cache,
                    connected: s.connected,
                    drafts: &mut s.drafts,
                    attachment_drafts: &s.attachment_drafts,
                    sending: &s.sending,
                    loading_attachments: &s.loading_attachments,
                    link_base: link_base.as_deref(),
                },
                selection: &selection,
                effects: &mut effects,
                notifications: &mut self.notifications,
                run_tabs: &self.run_tabs,
                local: &mut self.local,
                attention: &mut self.attention,
            };
            egui_dock::DockArea::new(&mut self.dock)
                .style(crate::theme::dock::dock_style(ui.style()))
                .show_leaf_close_all_buttons(false)
                .show_inside(ui, &mut viewer);
            pass.finish(ui)
        };
        self.window_rects.track(&self.dock, ui.ctx());
        if !frame.lanes.is_empty() || !frame.pull_requests.is_empty() {
            // What a lane menu chose is sent in the lanes feature's next pass.
            ui.ctx().request_repaint();
        }
        apply_frame(
            &self.actions,
            &mut shared,
            &mut self.project,
            &mut self.selected,
            frame,
        );
        if effects.iter().any(|effect| {
            matches!(
                effect,
                shell::Effect::Goal(_) | shell::Effect::Lane(_) | shell::Effect::PullRequest(_)
            )
        }) {
            // The goal or lane command leaves in its feature's next pass.
            ui.ctx().request_repaint();
        }
        for effect in effects {
            apply_effect(
                &self.actions,
                &mut self.dock,
                &mut self.conversation,
                &mut self.run_tabs,
                &mut shared,
                effect,
            );
        }
        // Closing a file tab or a reset layout drops its preview.
        self.local.sweep_files(ui.ctx(), &self.dock);
        // Closing a tab, a reset layout or another thread drops its run's history.
        let runs = &shared.cache.runs;
        let phase_of = |run: &RunId| runs.get(run.as_str()).map(|run| run.phase);
        for run in self
            .run_tabs
            .sweep(&mut self.dock, self.selected.as_ref(), phase_of)
        {
            shared.cache.forget_run(run.as_str());
        }
        if save_now {
            self.save_with(&shared, true);
        }
        if shared.connected {
            // A tab drawn reads its run's history, and so does one whose history a new
            // session's projection (a reconnect or resync) lacks. A window opened on
            // many subagents asks a few per frame, so the client's queue keeps room for
            // everything else.
            let mut reads = 0;
            for (thread, run) in self.run_tabs.drawn(&self.dock) {
                if reads == RUN_READS_PER_FRAME {
                    ui.ctx().request_repaint();
                    break;
                }
                if shared.cache.want_run(run.as_str()) {
                    let action = Action::History(thread.clone(), Some(run.clone()), false);
                    enqueue(&self.actions, &mut shared, action);
                    reads += 1;
                }
            }
            if let Some(thread) = &self.selected
                && shared.cache.read_older_for_roots(thread.as_str())
            {
                let action = Action::History(thread.clone(), None, true);
                enqueue(&self.actions, &mut shared, action);
            }
            if shared
                .features
                .execution_mut()
                .is_some_and(|execution| execution.names_waiting())
            {
                ui.ctx().request_repaint();
            }
        }
        if shared.features.focus_pending() {
            ui.ctx().request_repaint();
        }
    }
}

fn apply_effect(
    actions: &tokio::sync::mpsc::Sender<Action>,
    dock: &mut egui_dock::DockState<shell::CoreTab>,
    conversation: &mut conversation::ConversationPane,
    run_tabs: &mut run_tabs::RunTabs,
    shared: &mut Shared,
    effect: shell::Effect,
) {
    {
        match effect {
            shell::Effect::Send(action) => enqueue(actions, shared, *action),
            shell::Effect::Reveal(feature) => shared.features.reveal(feature),
            shell::Effect::Open(shell::CoreTab::Plan) => shell::focus_beside(
                dock,
                shell::CoreTab::Plan,
                shell::CoreTab::Feature(features::FeatureId::Execution),
            ),
            shell::Effect::Open(tab @ (shell::CoreTab::Diff | shell::CoreTab::Terminal)) => {
                shell::open_local(dock, tab);
            }
            shell::Effect::Open(tab @ shell::CoreTab::File(_)) => shell::focus_beside(
                dock,
                tab,
                shell::CoreTab::Feature(features::FeatureId::Execution),
            ),
            shell::Effect::Open(tab) => shell::focus(dock, tab),
            // The frame's end reads the run's history once the tab is in the dock.
            shell::Effect::OpenRun(pane) => shell::focus_beside(
                dock,
                shell::CoreTab::Run(run_tabs.open(pane)),
                shell::CoreTab::Feature(features::FeatureId::Execution),
            ),
            shell::Effect::Goal(action) => {
                if let Some(execution) = shared.features.execution_mut() {
                    execution.enqueue_goal(action);
                }
            }
            shell::Effect::Lane(action) => {
                if let Some(lanes) = shared.features.lanes_mut() {
                    lanes.enqueue_card(action);
                }
            }
            // Moving auto fix from another thread is confirmed in the Pull requests tab.
            shell::Effect::PullRequest(action) => {
                if shared
                    .features
                    .pull_requests_mut()
                    .is_some_and(|pull_requests| pull_requests.enqueue_bar(action))
                {
                    shared.features.reveal(features::FeatureId::PullRequests);
                }
            }
            // The run tabs of other threads, out of the dock, go with the layout too.
            shell::Effect::ResetLayout => {
                *dock = shell::default_dock();
                for run in run_tabs.clear() {
                    shared.cache.forget_run(run.as_str());
                }
            }
            shell::Effect::Status(status) => shared.status = status,
            shell::Effect::AttachRequested {
                request,
                project,
                thread,
            } => {
                *shared
                    .loading_attachments
                    .entry(thread.to_string())
                    .or_default() += 1;
                conversation
                    .attachment_imports
                    .insert(request, (project, thread));
            }
            shell::Effect::RetryAttachments { project, thread } => {
                let pending: Vec<_> = shared
                    .attachment_drafts
                    .get(thread.as_str())
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|file| file.reference.is_none() && !file.uploading && !file.uncertain)
                    .collect();
                for draft in pending {
                    enqueue(
                        actions,
                        shared,
                        Action::Upload {
                            project: project.clone(),
                            thread: thread.clone(),
                            file: draft.file,
                        },
                    );
                }
            }
            shell::Effect::DiscardAttachments(thread) => {
                for file in shared
                    .take_attachment_drafts(thread.as_str())
                    .into_iter()
                    .flatten()
                {
                    if let Some(id) = file.upload_id {
                        enqueue(
                            actions,
                            shared,
                            Action::Command {
                                command: Command::Conversation(
                                    ConversationCommand::DiscardAttachment { attachment_id: id },
                                ),
                                draft: None,
                            },
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod shell_tests;

#[cfg(test)]
mod upload_regressions {
    use super::*;
    use workbench_protocol::namespaces::ExecutionCommand;

    fn upload() -> Action {
        Action::Upload {
            project: ProjectId::new("p").unwrap(),
            thread: ThreadId::new("t").unwrap(),
            file: crate::attachments::AttachmentFile {
                bytes: b"local bytes".to_vec(),
                ..crate::fixture::attachment_draft().file
            },
        }
    }
    fn send() -> Action {
        Action::Command {
            command: Command::Execution(ExecutionCommand::StopChat(ThreadTarget {
                thread: ThreadId::new("t").unwrap(),
            })),
            draft: Some(("t".into(), 0)),
        }
    }
    #[test]
    fn attach_then_send_before_actor_runs_keeps_image_and_blocks_send() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut state = Shared::default();
        enqueue(&tx, &mut state, upload());
        assert!(
            state
                .attachment_drafts
                .get("t")
                .is_some_and(|i| i.len() == 1),
            "enqueue must register the image synchronously"
        );
        enqueue(&tx, &mut state, send());
        assert!(!state.sending.contains("t"));
        assert!(matches!(rx.try_recv(), Ok(Action::Upload { .. })));
        assert!(rx.try_recv().is_err(), "send must not overtake upload");
    }
    #[test]
    fn full_queue_keeps_local_image_for_explicit_retry() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        tx.try_send(Action::Reconnect).ok().unwrap();
        let mut state = Shared::default();
        enqueue(&tx, &mut state, upload());
        assert_eq!(state.attachment_drafts["t"][0].file.bytes, b"local bytes");
        assert!(state.attachment_drafts["t"][0].reference.is_none());
    }
    #[tokio::test]
    async fn discard_while_upload_reply_is_held_keeps_draft() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut state = Shared::default();
        enqueue(&tx, &mut state, upload());
        let Action::Upload { .. } = rx.recv().await.unwrap() else {
            panic!()
        };
        let (release, held) = tokio::sync::oneshot::channel();
        let reply = tokio::spawn(async move {
            held.await.unwrap();
        });
        assert!(
            state.take_attachment_drafts("t").is_none(),
            "discard must be refused until upload has a known outcome"
        );
        release.send(()).unwrap();
        reply.await.unwrap();
        assert_eq!(state.attachment_drafts["t"].len(), 1);
    }
}

#[cfg(test)]
mod navigation_tests {
    use super::*;

    fn ids() -> (ProjectId, ThreadId) {
        (ProjectId::new("p").unwrap(), ThreadId::new("t").unwrap())
    }

    #[test]
    fn navigation_changes_the_selection_like_the_built_in_selector() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut shared = Shared::default();
        let (id, thread) = ids();
        let (mut project, mut selected) = (None, None);
        let frame = |navigation| FeatureFrame {
            data: Vec::new(),
            previews: Vec::new(),
            observations: Vec::new(),
            lanes: Vec::new(),
            pull_requests: Vec::new(),
            actions: vec![],
            navigation: vec![navigation],
        };

        apply_frame(
            &tx,
            &mut shared,
            &mut project,
            &mut selected,
            frame(Navigation::SelectThread {
                project: id.clone(),
                thread: thread.clone(),
            }),
        );
        assert_eq!(
            (project.as_ref(), selected.as_ref()),
            (Some(&id), Some(&thread))
        );
        // Selecting a thread loads its history, as the built-in selector does.
        assert!(matches!(rx.try_recv(), Ok(Action::History(t, None, false)) if t == thread));

        apply_frame(
            &tx,
            &mut shared,
            &mut project,
            &mut selected,
            frame(Navigation::SelectProject(id.clone())),
        );
        assert_eq!((project.as_ref(), selected.as_ref()), (Some(&id), None));

        apply_frame(
            &tx,
            &mut shared,
            &mut project,
            &mut selected,
            frame(Navigation::NewConversation(id.clone())),
        );
        assert_eq!(project.as_ref(), Some(&id));
        assert!(selected.is_some() && selected != Some(thread));
        assert!(
            rx.try_recv().is_err(),
            "only selecting a thread sends anything"
        );
    }

    // Given: a run tab wants run-a, and the client queue is full
    // When: run-a's first page is enqueued
    // Then: run-a is not kept as asked for, so the next frame asks again
    #[test]
    fn a_run_history_the_queue_refuses_is_asked_for_again() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let mut shared = Shared::default();
        let (_, thread) = ids();
        enqueue(
            &tx,
            &mut shared,
            Action::History(thread.clone(), None, false),
        );
        assert!(shared.cache.want_run("run-a"));
        let run = RunId::new("run-a").unwrap();
        enqueue(&tx, &mut shared, Action::History(thread, Some(run), false));
        assert!(shared.cache.want_run("run-a"));
    }
}

#[cfg(test)]
mod keymap_tests {
    use egui::{Key, Modifiers};

    use super::*;

    fn action(keymap: &crate::keymap::Keymap, key: Key, modifiers: Modifiers) -> Option<KeyAction> {
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            events: vec![egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            }],
            ..Default::default()
        };
        let mut action = None;
        let mut output = ctx.run_ui(raw, |ui| {
            action = ui.input(|input| keymap.action_for_input(input));
        });
        output.textures_delta.clear();
        action
    }

    // Given: a ui.toml binding SaveLayout to Alt+S and the role switch to Alt+R
    // When: the core client's keymap is built from it
    // Then: Alt+S saves; Alt+R is no action and stays with a focused terminal
    #[test]
    fn the_keymap_follows_ui_settings_without_the_role_switch() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("ui.toml"),
            "[keybinds.bindings]\nsave_layout = \"Alt+S\"\ncycle_agent_role = \"Alt+R\"\n",
        )
        .unwrap();

        let keymap = core_keymap(&ui_settings(dir.path()).keybinds);

        assert_eq!(
            action(&keymap, Key::S, Modifiers::ALT),
            Some(KeyAction::SaveLayout)
        );
        assert_eq!(action(&keymap, Key::R, Modifiers::ALT), None);
        assert!(!keymap.passes_through_terminal(Key::R, Modifiers::ALT));
    }

    // Given: a ui.toml that does not parse
    // When: the settings are read
    // Then: the defaults are used
    #[test]
    fn unreadable_ui_settings_give_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ui.toml"), "version = \"x\"").unwrap();

        assert_eq!(ui_settings(dir.path()), workspace_ui::UiSettings::default());
    }
}
