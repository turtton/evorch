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

/// The features, which are the areas of `crate::areas` in the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

    /// Draw the feature. Called only while it is available.
    fn render(&mut self, _ui: &mut egui::Ui, _ctx: &mut FeatureContext<'_>) {}

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
}

/// The registration of every feature. A PR implementing a feature changes the
/// feature's own file, not this.
pub struct Features {
    features: Vec<Box<dyn Feature>>,
    selection: Option<Selection>,
    expanded: std::collections::HashMap<FeatureId, bool>,
    /// A section the composition was asked to open and scroll to at the next pass.
    reveal: Option<FeatureId>,
    pub(crate) data: DataInbox,
}

impl Default for Features {
    fn default() -> Self {
        Self {
            data: DataInbox::default(),
            selection: None,
            expanded: Default::default(),
            reveal: None,
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
    /// browser section opens and is scrolled into view: a preview queued while it is
    /// collapsed would otherwise never start.
    pub fn open_preview(&mut self, artifact: ArtifactRef) {
        if let Some(f) = self
            .features
            .iter_mut()
            .find(|f| f.id() == FeatureId::Browser)
        {
            f.open_preview(artifact);
            self.reveal = Some(FeatureId::Browser);
        }
    }

    /// Open the section of `feature` and scroll it into view at the next pass.
    pub fn reveal(&mut self, feature: FeatureId) {
        self.reveal = Some(feature);
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

    /// The workspace feature, for what observes the sidebar it holds.
    pub fn workspace(&self) -> Option<&workspace::WorkspaceFeature> {
        self.features.iter().find_map(|f| f.as_workspace())
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
            &mut self.reveal,
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
    reveal: &mut Option<FeatureId>,
    ui: &mut egui::Ui,
    view: FeatureView<'_>,
    selection_changed: bool,
) -> FeatureFrame {
    let mut frame = FeatureFrame::default();
    // The features are stacked in one scrolling column, as tall as the space given to it.
    egui::ScrollArea::vertical()
        .id_salt("features")
        .auto_shrink([false, true])
        .max_height(ui.available_height())
        .show(ui, |ui| {
            for feature in features.iter_mut() {
                let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(
                    ui.ctx(),
                    feature_header_id(ui, feature.id()),
                    true,
                );
                let revealed = *reveal == Some(feature.id());
                if revealed {
                    *reveal = None;
                    if is_available(feature.as_ref(), view.connected, view.capabilities) {
                        state.set_open(true);
                    }
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
                if revealed {
                    ui.scroll_to_cursor(Some(egui::Align::TOP));
                }
                let mut clicked = false;
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

#[cfg(test)]
mod maintenance_tests;
#[cfg(test)]
mod tests;
