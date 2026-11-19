//! O-G: the GUI side of the agent browser observation, composed inside
//! BrowserFeature (not another feature or connection). Read-only: it lists the
//! runs of the selected thread, watches one of them at a time and shows its tabs,
//! its latest frame and its activity. Nothing here drives the page.
//!
//! * One run is observed at a time. Its frames come through one shared data slot
//!   (`DataSource::AgentFrames`); a payload is converted and uploaded once and
//!   never kept as bytes.
//! * What stopped is kept as a still picture, at most
//!   [`MAX_STOPPED_IMAGES`] per thread. Disconnect, a change of the selection and
//!   a revoked incarnation drop them all (or the run's own).
//! * Hidden, collapsed or minimized: the subscription is released (data cancel
//!   and unsubscribe). Showing it again asks again; no token or bind is replayed.
//! * Every request tag and data tag of this leaf has the high bit set (`TAG_BASE`),
//!   which keeps them apart from the preview's.
//! * What a core refusal or a stream failure ended is not started again by
//!   itself; the user's reload does it.
mod pixels;
mod view;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, VecDeque};

use workbench_protocol::{
    commands::{Command, CommandResult},
    events::UiEvent,
    ids::{ProjectId, ThreadId},
    namespaces::browser::{
        BrowserCommand, BrowserEvent, BrowserQuery, BrowserReply, BrowserResult,
        observation::{
            MAX_RUNS, MAX_STOPPED_IMAGES, ObservationId, ObservationReply, ObservationResult,
            ObservationScope, ObservationSnapshot, ObservationState, ObservationTarget,
        },
    },
    queries::{Query, QueryReply},
};

use super::{
    super::{
        Feature, FeatureContext, FeatureId, FeatureReply, Navigation, Visibility,
        data::{DataNotice, DataPayload, DataSource},
    },
    frame::Picture,
};

pub const TAG_BASE: u64 = 1 << 63;
pub fn owns_tag(tag: u64) -> bool {
    tag >= TAG_BASE
}

/// Unsubscribes waiting to be sent; a dropped one is released by the core's own
/// bind window.
const MAX_UNSUBSCRIBES: usize = 8;
/// Scopes waiting to be checked against the core, at most one per picture kept
/// and the live one.
const MAX_VERIFY: usize = MAX_STOPPED_IMAGES + 1;

const NOTE_STOPPED: &str = "The page was closed.";
const NOTE_UNAVAILABLE: &str = "このrunのブラウザは観測できません";
const NOTE_REVOKED: &str = "この実行は失効したため観測を終了しました";
const NOTE_CAPACITY: &str = "同時に観測できる数の上限です。しばらくしてから更新してください";
const NOTE_ENDED: &str = "配信が終了しました";
const NOTE_UNEXPECTED: &str = "予期しない応答を受け取りました";

fn state_note(state: ObservationState) -> &'static str {
    match state {
        ObservationState::Stopped => NOTE_STOPPED,
        ObservationState::Revoked => NOTE_REVOKED,
        ObservationState::Capacity => NOTE_CAPACITY,
        ObservationState::Unavailable => NOTE_UNAVAILABLE,
        ObservationState::Starting | ObservationState::Live => NOTE_UNEXPECTED,
    }
}

/// What a request in flight is for.
#[derive(Debug)]
enum Request {
    List {
        project: ProjectId,
        thread: ThreadId,
    },
    Snapshot {
        scope: ObservationScope,
        /// The answer also says whether the scope is still valid.
        verify: bool,
    },
    /// Whether the incarnation of the scope is still valid.
    Verify(ObservationScope),
    Subscribe(ObservationScope),
    Token {
        scope: ObservationScope,
        subscription: ObservationId,
    },
    Unsubscribe,
}

enum Runs {
    Unknown,
    Loading,
    Loaded(Vec<ObservationScope>),
    Failed(String),
}

/// Where the subscription of the live run stands.
#[derive(Debug, Clone)]
enum Stage {
    /// The subscribe command is in flight.
    Subscribing(u64),
    /// Subscribed: the private token is to be asked for.
    NeedToken(ObservationId),
    Token(ObservationId),
    NeedBind {
        subscription: ObservationId,
        token: ObservationId,
    },
    Binding {
        data: u64,
        subscription: ObservationId,
    },
    Streaming {
        data: u64,
        subscription: ObservationId,
    },
}

impl Stage {
    fn subscription(&self) -> Option<&ObservationId> {
        match self {
            Self::Subscribing(_) => None,
            Self::NeedToken(s) | Self::Token(s) => Some(s),
            Self::NeedBind { subscription, .. }
            | Self::Binding { subscription, .. }
            | Self::Streaming { subscription, .. } => Some(subscription),
        }
    }

    fn data(&self) -> Option<u64> {
        match self {
            Self::Binding { data, .. } | Self::Streaming { data, .. } => Some(*data),
            _ => None,
        }
    }
}

/// The picture on screen: the only copy of the pixels the GUI keeps.
struct Shown {
    picture: Picture,
    tab: ObservationId,
}

struct Live {
    scope: ObservationScope,
    stage: Stage,
    snapshot: Option<ObservationSnapshot>,
    snapshot_in_flight: bool,
    /// The snapshot should be read again (a change was signalled).
    stale: bool,
    /// A revocation notice asked the next snapshot to confirm this scope too.
    verify: bool,
    shown: Option<Shown>,
}

/// A run that is no longer watched, with the last of what it showed.
struct Stopped {
    scope: ObservationScope,
    snapshot: Option<ObservationSnapshot>,
    shown: Shown,
    note: Option<String>,
}

/// Why the observation of a target is not started again by itself.
struct Hold {
    target: ObservationTarget,
    message: String,
}

pub struct ObservationFeature {
    next_tag: u64,
    /// The run the user wants to see. It is an intent, not a grant: the core's
    /// list decides whether it can be watched.
    target: Option<ObservationTarget>,
    /// Navigation to the target's thread was asked for.
    navigated: bool,
    was_visible: bool,
    runs: Runs,
    relist: bool,
    live: Option<Live>,
    stopped: VecDeque<Stopped>,
    hold: Option<Hold>,
    requests: HashMap<u64, Request>,
    /// Data slots to give back.
    cancels: Vec<u64>,
    outbox: VecDeque<ObservationId>,
    /// Scopes whose validity is to be read from the core.
    verify: VecDeque<ObservationScope>,
}

impl Default for ObservationFeature {
    fn default() -> Self {
        Self {
            next_tag: TAG_BASE,
            target: None,
            navigated: false,
            was_visible: false,
            runs: Runs::Unknown,
            relist: false,
            live: None,
            stopped: VecDeque::new(),
            hold: None,
            requests: HashMap::new(),
            cancels: Vec::new(),
            outbox: VecDeque::new(),
            verify: VecDeque::new(),
        }
    }
}

impl ObservationFeature {
    /// The typed View-to-run intent. It does no I/O: the next composition pass does.
    pub fn observe_run(&mut self, target: ObservationTarget) {
        self.choose(target);
    }

    fn tag(&mut self) -> u64 {
        let tag = self.next_tag;
        self.next_tag = if tag == u64::MAX { TAG_BASE } else { tag + 1 };
        tag
    }

    fn choose(&mut self, target: ObservationTarget) {
        self.hold = None;
        self.navigated = false;
        if self
            .live
            .as_ref()
            .is_some_and(|live| live.scope.run != target.run)
        {
            self.end_live(None);
        }
        let known = matches!(&self.runs, Runs::Loaded(scopes) if scopes.iter().any(|s| s.run == target.run));
        if !known {
            self.relist = true;
        }
        self.target = Some(target);
    }

    /// What the session held is gone with the session (or is to be released): only
    /// the user's intent stays.
    fn forget_session(&mut self) {
        self.runs = Runs::Unknown;
        self.relist = false;
        self.live = None;
        self.stopped.clear();
        self.hold = None;
        self.requests.clear();
        self.cancels.clear();
        self.outbox.clear();
        self.verify.clear();
        self.navigated = false;
    }

    fn release_stage(&mut self, stage: &Stage) {
        if let Some(data) = stage.data() {
            self.cancels.push(data);
        }
        if let Some(subscription) = stage.subscription()
            && self.outbox.len() < MAX_UNSUBSCRIBES
        {
            self.outbox.push_back(subscription.clone());
        }
    }

    /// Ask the core whether `scope` (the exact scope, generations included) is
    /// still valid. The end of one subscription says nothing about it (a
    /// disconnect or an expired bind ends a subscription too), so the core's
    /// answer decides, and an answer that does not confirm it counts as lost.
    /// The live scope is confirmed by its next snapshot, which is read anyway.
    fn queue_verify(&mut self, scope: &ObservationScope) {
        if let Some(live) = &mut self.live
            && live.scope == *scope
        {
            live.verify = true;
            live.stale = true;
            return;
        }
        let asked = self.verify.contains(scope)
            || self
                .requests
                .values()
                .any(|r| matches!(r, Request::Verify(s) if s == scope));
        if !asked && self.verify.len() < MAX_VERIFY {
            self.verify.push_back(scope.clone());
        }
    }

    /// `scope` is no longer valid, whichever state its watch is in: its
    /// pictures go, those of any other scope (a newer page generation or
    /// incarnation of the run) stay.
    fn revoke_scope(&mut self, scope: &ObservationScope) {
        if self.live.as_ref().is_some_and(|l| l.scope == *scope) {
            self.revoke();
        }
        self.stopped.retain(|s| s.scope != *scope);
        let superseded = matches!(&self.runs, Runs::Loaded(scopes)
            if scopes.iter().any(|s| s.run == scope.run && s != scope));
        if self.live.is_none()
            && !superseded
            && self.target.as_ref().is_some_and(|t| t.run == scope.run)
        {
            self.hold_for(scope, NOTE_REVOKED.to_owned());
        }
    }

    fn push_stopped(&mut self, stopped: Stopped) {
        self.stopped.retain(|s| s.scope.run != stopped.scope.run);
        self.stopped.push_back(stopped);
        while self.stopped.len() > MAX_STOPPED_IMAGES {
            self.stopped.pop_front();
        }
    }

    /// The run stops being watched; its last picture stays as a still one.
    fn end_live(&mut self, note: Option<String>) {
        let Some(live) = self.live.take() else { return };
        self.release_stage(&live.stage);
        if let Some(shown) = live.shown {
            self.push_stopped(Stopped {
                scope: live.scope,
                snapshot: live.snapshot,
                shown,
                note,
            });
        }
    }

    fn hold_for(&mut self, scope: &ObservationScope, message: String) {
        self.hold = Some(Hold {
            target: ObservationTarget {
                project: scope.project.clone(),
                thread: scope.thread.clone(),
                run: scope.run.clone(),
            },
            message,
        });
    }

    /// The watch ended and is not started again before the user asks.
    fn finish(&mut self, message: &str) {
        let Some(scope) = self.live.as_ref().map(|l| l.scope.clone()) else {
            return;
        };
        self.end_live(Some(message.to_owned()));
        self.hold_for(&scope, message.to_owned());
    }

    /// The incarnation is no longer valid: nothing of the run stays on screen.
    fn revoke(&mut self) {
        let Some(scope) = self.live.as_ref().map(|l| l.scope.clone()) else {
            return;
        };
        if let Some(live) = self.live.take() {
            self.release_stage(&live.stage);
        }
        self.stopped.retain(|s| s.scope != scope);
        self.hold_for(&scope, NOTE_REVOKED.to_owned());
    }

    fn apply_state(&mut self, state: ObservationState) {
        match state {
            ObservationState::Starting | ObservationState::Live => {}
            ObservationState::Revoked => self.revoke(),
            other => self.finish(state_note(other)),
        }
    }

    fn on_runs(&mut self, project: &ProjectId, thread: &ThreadId, scopes: Vec<ObservationScope>) {
        let scopes: Vec<_> = scopes
            .into_iter()
            .filter(|s| &s.project == project && &s.thread == thread)
            .take(MAX_RUNS)
            .collect();
        if let Some(live) = &self.live
            && let Some(now) = scopes.iter().find(|s| s.run == live.scope.run)
            && *now != live.scope
        {
            if now.incarnation != live.scope.incarnation {
                self.revoke();
            } else {
                self.end_live(None);
            }
        }
        self.stopped.retain(|st| {
            scopes
                .iter()
                .find(|s| s.run == st.scope.run)
                .is_none_or(|s| s.incarnation == st.scope.incarnation)
        });
        self.runs = Runs::Loaded(scopes);
    }

    fn on_subscribe_reply(&mut self, scope: ObservationScope, reply: FeatureReply, tag: u64) {
        let waiting = self.live.as_ref().is_some_and(|l| {
            l.scope == scope && matches!(l.stage, Stage::Subscribing(t) if t == tag)
        });
        let result = match reply {
            FeatureReply::Command(Ok(reply)) => match reply.result.map(|r| *r) {
                Some(CommandResult::Browser(BrowserResult::AgentObservation(r))) => Ok(r),
                _ => Err(NOTE_UNEXPECTED.to_owned()),
            },
            FeatureReply::Command(Err(e)) | FeatureReply::Query(Err(e)) => {
                Err(if e.outcome_unknown {
                    format!("{}（結果が分かりません。再送しません）", e.message)
                } else {
                    e.message
                })
            }
            FeatureReply::Query(Ok(_)) => Err(NOTE_UNEXPECTED.to_owned()),
        };
        match result {
            Ok(ObservationResult::Subscribed { subscription, .. }) => {
                if waiting && let Some(live) = &mut self.live {
                    live.stage = Stage::NeedToken(subscription);
                } else if self.outbox.len() < MAX_UNSUBSCRIBES {
                    // Nobody wants it any more.
                    self.outbox.push_back(subscription);
                }
            }
            Ok(ObservationResult::State(state)) if waiting => match state {
                ObservationState::Starting | ObservationState::Live => self.finish(NOTE_UNEXPECTED),
                other => self.apply_state(other),
            },
            Ok(_) if waiting => self.finish(NOTE_UNEXPECTED),
            Err(message) if waiting => self.finish(&message),
            _ => {}
        }
    }

    fn on_token_reply(
        &mut self,
        scope: ObservationScope,
        subscription: ObservationId,
        reply: FeatureReply,
    ) {
        let waiting = self.live.as_ref().is_some_and(|l| {
            l.scope == scope && matches!(&l.stage, Stage::Token(s) if *s == subscription)
        });
        if !waiting {
            if self.outbox.len() < MAX_UNSUBSCRIBES {
                self.outbox.push_back(subscription);
            }
            return;
        }
        match reply {
            FeatureReply::Query(Ok(QueryReply::Browser(BrowserReply::AgentObservation(
                ObservationReply::Subscription {
                    subscription: id,
                    scope: s,
                    token: Some(token),
                    ..
                },
            )))) if id == subscription && s == scope => {
                if let Some(live) = &mut self.live {
                    live.stage = Stage::NeedBind {
                        subscription,
                        token,
                    };
                }
            }
            FeatureReply::Query(Ok(QueryReply::Browser(BrowserReply::AgentObservation(
                ObservationReply::State(state),
            )))) => match state {
                ObservationState::Starting | ObservationState::Live => self.finish(NOTE_UNEXPECTED),
                other => self.apply_state(other),
            },
            FeatureReply::Query(Err(e)) | FeatureReply::Command(Err(e)) => self.finish(&e.message),
            _ => self.finish(NOTE_UNEXPECTED),
        }
    }

    /// Whether an answer to `GetAgentObservation` shows `scope` to be valid:
    /// a live snapshot, or the run stopped under the same incarnation. Anything
    /// else (revoked, unavailable, a refusal, an unknown outcome, a reply of
    /// another shape) does not confirm it. The host also refuses, rather than
    /// answers, when the scope is lost between its snapshot and its own check.
    fn confirms(reply: &FeatureReply, scope: &ObservationScope) -> bool {
        match reply {
            FeatureReply::Query(Ok(QueryReply::Browser(BrowserReply::AgentObservation(
                ObservationReply::Snapshot(snapshot),
            )))) => {
                snapshot.scope == *scope
                    && matches!(
                        snapshot.state,
                        ObservationState::Starting
                            | ObservationState::Live
                            | ObservationState::Stopped
                    )
            }
            FeatureReply::Query(Ok(QueryReply::Browser(BrowserReply::AgentObservation(
                ObservationReply::State(state),
            )))) => *state == ObservationState::Stopped,
            _ => false,
        }
    }

    fn on_verified(&mut self, scope: &ObservationScope, reply: &FeatureReply) {
        if !Self::confirms(reply, scope) {
            self.revoke_scope(scope);
        }
    }

    fn on_snapshot_reply(&mut self, scope: ObservationScope, verify: bool, reply: FeatureReply) {
        if !self.live.as_ref().is_some_and(|l| l.scope == scope) {
            // The watch ended before this answer. A confirmation is still
            // owed; otherwise only a revocation the core states matters.
            let stated = matches!(
                &reply,
                FeatureReply::Query(Ok(QueryReply::Browser(BrowserReply::AgentObservation(
                    ObservationReply::State(ObservationState::Revoked)
                ),)))
            ) || matches!(&reply, FeatureReply::Query(Ok(QueryReply::Browser(
                    BrowserReply::AgentObservation(ObservationReply::Snapshot(s)),
                )))
                    if s.scope == scope && s.state == ObservationState::Revoked);
            if stated || verify {
                self.on_verified(&scope, &reply);
            }
            return;
        }
        if verify && !Self::confirms(&reply, &scope) {
            self.revoke();
            return;
        }
        let Some(live) = &mut self.live else { return };
        live.snapshot_in_flight = false;
        match reply {
            FeatureReply::Query(Ok(QueryReply::Browser(BrowserReply::AgentObservation(
                ObservationReply::Snapshot(snapshot),
            )))) if snapshot.scope == scope && snapshot.validate().is_ok() => {
                let newer = live
                    .snapshot
                    .as_ref()
                    .is_none_or(|old| snapshot.revision >= old.revision);
                let state = snapshot.state;
                if newer {
                    live.snapshot = Some(*snapshot);
                }
                self.apply_state(state);
            }
            FeatureReply::Query(Ok(QueryReply::Browser(BrowserReply::AgentObservation(
                ObservationReply::State(state),
            )))) => self.apply_state(state),
            _ => {}
        }
    }

    fn flush(&mut self, ctx: &mut FeatureContext<'_>) {
        for tag in std::mem::take(&mut self.cancels) {
            ctx.cancel_data(tag);
        }
        while let Some(scope) = self.verify.pop_front() {
            let tag = self.tag();
            self.requests.insert(tag, Request::Verify(scope.clone()));
            ctx.query(
                tag,
                Query::Browser(BrowserQuery::GetAgentObservation { scope }),
            );
        }
        while let Some(subscription) = self.outbox.pop_front() {
            let tag = self.tag();
            self.requests.insert(tag, Request::Unsubscribe);
            ctx.command(
                tag,
                Command::Browser(BrowserCommand::UnsubscribeAgentFrames { subscription }),
            );
        }
    }

    fn drive(&mut self, ctx: &mut FeatureContext<'_>) {
        let (Some(project), Some(thread)) = (
            ctx.selection().project.clone(),
            ctx.selection().thread.clone(),
        ) else {
            return;
        };
        if (matches!(self.runs, Runs::Unknown) || self.relist)
            && !matches!(self.runs, Runs::Loading)
        {
            let tag = self.tag();
            self.requests.insert(
                tag,
                Request::List {
                    project: project.clone(),
                    thread: thread.clone(),
                },
            );
            self.runs = Runs::Loading;
            self.relist = false;
            ctx.query(
                tag,
                Query::Browser(BrowserQuery::ListAgentRuns { project, thread }),
            );
        }
        if self.live.is_none()
            && !self.relist
            && let (Some(target), Runs::Loaded(scopes)) = (&self.target, &self.runs)
            && self.hold.as_ref().is_none_or(|h| h.target != *target)
            && let Some(scope) = scopes
                .iter()
                .find(|s| {
                    s.run == target.run && s.project == target.project && s.thread == target.thread
                })
                .cloned()
        {
            let tag = self.tag();
            self.requests.insert(tag, Request::Subscribe(scope.clone()));
            ctx.command(
                tag,
                Command::Browser(BrowserCommand::SubscribeAgentFrames {
                    scope: scope.clone(),
                }),
            );
            self.live = Some(Live {
                scope,
                stage: Stage::Subscribing(tag),
                snapshot: None,
                snapshot_in_flight: false,
                stale: true,
                verify: false,
                shown: None,
            });
        }
        let Some(live) = &self.live else { return };
        let scope = live.scope.clone();
        match live.stage.clone() {
            Stage::NeedToken(subscription) => {
                let tag = self.tag();
                self.requests.insert(
                    tag,
                    Request::Token {
                        scope,
                        subscription: subscription.clone(),
                    },
                );
                ctx.query(
                    tag,
                    Query::Browser(BrowserQuery::GetAgentSubscription {
                        subscription: subscription.clone(),
                    }),
                );
                if let Some(live) = &mut self.live {
                    live.stage = Stage::Token(subscription);
                }
            }
            Stage::NeedBind {
                subscription,
                token,
            } => {
                let data = self.tag();
                ctx.start_data(
                    data,
                    DataSource::AgentFrames {
                        scope,
                        subscription: subscription.clone(),
                        token,
                    },
                );
                if let Some(live) = &mut self.live {
                    live.stage = Stage::Binding { data, subscription };
                }
            }
            _ => {}
        }
        let Some(live) = &mut self.live else { return };
        if live.stale && !live.snapshot_in_flight {
            live.stale = false;
            live.snapshot_in_flight = true;
            let verify = std::mem::take(&mut live.verify);
            let scope = live.scope.clone();
            let tag = self.tag();
            self.requests.insert(
                tag,
                Request::Snapshot {
                    scope: scope.clone(),
                    verify,
                },
            );
            ctx.query(
                tag,
                Query::Browser(BrowserQuery::GetAgentObservation { scope }),
            );
        }
    }

    /// Take the latest frame of the stream, once, and put it on the screen.
    fn ingest(&mut self, ctx: &mut FeatureContext<'_>, egui_ctx: &egui::Context) {
        let Some(live) = &mut self.live else { return };
        let Some(data) = live.stage.data() else {
            return;
        };
        let Some(DataPayload::AgentFrame(frame)) = ctx.take_data(data) else {
            return;
        };
        let last = live.shown.as_ref().and_then(|s| s.picture.sequence);
        if frame.start.scope != live.scope || last.is_some_and(|last| frame.start.sequence <= last)
        {
            return;
        }
        let Ok(image) = pixels::rgba_to_image(&frame) else {
            return;
        };
        let tab = frame.start.tab.clone();
        let changed = live.shown.as_ref().is_none_or(|s| s.tab != tab);
        let shown = live.shown.get_or_insert_with(|| Shown {
            picture: Picture::default(),
            tab: tab.clone(),
        });
        shown.picture.show(
            egui_ctx,
            "agent-observation",
            image,
            Some(frame.start.sequence),
        );
        shown.tab = tab;
        if changed {
            live.stale = true;
        }
        let run = live.scope.run.clone();
        self.stopped.retain(|s| s.scope.run != run);
    }
}

impl Feature for ObservationFeature {
    fn id(&self) -> FeatureId {
        FeatureId::Browser
    }

    fn title(&self) -> &'static str {
        "Agent observation"
    }

    fn reset(&mut self) {
        self.forget_session();
    }

    fn disconnected(&mut self) {
        self.forget_session();
    }

    fn selection_changed(&mut self, ctx: &mut FeatureContext<'_>) {
        if let Some(live) = self.live.take() {
            self.release_stage(&live.stage);
        }
        self.stopped.clear();
        self.verify.clear();
        self.runs = Runs::Unknown;
        self.relist = false;
        self.hold = None;
        self.navigated = false;
        // A reply still to come for a subscription is answered with its release.
        self.requests.retain(|_, r| {
            !matches!(
                r,
                Request::List { .. } | Request::Snapshot { .. } | Request::Verify(_)
            )
        });
        let selection = ctx.selection();
        if self.target.as_ref().is_some_and(|t| {
            selection.project.as_ref() != Some(&t.project)
                || selection.thread.as_ref() != Some(&t.thread)
        }) {
            self.target = None;
        }
        self.flush(ctx);
    }

    fn event(&mut self, event: &UiEvent) {
        let UiEvent::Browser(BrowserEvent::AgentObservationChanged(changed)) = event else {
            return;
        };
        // The revision is not compared: a state change may come with the same one.
        if let Some(live) = &mut self.live
            && live.stage.subscription() == Some(&changed.subscription)
        {
            match changed.state {
                ObservationState::Starting | ObservationState::Live => live.stale = true,
                // The end of this subscription, not necessarily of the incarnation.
                ObservationState::Revoked => {
                    let scope = live.scope.clone();
                    self.finish(NOTE_ENDED);
                    self.queue_verify(&scope);
                }
                other => self.apply_state(other),
            }
            return;
        }
        // Another subscription, or one whose stream already ended: the notice
        // names no scope, so everything on screen is checked with the core.
        if changed.state == ObservationState::Revoked {
            let scopes: Vec<_> = self
                .stopped
                .iter()
                .map(|s| s.scope.clone())
                .chain(self.live.as_ref().map(|l| l.scope.clone()))
                .collect();
            for scope in &scopes {
                self.queue_verify(scope);
            }
        }
    }

    fn reply(&mut self, tag: u64, reply: FeatureReply) {
        let Some(request) = self.requests.remove(&tag) else {
            return;
        };
        match request {
            Request::List { project, thread } => {
                if !matches!(self.runs, Runs::Loading) {
                    return;
                }
                match reply {
                    FeatureReply::Query(Ok(QueryReply::Browser(
                        BrowserReply::AgentObservation(ObservationReply::Runs { scopes }),
                    ))) => self.on_runs(&project, &thread, scopes),
                    FeatureReply::Query(Ok(QueryReply::Browser(
                        BrowserReply::AgentObservation(ObservationReply::State(state)),
                    ))) => self.runs = Runs::Failed(state_note(state).to_owned()),
                    FeatureReply::Query(Err(e)) | FeatureReply::Command(Err(e)) => {
                        self.runs = Runs::Failed(e.message)
                    }
                    _ => self.runs = Runs::Failed(NOTE_UNEXPECTED.to_owned()),
                }
            }
            Request::Snapshot { scope, verify } => self.on_snapshot_reply(scope, verify, reply),
            Request::Verify(scope) => self.on_verified(&scope, &reply),
            Request::Subscribe(scope) => self.on_subscribe_reply(scope, reply, tag),
            Request::Token {
                scope,
                subscription,
            } => self.on_token_reply(scope, subscription, reply),
            Request::Unsubscribe => {}
        }
    }

    fn data(&mut self, tag: u64, notice: DataNotice) {
        let Some(live) = &mut self.live else { return };
        let (subscription, streaming) = match &live.stage {
            Stage::Binding { data, subscription } if *data == tag => (subscription.clone(), false),
            Stage::Streaming { data, subscription } if *data == tag => (subscription.clone(), true),
            _ => return,
        };
        match notice {
            DataNotice::Started => {
                if !streaming {
                    live.stage = Stage::Streaming {
                        data: tag,
                        subscription,
                    };
                }
            }
            DataNotice::Ready => {}
            DataNotice::Completed => self.finish(NOTE_ENDED),
            DataNotice::Failed(error) => self.finish(&error.message),
            DataNotice::Cancelled => self.finish(NOTE_ENDED),
        }
    }

    fn maintain(&mut self, ctx: &mut FeatureContext<'_>, visibility: Visibility) {
        let visible = visibility.is_visible();
        if visible && !self.was_visible {
            // Shown again: what was asked before is asked afresh.
            self.relist = true;
            self.hold = None;
        }
        self.was_visible = visible;
        if !ctx.connected() {
            return;
        }
        if !visible {
            self.end_live(None);
            self.flush(ctx);
            return;
        }
        self.flush(ctx);
        if let Some(target) = &self.target {
            let selection = ctx.selection();
            if !self.navigated
                && (selection.project.as_ref() != Some(&target.project)
                    || selection.thread.as_ref() != Some(&target.thread))
            {
                self.navigated = true;
                ctx.navigate(Navigation::SelectThread {
                    project: target.project.clone(),
                    thread: target.thread.clone(),
                });
                return;
            }
        }
        self.drive(ctx);
    }

    fn render(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        let egui_ctx = ui.ctx().clone();
        self.ingest(ctx, &egui_ctx);
        if let Some(click) = self.draw(ui) {
            self.apply(click);
            egui_ctx.request_repaint();
        }
    }
}
