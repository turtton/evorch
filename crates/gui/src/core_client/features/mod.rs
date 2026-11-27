//! Extension points of the core-client desktop: one feature per area (ADR 0045).
//!
//! The shared connection (`connection.rs`) does one thing for features: it
//! delivers typed protocol envelopes. Events of a feature's namespace reach
//! [`Feature::event`], the replies to its requests reach [`Feature::reply`],
//! and what it wants sent leaves through [`FeatureContext`] as a typed
//! [`Command`] or [`Query`]. A feature never opens, retries or reconnects a
//! connection of its own, never sees the transport, and has no untyped way to
//! talk to the core.
//!
//! A PR implementing an area edits only its own file in this directory (or
//! the submodules in `<area>/`): its state, its rendering,
//! how it reads events and replies. The registration below and the connection
//! do not change.
//!
//! ## What a feature gets
//!
//! | entry point | when |
//! |---|---|
//! | state | the feature struct itself; it lives in [`super::Shared`], is `Default` and starts empty |
//! | [`Feature::reset`] | every time a new snapshot replaces the projection (connect, reconnect, resync): drop state derived from the core, but keep what identifies requests still in flight (see below) |
//! | [`Feature::event`] | for each delivered event of the namespace the feature owns, in order, exactly once per snapshot |
//! | [`Feature::reply`] | the answer to a request the feature sent, matched by the tag it chose |
//! | [`Feature::disconnected`] | immediate, ctx-free invalidation on the actor thread; no I/O or waiting |
//! | [`Feature::selection_changed`] | first composition and changed project/thread, before maintain |
//! | [`Feature::maintain`] | every non-minimized composition pass or minimized native logic tick, regardless of availability, collapse or connection |
//! | [`Feature::render`] | available, expanded, non-minimized bodies only; lifecycle work belongs in maintain |
//! | [`Feature::render_overlay`] | available, non-minimized passes, over the whole window and whichever tab is in front (a modal); lifecycle work belongs in maintain |
//! | [`Feature::render_selector`] | the project/thread selector at the top of the conversation view; return `true` to replace the built-in one (what was queued is applied either way) |
//! | [`Feature::unsupported`] | a note for the list of unsupported operations while the feature is a placeholder |
//!
//! ## Availability
//!
//! [`Feature::availability`] decides whether a feature is shown at all. The
//! default is [`Availability::Placeholder`]: nothing is rendered and nothing is
//! sent, so an empty area is never offered as a working one. An implemented
//! feature names the capabilities it needs and is shown only while connected to
//! a core that offers all of them (the core advertises a capability only for
//! operations it serves).
//!
//! ## Replies, tags and reset
//!
//! A command that was sent but whose reply was lost answers
//! [`FeatureError::outcome_unknown`] first. The connection never resends it: after
//! the next connect it asks the core for the command's receipt, and delivers the
//! resolved reply for the same tag **after** the new snapshot replaced the
//! projection and [`Feature::reset`] ran. A feature must therefore keep what ties a
//! tag to its meaning (which draft, which row) across `reset`; only state derived
//! from the core is dropped. A request gets one reply, or an `outcome_unknown` reply and
//! later the resolved one.
//!
//! ## Selection and navigation
//!
//! [`FeatureContext::selection`] is the project and thread the conversation view
//! currently shows, and [`FeatureContext::cache`] the core's projection (read only).
//! A feature changes the selection only with a typed local [`Navigation`] intent
//! ([`FeatureContext::navigate`]); the view applies it after the frame, exactly as
//! the built-in selector does (a thread selection also loads its history). The
//! built-in selector stays as it is while no feature replaces it. The `workspace`
//! feature draws its own selector by implementing [`Feature::render_selector`]
//! and returning `true`, and only an available feature is asked.
//! ## Binary data (Stage 16b, PR I -> A/B)
//!
//! PR A owns only artifacts.rs/artifacts/; PR B owns only browser.rs/browser/.
//! The shared actor owns connect/bind/read/cancel/join. Use start_data/cancel_data
//! and take_data, not another connection. A uses DataSource::Artifact, B uses
//! Frames/Capture. Tags identify the feature's selection: do not reuse a tag for
//! a different selection until cancellation/replacement. Reset invalidates all
//! old requests and payloads; mutation-receipt correlation above is unchanged.
//! DataNotice carries only a state signal, never bytes. Ready means inspect the
//! latest-slot; Completed means that data stream ended, NOT operation success.
//! Explicitly cancel/restart a completed blob tag to release its slot reservation.
//! Features must discard decoded state on Cancelled/reset and bound their own
//! decoded images/textures. Only GUI rendering may upload textures.
//! Artifact->preview passes only ArtifactRef through open_preview; the browser
//! callback queues it (no I/O); PR B later creates its session and OpenPreview.
//! No automatic retry, reconnect bind or mutation replay is introduced here.
//!
pub mod artifacts;
pub mod auth;
pub mod browser;
pub mod data;
pub mod diagnostics;
pub mod evaluation;
pub mod execution;
pub mod improvement;
pub mod lanes;
pub mod maintenance;
pub mod mcp;
pub mod memory;
pub mod pull_requests;
pub mod restart;
pub mod settings;
pub mod teams;
pub mod workspace;

mod layout;

use super::cache::Cache;
use data::{DataAction, DataInbox, DataNotice, DataPayload, DataRequest, DataSource};
use workbench_protocol::namespaces::artifacts::ArtifactRef;
use workbench_protocol::{
    commands::{Command, CommandReply},
    events::UiEvent,
    handshake::Capability,
    ids::{ProjectId, ThreadId},
    queries::{Query, QueryReply},
};

/// The features, which are the areas of `crate::areas` in the host. The serde names
/// are what the core client's layout file keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureId {
    Workspace,
    Mcp,
    Execution,
    Teams,
    Lanes,
    Settings,
    Auth,
    Diagnostics,
    Memory,
    Improvement,
    Evaluation,
    PullRequests,
    Maintenance,
    Artifacts,
    Browser,
    Restart,
}

impl FeatureId {
    /// The protocol namespace the feature owns.
    pub const fn namespace(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::Mcp => "mcp",
            Self::Execution => "execution",
            Self::Teams => "teams",
            Self::Lanes => "lanes",
            Self::Settings => "settings",
            Self::Auth => "auth",
            Self::Diagnostics => "diagnostics",
            Self::Memory => "memory",
            Self::Improvement => "improvement",
            Self::Evaluation => "evaluation",
            Self::PullRequests => "pull_requests",
            Self::Maintenance => "maintenance",
            Self::Artifacts => "artifacts",
            Self::Browser => "browser",
            Self::Restart => "lifecycle",
        }
    }
}

/// Whether a feature does anything yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    /// Nothing behind it: not rendered, nothing sent.
    Placeholder,
    /// Shown while connected to a core offering every capability listed.
    Implemented { required: &'static [Capability] },
}

/// A request a feature asks the connection to make, typed as the protocol is.
#[derive(Debug, Clone)]
pub enum FeatureRequest {
    Command(Command),
    Query(Query),
}

/// A request together with whom the answer is for. `tag` is the feature's own
/// correlation number, echoed with the reply.
#[derive(Debug, Clone)]
pub struct FeatureAction {
    pub feature: FeatureId,
    pub tag: u64,
    pub request: FeatureRequest,
}

/// Why a request did not produce its reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureError {
    pub message: String,
    /// The command may or may not have been applied. It is not resent.
    pub outcome_unknown: bool,
    /// The code of the core's refusal, when the core answered with one.
    pub code: Option<workbench_protocol::errors::ErrorCode>,
    /// What the core advised the client to do next, when it answered with a refusal.
    pub retry: Option<workbench_protocol::errors::RetryAdvice>,
}

impl FeatureError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            outcome_unknown: false,
            code: None,
            retry: None,
        }
    }

    pub fn outcome_unknown(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            outcome_unknown: true,
            code: None,
            retry: None,
        }
    }

    pub fn with_code(mut self, code: workbench_protocol::errors::ErrorCode) -> Self {
        self.code = Some(code);
        self
    }

    pub fn with_retry(mut self, retry: workbench_protocol::errors::RetryAdvice) -> Self {
        self.retry = Some(retry);
        self
    }
}

/// The answer to a [`FeatureAction`].
#[derive(Debug, Clone)]
pub enum FeatureReply {
    Command(Result<CommandReply, FeatureError>),
    Query(Result<QueryReply, FeatureError>),
}

impl FeatureAction {
    /// The failed reply of a request that never left the client.
    pub(super) fn refused(&self, message: &str) -> FeatureReply {
        let error = FeatureError::new(message);
        match self.request {
            FeatureRequest::Command(_) => FeatureReply::Command(Err(error)),
            FeatureRequest::Query(_) => FeatureReply::Query(Err(error)),
        }
    }
}

/// A reply that has to wait for the next snapshot: the resolved outcome of a
/// command whose reply was lost (see the module docs).
#[derive(Debug, Clone)]
pub struct HeldReply {
    pub feature: FeatureId,
    pub tag: u64,
    pub reply: FeatureReply,
}

/// The project and thread the conversation view shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    pub project: Option<ProjectId>,
    pub thread: Option<ThreadId>,
}

/// A change of the selection a feature asks for. The view applies it after the
/// frame, like a click on the built-in selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Navigation {
    /// Show `project` and no thread.
    SelectProject(ProjectId),
    /// Show an existing thread (its history is loaded).
    SelectThread {
        project: ProjectId,
        thread: ThreadId,
    },
    /// Start a conversation in `project`: a new, still empty thread.
    NewConversation(ProjectId),
}

/// What a feature sees of the client while it renders.
#[derive(Clone, Copy)]
pub struct FeatureView<'a> {
    pub connected: bool,
    pub capabilities: &'a [Capability],
    pub cache: &'a Cache,
    pub selection: &'a Selection,
}

/// What came out of a frame: requests to send and selection changes, both in
/// the order they were made.
#[derive(Debug, Default)]
pub struct FeatureFrame {
    pub actions: Vec<FeatureAction>,
    pub navigation: Vec<Navigation>,
    pub data: Vec<DataAction>,
    pub previews: Vec<ArtifactRef>,
    pub observations: Vec<workbench_protocol::namespaces::browser::observation::ObservationTarget>,
    /// What a lane thread's menu asked of the lanes feature.
    pub lanes: Vec<lanes::LaneIntent>,
    /// What a thread's row or menu asked of the pull requests feature.
    pub pull_requests: Vec<pull_requests::PullRequestIntent>,
    /// What a feature asked of the settings screens (the window applies it like a ⚙ menu entry).
    pub settings: Vec<settings::SettingsIntent>,
}

/// What asking the features for the selector came to.
#[derive(Debug, Default)]
pub struct SelectorOutcome {
    /// A feature drew the selector: the built-in one is to be skipped.
    pub replaced: bool,
    /// What the features asked for while being asked, whether or not one of
    /// them replaced the selector.
    pub frame: FeatureFrame,
}

/// What a feature may do while rendering: read the client's view of the core
/// and queue requests and navigation. They leave after the frame.
pub struct FeatureContext<'a> {
    feature: FeatureId,
    view: FeatureView<'a>,
    frame: &'a mut FeatureFrame,
    data: &'a mut DataInbox,
}

impl FeatureContext<'_> {
    pub fn connected(&self) -> bool {
        self.view.connected
    }

    pub fn has(&self, capability: Capability) -> bool {
        self.view.capabilities.contains(&capability)
    }

    /// The core's projection as the client holds it.
    pub fn cache(&self) -> &Cache {
        self.view.cache
    }

    /// The project and thread the conversation view shows.
    pub fn selection(&self) -> &Selection {
        self.view.selection
    }

    /// Ask for a state-changing command to be sent.
    pub fn command(&mut self, tag: u64, command: Command) {
        self.push(tag, FeatureRequest::Command(command));
    }

    /// Ask for a read-only query to be sent.
    pub fn query(&mut self, tag: u64, query: Query) {
        self.push(tag, FeatureRequest::Query(query));
    }

    /// Ask the view to change the selection.
    pub fn navigate(&mut self, navigation: Navigation) {
        self.frame.navigation.push(navigation);
    }

    /// Start/restart a binary request. Only explicit calls bind; nothing is replayed.
    pub fn start_data(&mut self, tag: u64, source: DataSource) {
        self.data_action(tag, DataRequest::Start(source));
    }
    pub fn cancel_data(&mut self, tag: u64) {
        self.data_action(tag, DataRequest::Cancel);
    }
    fn data_action(&mut self, tag: u64, request: DataRequest) {
        self.frame.data.push(DataAction {
            feature: self.feature,
            tag,
            generation: self.data.generation(self.feature),
            request,
        });
    }
    /// Consume at most one latest payload, on the GUI thread. Do not keep a history.
    pub fn take_data(&mut self, tag: u64) -> Option<DataPayload> {
        self.data.take(self.feature, tag)
    }
    /// PR A emits only a verified reference; PR B queues it and owns OpenPreview.
    pub fn open_preview(&mut self, artifact: ArtifactRef) {
        if self.feature == FeatureId::Artifacts {
            self.frame.previews.push(artifact);
        }
    }

    /// Ask the lanes feature for what a lane thread's menu chose, applied after the frame.
    pub fn lane(&mut self, intent: lanes::LaneIntent) {
        self.frame.lanes.push(intent);
    }

    /// Ask the pull requests feature for what a thread's row or menu chose, applied after
    /// the frame.
    pub fn pull_request(&mut self, intent: pull_requests::PullRequestIntent) {
        self.frame.pull_requests.push(intent);
    }

    /// Ask the window to open a settings screen, applied after the frame.
    pub fn settings(&mut self, intent: settings::SettingsIntent) {
        self.frame.settings.push(intent);
    }

    /// Typed View-to-run intent, processed once after this composition pass.
    pub fn observe_run(
        &mut self,
        target: workbench_protocol::namespaces::browser::observation::ObservationTarget,
    ) {
        self.frame.observations.clear();
        self.frame.observations.push(target);
    }

    fn push(&mut self, tag: u64, request: FeatureRequest) {
        self.frame.actions.push(FeatureAction {
            feature: self.feature,
            tag,
            request,
        });
    }
}

/// Composition state, independent of whether the feature body is drawn.
/// Minimized and collapsed are separate: neither implies a lost connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Visibility {
    pub available: bool,
    pub expanded: bool,
    pub minimized: bool,
}
impl Visibility {
    pub fn is_visible(self) -> bool {
        self.available && self.expanded && !self.minimized
    }
}

/// One area's part of the desktop. Every method but `id` and `title` has a
/// default that does nothing, which is what a placeholder wants.
pub trait Feature: Send {
    fn id(&self) -> FeatureId;
    fn title(&self) -> &'static str;

    fn availability(&self) -> Availability {
        Availability::Placeholder
    }

    /// A new snapshot replaced the projection: forget what came from the core,
    /// and keep what ties a tag of a request in flight to its meaning.
    fn reset(&mut self) {}

    /// The actor lost its control session. Invalidate local state or enqueue cleanup
    /// only: no I/O, decoding, waiting, or protocol commands on the actor thread.
    /// Preserve request tags needed for outcome-unknown/receipt reconciliation.
    fn disconnected(&mut self) {}

    /// First composition, or its project/thread selection changed. Runs before
    /// maintain, even while hidden/disconnected; ctx.selection() is the new value.
    fn selection_changed(&mut self, _ctx: &mut FeatureContext<'_>) {}

    /// Every composition pass, including placeholders, missing capabilities,
    /// collapsed bodies and disconnection. Minimized windows use App::logic instead
    /// (eframe skips UI passes); never run twice if logic is followed by UI. Use ctx
    /// for deferred cleanup/cancel_data; render must not own lifecycle work.
    fn maintain(&mut self, _ctx: &mut FeatureContext<'_>, _visibility: Visibility) {}

    /// One delivered event of this feature's namespace.
    fn event(&mut self, _event: &UiEvent) {}

    /// The answer to the request tagged `tag`.
    fn reply(&mut self, _tag: u64, _reply: FeatureReply) {}

    /// A signal to inspect the latest-slot, never a payload. Keep this callback short.
    fn data(&mut self, _tag: u64, _notice: DataNotice) {}

    /// Browser only: enqueue this reference for later UI processing. No I/O, work,
    /// or synchronous OpenPreview here; the shared actor must not be blocked.
    fn open_preview(&mut self, _artifact: ArtifactRef) {}

    /// Browser-only, typed View-to-run observation intent; callback does no I/O.
    fn observe_run(
        &mut self,
        _target: workbench_protocol::namespaces::browser::observation::ObservationTarget,
    ) {
    }

    /// The client is leaving for good (a clean exit, not a lost connection). Names
    /// what only this client holds in the core and no longer needs, as commands the
    /// actor sends once on a best-effort basis. Never name anything a run, another
    /// client or the core's own execution holds: a kill sends nothing at all, so the
    /// core must keep everything else running either way.
    fn release(&mut self) -> Vec<Command> {
        Vec::new()
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    /// The planned restart this feature knows the core is performing, from the
    /// core's own evidence only; `epoch` is the core the connection was to.
    fn planned_restart(
        &self,
        _epoch: workbench_protocol::wire::InstanceEpoch,
        _now: std::time::Instant,
    ) -> Option<workbench_client::restart::reconnect::PlannedEvidence> {
        None
    }

    /// Draw the feature. Called only while it is available.
    fn render(&mut self, _ui: &mut egui::Ui, _ctx: &mut FeatureContext<'_>) {}

    /// Draw what floats over the whole window (a modal) rather than inside a tab. Called every
    /// non-minimized pass while the feature is available, after the dock, whichever tab is in
    /// front; lifecycle work still belongs in maintain.
    fn render_overlay(&mut self, _egui_ctx: &egui::Context, _ctx: &mut FeatureContext<'_>) {}

    /// What this area cannot do in this mode, shown in the list of unsupported
    /// operations while the feature is a [`Availability::Placeholder`]. `None`
    /// (the default) says nothing: the view never names features itself.
    fn unsupported(&self) -> Option<&'static str> {
        None
    }

    /// Draw the project/thread selector instead of the built-in one. Return
    /// `true` if it was drawn: the built-in selector is then skipped. The
    /// default draws nothing and keeps the built-in selector. Either way the
    /// requests and navigation queued through `ctx` are kept and applied. Called
    /// only while the feature is available.
    fn render_selector(&mut self, _ui: &mut egui::Ui, _ctx: &mut FeatureContext<'_>) -> bool {
        false
    }

    /// The workspace feature itself, for what observes the sidebar it holds.
    fn as_workspace(&self) -> Option<&workspace::WorkspaceFeature> {
        None
    }

    /// The diagnostics feature itself, for the footer's cost of today.
    fn as_diagnostics(&self) -> Option<&diagnostics::DiagnosticsFeature> {
        None
    }

    /// The execution feature itself, for the conversation's goal and plan band
    /// and the Plan tab, which queue what is pressed in them into it.
    fn as_execution(&self) -> Option<&execution::ExecutionFeature> {
        None
    }

    fn as_execution_mut(&mut self) -> Option<&mut execution::ExecutionFeature> {
        None
    }

    /// The workspace feature itself, to hand it what the sidebar's lane menus offer.
    fn as_workspace_mut(&mut self) -> Option<&mut workspace::WorkspaceFeature> {
        None
    }

    /// The lanes feature itself, for the sidebar's lane menus and the conversation's lane
    /// card, which queue what is pressed in them into it.
    fn as_lanes(&self) -> Option<&lanes::LanesFeature> {
        None
    }

    fn as_lanes_mut(&mut self) -> Option<&mut lanes::LanesFeature> {
        None
    }

    /// The pull requests feature itself, for the sidebar's badges and menu item and the
    /// conversation's bar, which queue what is pressed in them into it.
    fn as_pull_requests(&self) -> Option<&pull_requests::PullRequestsFeature> {
        None
    }

    fn as_pull_requests_mut(&mut self) -> Option<&mut pull_requests::PullRequestsFeature> {
        None
    }

    /// The settings feature itself, for the ⚙ menu that opens its modal.
    fn as_settings_mut(&mut self) -> Option<&mut settings::SettingsFeature> {
        None
    }
}

/// Sections the composition was asked to open, and the ones moved to the top of the column.
#[derive(Default)]
struct Pins {
    /// Opened at the next pass.
    reveal: Option<FeatureId>,
    /// The latest first: what the user was taken to (an artifact's preview, the artifacts of
    /// the conversation) is where they look, not somewhere far down a long column.
    pinned: Vec<FeatureId>,
    focus: Vec<FeatureId>,
}

/// The registration of every feature. A PR implementing a feature changes the
/// feature's own file, not this.
pub struct Features {
    features: Vec<Box<dyn Feature>>,
    selection: Option<Selection>,
    expanded: std::collections::HashMap<FeatureId, bool>,
    pins: Pins,
    pub(crate) data: DataInbox,
}

impl Default for Features {
    fn default() -> Self {
        Self {
            data: DataInbox::default(),
            selection: None,
            expanded: Default::default(),
            pins: Pins::default(),
            features: vec![
                Box::<workspace::WorkspaceFeature>::default(),
                Box::<mcp::McpFeature>::default(),
                Box::<execution::ExecutionFeature>::default(),
                Box::<teams::TeamsFeature>::default(),
                Box::<lanes::LanesFeature>::default(),
                Box::<settings::SettingsFeature>::default(),
                Box::<auth::AuthFeature>::default(),
                Box::<diagnostics::DiagnosticsFeature>::default(),
                Box::<memory::MemoryFeature>::default(),
                Box::<improvement::ImprovementFeature>::default(),
                Box::<evaluation::EvaluationFeature>::default(),
                Box::<pull_requests::PullRequestsFeature>::default(),
                Box::<maintenance::MaintenanceFeature>::default(),
                Box::<artifacts::ArtifactsFeature>::default(),
                Box::<browser::BrowserFeature>::default(),
                Box::<restart::RestartFeature>::default(),
            ],
        }
    }
}

impl Features {
    /// Replace the feature with the same id (tests observe delivery with a probe).
    #[cfg(test)]
    pub(super) fn install(&mut self, feature: Box<dyn Feature>) {
        let slot = self
            .features
            .iter_mut()
            .find(|f| f.id() == feature.id())
            .expect("every feature has a slot");
        *slot = feature;
    }

    /// A new snapshot replaced the projection: reset every feature, deliver the
    /// events that arrived during the snapshot, then the replies that were held
    /// for it. In this order, so a reset never discards a held reply.
    pub fn begin_session(&mut self, delivered: &[UiEvent], held: Vec<HeldReply>) {
        self.clear_data();
        begin_session(&mut self.features, delivered, held);
    }

    /// Immediate invalidation, independent of the GUI composition/event loop.
    pub fn disconnected(&mut self) {
        self.clear_data();
        for feature in &mut self.features {
            feature.disconnected();
        }
    }

    /// Route a delivered event to the feature owning its namespace.
    pub fn event(&mut self, event: &UiEvent) {
        route_event(&mut self.features, event);
        // The workspace feature caches the model catalog (from the applied
        // settings) but only receives workspace events: tell it when they may
        // have changed. A project-settings reload leaves the user settings
        // revision unchanged, so a finished reload counts as well.
        if invalidates_model_catalog(event)
            && let Some(workspace) = self.workspace_mut()
        {
            workspace.invalidate_model_catalogs();
        }
    }

    /// Route the answer to a request back to the feature that made it.
    pub fn reply(&mut self, feature: FeatureId, tag: u64, reply: FeatureReply) {
        route_reply(&mut self.features, feature, tag, reply);
    }

    /// A request that never left the client (the queue was full or closed).
    pub fn refuse(&mut self, action: &FeatureAction, message: &str) {
        let reply = action.refused(message);
        self.reply(action.feature, action.tag, reply);
    }

    pub(crate) fn data_notice(&mut self, feature: FeatureId, tag: u64, notice: DataNotice) {
        if let Some(f) = self.features.iter_mut().find(|f| f.id() == feature) {
            f.data(tag, notice);
        }
    }
    pub(crate) fn clear_data(&mut self) {
        for f in &self.features {
            self.data.reset(f.id());
        }
    }
    pub fn observe_run(
        &mut self,
        target: workbench_protocol::namespaces::browser::observation::ObservationTarget,
    ) {
        if let Some(f) = self
            .features
            .iter_mut()
            .find(|f| f.id() == FeatureId::Browser)
        {
            f.observe_run(target);
        }
    }
    /// Local artifact-to-browser routing. Only references cross the boundary. The
    /// browser section opens and is pinned near the top: a preview queued while it is
    /// collapsed would otherwise never start.
    pub fn open_preview(&mut self, artifact: ArtifactRef) {
        if let Some(f) = self
            .features
            .iter_mut()
            .find(|f| f.id() == FeatureId::Browser)
        {
            f.open_preview(artifact);
            self.reveal(FeatureId::Browser);
        }
    }

    /// Open the section of `feature` and move it to the top of the column at the next pass.
    /// The two latest stay there, so the artifact list stays above its preview.
    pub fn reveal(&mut self, feature: FeatureId) {
        self.pins.reveal = Some(feature);
        self.pins.pinned.retain(|f| *f != feature);
        self.pins.pinned.insert(0, feature);
        self.pins.pinned.truncate(2);
        self.pins.focus.retain(|f| *f != feature);
        self.pins.focus.push(feature);
    }

    /// The tabs to bring to the front, oldest request first, so the latest ends in front.
    /// Taken once: the dock applies them before it draws, so the tab renders in that pass.
    pub fn take_focus(&mut self) -> Vec<FeatureId> {
        std::mem::take(&mut self.pins.focus)
    }

    pub fn focus_pending(&self) -> bool {
        !self.pins.focus.is_empty()
    }

    pub fn titles(&self) -> Vec<(FeatureId, &'static str)> {
        self.features.iter().map(|f| (f.id(), f.title())).collect()
    }

    pub fn placeholders(&self) -> Vec<FeatureId> {
        self.features
            .iter()
            .filter(|f| f.availability() == Availability::Placeholder)
            .map(|f| f.id())
            .collect()
    }

    /// Start one composition pass of the docked desktop (see [`DockPass`]). While the window
    /// is minimized the pass draws and maintains nothing: minimized maintenance belongs to
    /// `App::logic` alone, as for [`Features::render`].
    pub fn begin_dock_pass<'a>(
        &'a mut self,
        minimized: bool,
        view: FeatureView<'a>,
    ) -> DockPass<'a> {
        let selection_changed = !minimized && self.observe_selection(view.selection);
        DockPass {
            features: &mut self.features,
            data: &mut self.data,
            expanded: &mut self.expanded,
            view,
            selection_changed,
            frame: FeatureFrame::default(),
            drawn: Vec::new(),
            fronted: Vec::new(),
            selector_drawn: false,
            minimized,
        }
    }

    /// Whether the section of `feature` is shown for this connection.
    pub fn is_shown(
        &self,
        feature: FeatureId,
        connected: bool,
        capabilities: &[Capability],
    ) -> bool {
        self.features
            .iter()
            .any(|f| f.id() == feature && is_available(f.as_ref(), connected, capabilities))
    }

    /// Counters of the binary slots, for measurements and diagnostics.
    pub fn data_stats(&self) -> data::DataStats {
        self.data.stats()
    }

    /// What the client alone holds in the core, to let go of when it exits cleanly.
    pub fn release_commands(&mut self) -> Vec<Command> {
        self.features
            .iter_mut()
            .flat_map(|feature| feature.release())
            .collect()
    }

    /// The features shown for a connection with `capabilities`.
    pub fn available(&self, connected: bool, capabilities: &[Capability]) -> Vec<FeatureId> {
        available_ids(&self.features, connected, capabilities)
    }

    /// The planned restart to wait for when the connection ends (see
    /// [`Feature::planned_restart`]).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn planned_restart(
        &self,
        epoch: workbench_protocol::wire::InstanceEpoch,
        now: std::time::Instant,
    ) -> Option<workbench_client::restart::reconnect::PlannedEvidence> {
        self.features
            .iter()
            .find_map(|feature| feature.planned_restart(epoch, now))
    }

    /// The workspace feature, for what observes the sidebar it holds.
    pub fn workspace(&self) -> Option<&workspace::WorkspaceFeature> {
        self.features.iter().find_map(|f| f.as_workspace())
    }

    pub fn workspace_mut(&mut self) -> Option<&mut workspace::WorkspaceFeature> {
        self.features.iter_mut().find_map(|f| f.as_workspace_mut())
    }

    pub fn diagnostics(&self) -> Option<&diagnostics::DiagnosticsFeature> {
        self.features.iter().find_map(|f| f.as_diagnostics())
    }

    pub fn execution_mut(&mut self) -> Option<&mut execution::ExecutionFeature> {
        self.features.iter_mut().find_map(|f| f.as_execution_mut())
    }

    pub fn lanes_mut(&mut self) -> Option<&mut lanes::LanesFeature> {
        self.features.iter_mut().find_map(|f| f.as_lanes_mut())
    }

    pub fn pull_requests_mut(&mut self) -> Option<&mut pull_requests::PullRequestsFeature> {
        self.features
            .iter_mut()
            .find_map(|f| f.as_pull_requests_mut())
    }

    pub fn settings_mut(&mut self) -> Option<&mut settings::SettingsFeature> {
        self.features.iter_mut().find_map(|f| f.as_settings_mut())
    }

    fn observe_selection(&mut self, selection: &Selection) -> bool {
        let changed = self.selection.as_ref() != Some(selection);
        if changed {
            self.selection = Some(selection.clone());
        }
        changed
    }

    /// The native App::logic path: no Ui/pass/painting exists when minimized.
    /// Reuse the last composed toggle state without touching egui's UI state.
    pub fn maintain_minimized(&mut self, view: FeatureView<'_>) -> FeatureFrame {
        let selection_changed = self.observe_selection(view.selection);
        let mut frame = FeatureFrame::default();
        for feature in &mut self.features {
            let visibility = Visibility {
                available: is_available(feature.as_ref(), view.connected, view.capabilities),
                expanded: self.expanded.get(&feature.id()).copied().unwrap_or(true),
                minimized: true,
            };
            let mut ctx = FeatureContext {
                feature: feature.id(),
                view,
                frame: &mut frame,
                data: &mut self.data,
            };
            maintain_feature(feature.as_mut(), &mut ctx, selection_changed, visibility);
        }
        frame
    }

    /// Compose visible-window features. Minimized maintenance belongs exclusively
    /// to App::logic, even if eframe also runs a UI pass for a visible child viewport.
    pub fn render(&mut self, ui: &mut egui::Ui, view: FeatureView<'_>) -> FeatureFrame {
        if ui.input(|input| input.viewport().minimized.unwrap_or(false)) {
            return FeatureFrame::default();
        }
        let selection_changed = self.observe_selection(view.selection);
        render_all(
            &mut self.features,
            &mut self.data,
            &mut self.expanded,
            &mut self.pins,
            ui,
            view,
            selection_changed,
        )
    }

    /// Let the available features draw the selector, in order, until one does.
    pub fn render_selector(&mut self, ui: &mut egui::Ui, view: FeatureView<'_>) -> SelectorOutcome {
        render_selector(&mut self.features, &mut self.data, ui, view)
    }

    /// What the placeholder features say they cannot do (title and note), for
    /// the list of unsupported operations.
    pub fn unsupported_notes(&self) -> Vec<(&'static str, &'static str)> {
        self.features
            .iter()
            .filter(|f| f.availability() == Availability::Placeholder)
            .filter_map(|f| f.unsupported().map(|note| (f.title(), note)))
            .collect()
    }
}

fn begin_session(features: &mut [Box<dyn Feature>], delivered: &[UiEvent], held: Vec<HeldReply>) {
    for feature in features.iter_mut() {
        feature.reset();
    }
    for event in delivered {
        route_event(features, event);
    }
    for HeldReply {
        feature,
        tag,
        reply,
    } in held
    {
        route_reply(features, feature, tag, reply);
    }
}

fn invalidates_model_catalog(event: &UiEvent) -> bool {
    use workbench_protocol::namespaces::settings::{SettingsEvent, SettingsOperationKind};
    match event {
        UiEvent::Settings(SettingsEvent::SettingsSnapshotChanged { .. }) => true,
        UiEvent::Settings(SettingsEvent::SettingsOperationCompleted { operation }) => {
            operation.kind == SettingsOperationKind::ReloadModels
        }
        _ => false,
    }
}

fn route_event(features: &mut [Box<dyn Feature>], event: &UiEvent) {
    let namespace = event.namespace();
    for feature in features {
        if feature.id().namespace() == namespace {
            feature.event(event);
        }
    }
}

fn route_reply(features: &mut [Box<dyn Feature>], id: FeatureId, tag: u64, reply: FeatureReply) {
    if let Some(feature) = features.iter_mut().find(|f| f.id() == id) {
        feature.reply(tag, reply);
    }
}

fn is_available(feature: &dyn Feature, connected: bool, capabilities: &[Capability]) -> bool {
    match feature.availability() {
        Availability::Placeholder => false,
        Availability::Implemented { required } => {
            connected && required.iter().all(|c| capabilities.contains(c))
        }
    }
}

fn available_ids(
    features: &[Box<dyn Feature>],
    connected: bool,
    capabilities: &[Capability],
) -> Vec<FeatureId> {
    features
        .iter()
        .filter(|f| is_available(f.as_ref(), connected, capabilities))
        .map(|f| f.id())
        .collect()
}

fn maintain_feature(
    feature: &mut dyn Feature,
    ctx: &mut FeatureContext<'_>,
    selection_changed: bool,
    visibility: Visibility,
) {
    if selection_changed {
        feature.selection_changed(ctx);
    }
    feature.maintain(ctx, visibility);
}

fn render_all(
    features: &mut [Box<dyn Feature>],
    data: &mut DataInbox,
    expanded: &mut std::collections::HashMap<FeatureId, bool>,
    pins: &mut Pins,
    ui: &mut egui::Ui,
    view: FeatureView<'_>,
    selection_changed: bool,
) -> FeatureFrame {
    let mut frame = FeatureFrame::default();
    // The features are stacked in one scrolling column, as tall as the space given to it,
    // the pinned sections first.
    let mut order: Vec<usize> = pins
        .pinned
        .iter()
        .filter_map(|id| features.iter().position(|f| f.id() == *id))
        .collect();
    let rest: Vec<usize> = (0..features.len()).filter(|i| !order.contains(i)).collect();
    order.extend(rest);
    let opening = pins.reveal.take();
    egui::ScrollArea::vertical()
        .id_salt("features")
        .auto_shrink([false, true])
        .max_height(ui.available_height())
        .show(ui, |ui| {
            for index in order {
                let feature = &mut features[index];
                let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(
                    ui.ctx(),
                    feature_header_id(ui, feature.id()),
                    true,
                );
                if opening == Some(feature.id())
                    && is_available(feature.as_ref(), view.connected, view.capabilities)
                {
                    state.set_open(true);
                }
                let mut visibility = Visibility {
                    available: is_available(feature.as_ref(), view.connected, view.capabilities),
                    expanded: state.is_open(),
                    minimized: false,
                };
                expanded.insert(feature.id(), visibility.expanded);
                let mut ctx = FeatureContext {
                    feature: feature.id(),
                    view,
                    frame: &mut frame,
                    data,
                };
                if !visibility.available {
                    maintain_feature(feature.as_mut(), &mut ctx, selection_changed, visibility);
                    continue;
                }
                let mut clicked = false;
                if opening == Some(feature.id()) {
                    // The revealed section is the first of the column. Once, at the request,
                    // every scroll area around it jumps to its header: an animation would
                    // chase the layout while the section opens.
                    let bar = egui::Rect::from_min_size(
                        ui.cursor().min,
                        egui::vec2(ui.available_width(), ui.spacing().interact_size.y),
                    );
                    ui.scroll_to_rect_animation(
                        bar,
                        Some(egui::Align::Min),
                        egui::style::ScrollAnimation::none(),
                    );
                }
                let mut header = state.show_header(ui, |ui| {
                    clicked = ui.selectable_label(false, feature.title()).clicked();
                });
                if clicked {
                    header.toggle();
                }
                // The current toggle, not animation openness, controls subscriptions.
                visibility.expanded = header.is_open();
                expanded.insert(feature.id(), visibility.expanded);
                maintain_feature(feature.as_mut(), &mut ctx, selection_changed, visibility);
                header.body(|ui| {
                    if visibility.is_visible() {
                        feature.render(ui, &mut ctx);
                    }
                });
            }
        });
    frame
}

fn feature_header_id(ui: &egui::Ui, feature: FeatureId) -> egui::Id {
    ui.make_persistent_id(("core-feature", feature.namespace()))
}

fn render_selector(
    features: &mut [Box<dyn Feature>],
    data: &mut DataInbox,
    ui: &mut egui::Ui,
    view: FeatureView<'_>,
) -> SelectorOutcome {
    let mut frame = FeatureFrame::default();
    // The sidebar offers lane operations only while the lanes feature can send them.
    let menu = features
        .iter()
        .find(|f| f.id() == FeatureId::Lanes)
        .filter(|f| is_available(f.as_ref(), view.connected, view.capabilities))
        .and_then(|f| f.as_lanes())
        .map(lanes::LanesFeature::menu);
    // The badges and "Link PR…" are offered only while the pull requests feature can read.
    let pull_requests = features
        .iter()
        .find(|f| f.id() == FeatureId::PullRequests)
        .filter(|f| is_available(f.as_ref(), view.connected, view.capabilities))
        .and_then(|f| f.as_pull_requests())
        .map(pull_requests::PullRequestsFeature::menu);
    if let Some(workspace) = features.iter_mut().find_map(|f| f.as_workspace_mut()) {
        workspace.set_lane_menu(menu);
        workspace.set_pull_request_menu(pull_requests);
    }
    for feature in features
        .iter_mut()
        .filter(|f| is_available(f.as_ref(), view.connected, view.capabilities))
    {
        let mut ctx = FeatureContext {
            feature: feature.id(),
            view,
            frame: &mut frame,
            data,
        };
        if feature.render_selector(ui, &mut ctx) {
            return SelectorOutcome {
                replaced: true,
                frame,
            };
        }
    }
    SelectorOutcome {
        replaced: false,
        frame,
    }
}

/// One composition pass of the docked desktop. Every feature is maintained exactly once per
/// pass: a feature whose tab the dock draws is maintained as visible right before it renders,
/// and every other one (an inactive or closed tab, an unavailable or placeholder feature) is
/// maintained as hidden by [`DockPass::finish`]. Visibility is what the dock really drew in
/// this pass, never a guess made before it, so a stream runs only while its tab is on screen.
pub struct DockPass<'a> {
    features: &'a mut [Box<dyn Feature>],
    data: &'a mut DataInbox,
    expanded: &'a mut std::collections::HashMap<FeatureId, bool>,
    view: FeatureView<'a>,
    selection_changed: bool,
    frame: FeatureFrame,
    drawn: Vec<FeatureId>,
    fronted: Vec<FeatureId>,
    selector_drawn: bool,
    minimized: bool,
}

impl<'a> DockPass<'a> {
    pub fn view(&self) -> FeatureView<'a> {
        self.view
    }

    pub fn navigate(&mut self, navigation: Navigation) {
        self.frame.navigation.push(navigation);
    }

    pub fn is_available(&self, feature: FeatureId) -> bool {
        self.features.iter().any(|f| {
            f.id() == feature
                && is_available(f.as_ref(), self.view.connected, self.view.capabilities)
        })
    }

    /// The execution feature while it is available.
    pub fn execution(&self) -> Option<&execution::ExecutionFeature> {
        self.is_available(FeatureId::Execution)
            .then(|| self.features.iter().find_map(|f| f.as_execution()))
            .flatten()
    }

    /// The workspace feature while it is available.
    pub fn workspace(&self) -> Option<&workspace::WorkspaceFeature> {
        self.is_available(FeatureId::Workspace)
            .then(|| self.features.iter().find_map(|f| f.as_workspace()))
            .flatten()
    }

    pub fn execution_mut(&mut self) -> Option<&mut execution::ExecutionFeature> {
        if !self.is_available(FeatureId::Execution) {
            return None;
        }
        self.features.iter_mut().find_map(|f| f.as_execution_mut())
    }

    /// The lanes feature while it is available.
    pub fn lanes(&self) -> Option<&lanes::LanesFeature> {
        self.is_available(FeatureId::Lanes)
            .then(|| self.features.iter().find_map(|f| f.as_lanes()))
            .flatten()
    }

    /// The pull requests feature while it is available.
    pub fn pull_requests(&self) -> Option<&pull_requests::PullRequestsFeature> {
        self.is_available(FeatureId::PullRequests)
            .then(|| self.features.iter().find_map(|f| f.as_pull_requests()))
            .flatten()
    }

    /// Draw the body of `feature` in its tab. Returns `false` when nothing was drawn: the
    /// feature is unavailable, the window is minimized, or it was already drawn this pass.
    pub fn render_tab(&mut self, feature: FeatureId, ui: &mut egui::Ui) -> bool {
        if !self.minimized && !self.fronted.contains(&feature) {
            self.fronted.push(feature);
        }
        if self.minimized || self.drawn.contains(&feature) || !self.is_available(feature) {
            return false;
        }
        let Some(entry) = self.features.iter_mut().find(|f| f.id() == feature) else {
            return false;
        };
        self.drawn.push(feature);
        self.expanded.insert(feature, true);
        let visibility = Visibility {
            available: true,
            expanded: true,
            minimized: false,
        };
        let mut ctx = FeatureContext {
            feature,
            view: self.view,
            frame: &mut self.frame,
            data: self.data,
        };
        maintain_feature(entry.as_mut(), &mut ctx, self.selection_changed, visibility);
        layout::in_tab(ui, |ui| entry.render(ui, &mut ctx));
        true
    }

    pub fn render_selector(&mut self, ui: &mut egui::Ui) -> bool {
        self.selector_drawn = true;
        let outcome = render_selector(self.features, self.data, ui, self.view);
        self.frame.absorb(outcome.frame);
        outcome.replaced
    }

    /// Draw what the available features show over the whole window, after the dock. The
    /// feature is maintained by the pass as usual: this only draws.
    pub fn render_overlays(&mut self, egui_ctx: &egui::Context) {
        if self.minimized {
            return;
        }
        for feature in self.features.iter_mut() {
            if !is_available(
                feature.as_ref(),
                self.view.connected,
                self.view.capabilities,
            ) {
                continue;
            }
            let mut ctx = FeatureContext {
                feature: feature.id(),
                view: self.view,
                frame: &mut self.frame,
                data: self.data,
            };
            feature.render_overlay(egui_ctx, &mut ctx);
        }
    }

    /// End the pass and hand back what the features asked for. The selector is asked even
    /// when its tab is hidden, on a child that draws nothing, so the requests the workspace
    /// queued still leave.
    pub fn finish(mut self, ui: &mut egui::Ui) -> FeatureFrame {
        if !self.selector_drawn {
            let mut hidden = ui.new_child(egui::UiBuilder::new().invisible());
            self.render_selector(&mut hidden);
        }
        if self.minimized {
            return self.frame;
        }
        for feature in self.features.iter_mut() {
            let id = feature.id();
            if self.drawn.contains(&id) {
                continue;
            }
            // A tab in front stays expanded while its area is unavailable, as an open
            // section did, so it is drawn again as soon as the area returns.
            let expanded = self.fronted.contains(&id);
            self.expanded.insert(id, expanded);
            let visibility = Visibility {
                available: is_available(
                    feature.as_ref(),
                    self.view.connected,
                    self.view.capabilities,
                ),
                expanded,
                minimized: false,
            };
            let mut ctx = FeatureContext {
                feature: id,
                view: self.view,
                frame: &mut self.frame,
                data: self.data,
            };
            maintain_feature(
                feature.as_mut(),
                &mut ctx,
                self.selection_changed,
                visibility,
            );
        }
        self.frame
    }
}

impl FeatureFrame {
    fn absorb(&mut self, other: FeatureFrame) {
        self.actions.extend(other.actions);
        self.navigation.extend(other.navigation);
        self.data.extend(other.data);
        self.previews.extend(other.previews);
        self.lanes.extend(other.lanes);
        self.pull_requests.extend(other.pull_requests);
        if !other.observations.is_empty() {
            self.observations = other.observations;
        }
    }
}

#[cfg(test)]
mod maintenance_tests;
#[cfg(test)]
mod tests;
