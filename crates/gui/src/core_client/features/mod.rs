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
//! A PR implementing the `workspace`, `mcp`, `execution`, `teams` or `lanes`
//! area edits only its own file in this directory (its state, its rendering,
//! how it reads events and replies). The registration below and the connection
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
//! | [`Feature::render`] | each frame while the feature is available; requests and navigation go out through the [`FeatureContext`] |
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
pub mod execution;
pub mod lanes;
pub mod mcp;
pub mod teams;
pub mod workspace;

mod layout;

use super::cache::Cache;
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
}

impl FeatureError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            outcome_unknown: false,
            code: None,
        }
    }

    pub fn outcome_unknown(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            outcome_unknown: true,
            code: None,
        }
    }

    pub fn with_code(mut self, code: workbench_protocol::errors::ErrorCode) -> Self {
        self.code = Some(code);
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

    fn push(&mut self, tag: u64, request: FeatureRequest) {
        self.frame.actions.push(FeatureAction {
            feature: self.feature,
            tag,
            request,
        });
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

    /// One delivered event of this feature's namespace.
    fn event(&mut self, _event: &UiEvent) {}

    /// The answer to the request tagged `tag`.
    fn reply(&mut self, _tag: u64, _reply: FeatureReply) {}

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
}

impl Default for Features {
    fn default() -> Self {
        Self {
            features: vec![
                Box::<workspace::WorkspaceFeature>::default(),
                Box::<mcp::McpFeature>::default(),
                Box::<execution::ExecutionFeature>::default(),
                Box::<teams::TeamsFeature>::default(),
                Box::<lanes::LanesFeature>::default(),
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
        begin_session(&mut self.features, delivered, held);
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

    /// The features shown for a connection with `capabilities`.
    pub fn available(&self, connected: bool, capabilities: &[Capability]) -> Vec<FeatureId> {
        available_ids(&self.features, connected, capabilities)
    }

    /// The workspace feature, for what observes the sidebar it holds.
    pub fn workspace(&self) -> Option<&workspace::WorkspaceFeature> {
        self.features.iter().find_map(|f| f.as_workspace())
    }

    /// Draw every available feature and return what they asked for.
    pub fn render(&mut self, ui: &mut egui::Ui, view: FeatureView<'_>) -> FeatureFrame {
        render_all(&mut self.features, ui, view)
    }

    /// Let the available features draw the selector, in order, until one does.
    pub fn render_selector(&mut self, ui: &mut egui::Ui, view: FeatureView<'_>) -> SelectorOutcome {
        render_selector(&mut self.features, ui, view)
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

fn render_all(
    features: &mut [Box<dyn Feature>],
    ui: &mut egui::Ui,
    view: FeatureView<'_>,
) -> FeatureFrame {
    let mut frame = FeatureFrame::default();
    // The features are stacked in one scrolling column, as tall as the space given to it.
    egui::ScrollArea::vertical()
        .id_salt("features")
        .auto_shrink([false, true])
        .max_height(ui.available_height())
        .show(ui, |ui| {
            for feature in features
                .iter_mut()
                .filter(|f| is_available(f.as_ref(), view.connected, view.capabilities))
            {
                let mut ctx = FeatureContext {
                    feature: feature.id(),
                    view,
                    frame: &mut frame,
                };
                egui::CollapsingHeader::new(feature.title())
                    .default_open(true)
                    .show(ui, |ui| feature.render(ui, &mut ctx));
            }
        });
    frame
}

fn render_selector(
    features: &mut [Box<dyn Feature>],
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
mod tests;
