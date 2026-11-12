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
//! | [`Feature::reset`] | every time a new snapshot replaces the projection (connect, reconnect, resync): drop state derived from the core |
//! | [`Feature::event`] | for each delivered event of the namespace the feature owns, in order, exactly once per snapshot |
//! | [`Feature::reply`] | the answer to a request the feature sent, matched by the tag it chose |
//! | [`Feature::render`] | each frame while the feature is available; requests go out through the [`FeatureContext`] |
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
//! Replies: a command that was sent but whose reply was lost answers
//! [`FeatureError::outcome_unknown`] first; the connection never resends it and
//! reconciles it after the next connect, which delivers the resolved reply for
//! the same tag.
pub mod execution;
pub mod lanes;
pub mod mcp;
pub mod teams;
pub mod workspace;

use workbench_protocol::{
    commands::{Command, CommandReply},
    events::UiEvent,
    handshake::Capability,
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
}

impl FeatureError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            outcome_unknown: false,
        }
    }

    pub fn outcome_unknown(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            outcome_unknown: true,
        }
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

/// What a feature may do while rendering: see the connection and queue
/// requests. Requests leave after the frame, in the order they were queued.
pub struct FeatureContext<'a> {
    feature: FeatureId,
    connected: bool,
    capabilities: &'a [Capability],
    outbox: &'a mut Vec<FeatureAction>,
}

impl FeatureContext<'_> {
    pub fn connected(&self) -> bool {
        self.connected
    }

    pub fn has(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }

    /// Ask for a state-changing command to be sent.
    pub fn command(&mut self, tag: u64, command: Command) {
        self.push(tag, FeatureRequest::Command(command));
    }

    /// Ask for a read-only query to be sent.
    pub fn query(&mut self, tag: u64, query: Query) {
        self.push(tag, FeatureRequest::Query(query));
    }

    fn push(&mut self, tag: u64, request: FeatureRequest) {
        self.outbox.push(FeatureAction {
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

    /// A new snapshot replaced the projection: forget what came from the core.
    fn reset(&mut self) {}

    /// One delivered event of this feature's namespace.
    fn event(&mut self, _event: &UiEvent) {}

    /// The answer to the request tagged `tag`.
    fn reply(&mut self, _tag: u64, _reply: FeatureReply) {}

    /// Draw the feature. Called only while it is available.
    fn render(&mut self, _ui: &mut egui::Ui, _ctx: &mut FeatureContext<'_>) {}
}

/// The registration of every feature. A PR implementing a feature changes the
/// feature's own file, not this.
#[derive(Default)]
pub struct Features {
    workspace: workspace::WorkspaceFeature,
    mcp: mcp::McpFeature,
    execution: execution::ExecutionFeature,
    teams: teams::TeamsFeature,
    lanes: lanes::LanesFeature,
}

impl Features {
    fn each(&mut self) -> [&mut dyn Feature; 5] {
        [
            &mut self.workspace,
            &mut self.mcp,
            &mut self.execution,
            &mut self.teams,
            &mut self.lanes,
        ]
    }

    /// A new snapshot replaced the projection.
    pub fn reset(&mut self) {
        for feature in self.each() {
            feature.reset();
        }
    }

    /// Route a delivered event to the feature owning its namespace.
    pub fn event(&mut self, event: &UiEvent) {
        route_event(&mut self.each(), event);
    }

    /// Route the answer to a request back to the feature that made it.
    pub fn reply(&mut self, feature: FeatureId, tag: u64, reply: FeatureReply) {
        route_reply(&mut self.each(), feature, tag, reply);
    }

    /// A request that never left the client (the queue was full or closed).
    pub fn refuse(&mut self, action: &FeatureAction, message: &str) {
        let reply = action.refused(message);
        self.reply(action.feature, action.tag, reply);
    }

    /// The features shown for a connection with `capabilities`.
    pub fn available(&mut self, connected: bool, capabilities: &[Capability]) -> Vec<FeatureId> {
        available_ids(&mut self.each(), connected, capabilities)
    }

    /// Draw every available feature and return what they asked to send.
    pub fn render(
        &mut self,
        ui: &mut egui::Ui,
        connected: bool,
        capabilities: &[Capability],
    ) -> Vec<FeatureAction> {
        render_all(&mut self.each(), ui, connected, capabilities)
    }
}

fn route_event(features: &mut [&mut dyn Feature], event: &UiEvent) {
    let namespace = event.namespace();
    for feature in features {
        if feature.id().namespace() == namespace {
            feature.event(event);
        }
    }
}

fn route_reply(features: &mut [&mut dyn Feature], id: FeatureId, tag: u64, reply: FeatureReply) {
    if let Some(feature) = features.iter_mut().find(|f| f.id() == id) {
        feature.reply(tag, reply);
    }
}

fn available_ids(
    features: &mut [&mut dyn Feature],
    connected: bool,
    capabilities: &[Capability],
) -> Vec<FeatureId> {
    features
        .iter()
        .filter(|feature| match feature.availability() {
            Availability::Placeholder => false,
            Availability::Implemented { required } => {
                connected && required.iter().all(|c| capabilities.contains(c))
            }
        })
        .map(|feature| feature.id())
        .collect()
}

fn render_all(
    features: &mut [&mut dyn Feature],
    ui: &mut egui::Ui,
    connected: bool,
    capabilities: &[Capability],
) -> Vec<FeatureAction> {
    let mut outbox = Vec::new();
    let shown = available_ids(features, connected, capabilities);
    for feature in features.iter_mut().filter(|f| shown.contains(&f.id())) {
        let mut ctx = FeatureContext {
            feature: feature.id(),
            connected,
            capabilities,
            outbox: &mut outbox,
        };
        egui::CollapsingHeader::new(feature.title())
            .default_open(true)
            .show(ui, |ui| feature.render(ui, &mut ctx));
    }
    outbox
}

#[cfg(test)]
mod tests;
