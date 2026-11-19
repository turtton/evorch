//! The observation leaf without a core: a scripted fake answers its requests
//! and data slots are claimed by the real `DataInbox`. Completion is awaited
//! through explicit signals (replies, notices), never through sleeps.
use std::collections::HashMap;

use egui_kittest::{Harness, kittest::Queryable};
use workbench_client::renderer::observation::AgentFrame;
use workbench_protocol::{
    commands::{CommandReply, CommandResult},
    events::UiEvent,
    ids::{ProjectId, RunId, ThreadId},
    namespaces::browser::{
        BrowserEvent,
        observation::{
            ObservationActivity, ObservationEvent, ObservationTab, data::AgentFrameStart,
        },
    },
};

use super::*;
use crate::core_client::{
    cache::Cache,
    features::{
        FeatureAction, FeatureFrame, FeatureRequest, FeatureView, Selection,
        data::{DataAction, DataInbox, DataKey, DataRequest},
    },
};
use workbench_protocol::handshake::Capability;

static CAPS: [Capability; 2] = [Capability::Queries, Capability::Commands];

fn oid(n: u64) -> ObservationId {
    ObservationId::new(format!("{n:032x}")).unwrap()
}

fn scope_in(thread: &str, run: &str, incarnation: &str) -> ObservationScope {
    ObservationScope {
        project: ProjectId::new("p").unwrap(),
        thread: ThreadId::new(thread).unwrap(),
        run: RunId::new(run).unwrap(),
        incarnation: RunId::new(incarnation).unwrap(),
        core_epoch: 7,
        browser_generation: 1,
        page_generation: 1,
    }
}

fn scope(run: &str) -> ObservationScope {
    scope_in("t", run, "i1")
}

fn target(run: &str) -> ObservationTarget {
    target_in("t", run)
}

fn target_in(thread: &str, run: &str) -> ObservationTarget {
    ObservationTarget {
        project: ProjectId::new("p").unwrap(),
        thread: ThreadId::new(thread).unwrap(),
        run: RunId::new(run).unwrap(),
    }
}

fn snapshot(scope: &ObservationScope, activity: Vec<ObservationActivity>) -> ObservationSnapshot {
    ObservationSnapshot {
        scope: scope.clone(),
        revision: 1,
        state: ObservationState::Live,
        tabs: vec![ObservationTab {
            id: oid(1),
            title: "Example".into(),
            url: "https://example.test/a".into(),
        }],
        active_tab: Some(oid(1)),
        activity,
    }
}

fn frame(scope: &ObservationScope, tab: u64, sequence: u64) -> AgentFrame {
    AgentFrame {
        start: AgentFrameStart {
            scope: scope.clone(),
            tab: oid(tab),
            sequence,
            width: 2,
            height: 1,
            stride: 8,
            total_bytes: 8,
        },
        rgba: vec![255, 0, 0, 255, 0, 255, 0, 255],
    }
}

fn subscribe_of(command: &Command) -> Option<&ObservationScope> {
    match command {
        Command::Browser(BrowserCommand::SubscribeAgentFrames { scope }) => Some(scope),
        _ => None,
    }
}

fn unsubscribed(command: &Command) -> Option<&ObservationId> {
    match command {
        Command::Browser(BrowserCommand::UnsubscribeAgentFrames { subscription }) => {
            Some(subscription)
        }
        _ => None,
    }
}

// ---- the fake core -----------------------------------------------------

#[derive(Default)]
struct Core {
    scopes: Vec<ObservationScope>,
    activity: Vec<ObservationActivity>,
    subscriptions: HashMap<ObservationId, ObservationScope>,
    serial: u64,
    commands: Vec<Command>,
    queries: Vec<Query>,
    starts: Vec<(u64, DataSource)>,
    cancels: Vec<u64>,
    keys: HashMap<u64, DataKey>,
    navigations: Vec<Navigation>,
    refuse_subscribe: Option<FeatureError>,
    /// Scopes whose incarnation the core no longer accepts.
    revoked: Vec<ObservationScope>,
    /// Scopes the host refuses (it lost them between its snapshot and its own check).
    denied: Vec<ObservationScope>,
    /// Scopes of a run that stopped under a valid incarnation.
    stopped_scopes: Vec<ObservationScope>,
}

use crate::core_client::features::FeatureError;

impl Core {
    fn next_id(&mut self) -> ObservationId {
        self.serial += 1;
        oid(100 + self.serial)
    }

    fn subscribes(&self) -> usize {
        self.commands
            .iter()
            .filter(|c| subscribe_of(c).is_some())
            .count()
    }

    fn snapshots(&self) -> usize {
        self.queries
            .iter()
            .filter(|q| matches!(q, Query::Browser(BrowserQuery::GetAgentObservation { .. })))
            .count()
    }

    fn lists(&self) -> usize {
        self.queries
            .iter()
            .filter(|q| matches!(q, Query::Browser(BrowserQuery::ListAgentRuns { .. })))
            .count()
    }

    fn command(&mut self, command: &Command) -> FeatureReply {
        self.commands.push(command.clone());
        let result = match command {
            Command::Browser(BrowserCommand::SubscribeAgentFrames { scope }) => {
                if let Some(error) = self.refuse_subscribe.take() {
                    return FeatureReply::Command(Err(error));
                }
                let id = self.next_id();
                self.subscriptions.insert(id.clone(), scope.clone());
                ObservationResult::Subscribed {
                    subscription: id,
                    state: ObservationState::Starting,
                }
            }
            Command::Browser(BrowserCommand::UnsubscribeAgentFrames { subscription }) => {
                self.subscriptions.remove(subscription);
                ObservationResult::Unsubscribed {}
            }
            other => panic!("unexpected command {other:?}"),
        };
        FeatureReply::Command(Ok(CommandReply::with_result(CommandResult::Browser(
            BrowserResult::AgentObservation(result),
        ))))
    }

    fn query(&mut self, query: &Query) -> FeatureReply {
        self.queries.push(query.clone());
        let reply = match query {
            Query::Browser(BrowserQuery::ListAgentRuns { .. }) => ObservationReply::Runs {
                scopes: self.scopes.clone(),
            },
            Query::Browser(BrowserQuery::GetAgentObservation { scope })
                if self.denied.contains(scope) =>
            {
                return FeatureReply::Query(Err(FeatureError::new("denied")));
            }
            Query::Browser(BrowserQuery::GetAgentObservation { scope })
                if self.stopped_scopes.contains(scope) =>
            {
                ObservationReply::State(ObservationState::Stopped)
            }
            Query::Browser(BrowserQuery::GetAgentObservation { scope })
                if self.revoked.contains(scope) =>
            {
                ObservationReply::State(ObservationState::Revoked)
            }
            Query::Browser(BrowserQuery::GetAgentObservation { scope }) => {
                ObservationReply::Snapshot(Box::new(snapshot(scope, self.activity.clone())))
            }
            Query::Browser(BrowserQuery::GetAgentSubscription { subscription }) => {
                let scope = self.subscriptions[subscription].clone();
                let token = self.next_id();
                ObservationReply::Subscription {
                    subscription: subscription.clone(),
                    scope,
                    token: Some(token),
                    state: ObservationState::Starting,
                }
            }
            other => panic!("unexpected query {other:?}"),
        };
        FeatureReply::Query(Ok(QueryReply::Browser(BrowserReply::AgentObservation(
            reply,
        ))))
    }
}

// ---- the rig -----------------------------------------------------------

struct State {
    feature: ObservationFeature,
    selection: Selection,
    cache: Cache,
    frame: FeatureFrame,
    inbox: DataInbox,
    visible: bool,
    connected: bool,
}

fn with_ctx<R>(
    state: &mut State,
    f: impl FnOnce(&mut ObservationFeature, &mut FeatureContext<'_>) -> R,
) -> R {
    let State {
        feature,
        selection,
        cache,
        frame,
        inbox,
        connected,
        ..
    } = state;
    let view = FeatureView {
        connected: *connected,
        capabilities: &CAPS,
        cache,
        selection,
    };
    let mut ctx = FeatureContext {
        data: inbox,
        feature: FeatureId::Browser,
        view,
        frame,
    };
    f(feature, &mut ctx)
}

struct Rig {
    harness: Harness<'static, State>,
    core: Core,
}

impl Rig {
    fn new(runs: &[&str]) -> Self {
        let state = State {
            feature: ObservationFeature::default(),
            selection: Selection {
                project: Some(ProjectId::new("p").unwrap()),
                thread: Some(ThreadId::new("t").unwrap()),
            },
            cache: Cache::default(),
            frame: FeatureFrame::default(),
            inbox: DataInbox::default(),
            visible: true,
            connected: true,
        };
        let harness = Harness::builder()
            .with_size(egui::vec2(640.0, 480.0))
            .build_ui_state(
                |ui, state: &mut State| {
                    let visibility = Visibility {
                        available: true,
                        expanded: state.visible,
                        minimized: false,
                    };
                    with_ctx(state, |feature, ctx| {
                        feature.maintain(ctx, visibility);
                        if visibility.is_visible() {
                            feature.render(ui, ctx);
                        }
                    });
                },
                state,
            );
        let mut rig = Self {
            harness,
            core: Core {
                scopes: runs.iter().map(|r| scope(r)).collect(),
                ..Core::default()
            },
        };
        rig.pump();
        rig
    }

    fn feature(&mut self) -> &mut ObservationFeature {
        &mut self.harness.state_mut().feature
    }

    fn state(&self) -> &State {
        self.harness.state()
    }

    /// Renders and answers what the feature asked until it asks for nothing.
    fn pump(&mut self) {
        for _ in 0..60 {
            self.harness.run_steps(2);
            let (actions, data, navigation) = {
                let frame = &mut self.harness.state_mut().frame;
                (
                    std::mem::take(&mut frame.actions),
                    std::mem::take(&mut frame.data),
                    std::mem::take(&mut frame.navigation),
                )
            };
            self.core.navigations.extend(navigation);
            if actions.is_empty() && data.is_empty() {
                return;
            }
            for action in data {
                self.data_action(action);
            }
            for action in actions {
                self.answer(action);
            }
        }
        panic!("the feature kept asking");
    }

    fn answer(&mut self, action: FeatureAction) {
        let reply = match &action.request {
            FeatureRequest::Command(command) => self.core.command(command),
            FeatureRequest::Query(query) => self.core.query(query),
        };
        self.feature().reply(action.tag, reply);
    }

    fn data_action(&mut self, action: DataAction) {
        let tag = action.tag;
        match &action.request {
            DataRequest::Start(source) => {
                let source = source.clone();
                self.core.starts.push((tag, source.clone()));
                let state = self.harness.state_mut();
                match state.inbox.claim(&action, source) {
                    Ok(key) => {
                        self.core.keys.insert(tag, key);
                        state.feature.data(tag, DataNotice::Started);
                    }
                    Err(error) => state.feature.data(tag, DataNotice::Failed(error)),
                }
            }
            DataRequest::Cancel => {
                self.core.cancels.push(tag);
                self.core.keys.remove(&tag);
                let state = self.harness.state_mut();
                state.inbox.remove(FeatureId::Browser, tag);
                state.feature.data(tag, DataNotice::Cancelled);
            }
        }
    }

    fn publish(&mut self, tag: u64, frame: AgentFrame) -> bool {
        let key = self.core.keys[&tag];
        let state = self.harness.state_mut();
        let stored = state.inbox.publish(key, DataPayload::AgentFrame(frame));
        state.feature.data(tag, DataNotice::Ready);
        stored
    }

    fn live_tag(&self) -> u64 {
        self.core.starts.last().expect("a stream was started").0
    }

    fn live_scope(&self) -> ObservationScope {
        self.state()
            .feature
            .live
            .as_ref()
            .expect("live")
            .scope
            .clone()
    }

    /// The user picks the run, and its first frame arrives.
    fn watch(&mut self, run: &str, sequence: u64) {
        self.feature().observe_run(target(run));
        self.pump();
        let scope = self.live_scope();
        self.publish(self.live_tag(), frame(&scope, 1, sequence));
        self.pump();
    }

    fn shown_size(&self) -> Option<[usize; 2]> {
        let live = self.state().feature.live.as_ref()?;
        Some(live.shown.as_ref()?.picture.texture()?.size())
    }

    fn select(&mut self, thread: &str) {
        let state = self.harness.state_mut();
        state.selection.thread = Some(ThreadId::new(thread).unwrap());
        with_ctx(state, |feature, ctx| feature.selection_changed(ctx));
        self.pump();
    }

    fn subscription(&self) -> ObservationId {
        self.state()
            .feature
            .live
            .as_ref()
            .and_then(|l| l.stage.subscription().cloned())
            .expect("subscribed")
    }

    /// A notice of the live subscription, with the revision of the snapshot
    /// the core already served (a state may change without a new revision).
    fn event(&mut self, state: ObservationState) {
        let subscription = self.subscription();
        self.notify(subscription, state, 1);
    }

    fn notify(&mut self, subscription: ObservationId, state: ObservationState, revision: u64) {
        self.feature()
            .event(&UiEvent::Browser(BrowserEvent::AgentObservationChanged(
                ObservationEvent {
                    subscription,
                    revision,
                    state,
                },
            )));
        self.pump();
    }

    fn visible(&mut self, visible: bool) {
        self.harness.state_mut().visible = visible;
        self.pump();
    }

    fn click(&mut self, label: &str) {
        self.harness.get_by_label(label).click();
        self.pump();
    }
}

// ---- tests -------------------------------------------------------------

#[test]
fn rgba_keeps_the_color_order_and_a_short_frame_is_refused() {
    let image = pixels::rgba_to_image(&frame(&scope("r"), 1, 1)).unwrap();
    assert_eq!(image.size, [2, 1]);
    assert_eq!(image.pixels, [egui::Color32::RED, egui::Color32::GREEN]);
    let mut short = frame(&scope("r"), 1, 1);
    short.rgba.pop();
    assert!(pixels::rgba_to_image(&short).is_err());
}

#[test]
fn a_page_text_is_one_line() {
    assert_eq!(pixels::one_line("a\nb\tc"), "a b c");
}

#[test]
fn a_run_is_listed_viewed_subscribed_bound_and_shown() {
    let mut rig = Rig::new(&["run-1"]);
    rig.core.activity = (0..8)
        .map(|n| ObservationActivity::Action {
            label: format!("action {n}"),
            error: (n == 7).then(|| "boom".to_owned()),
        })
        .collect();
    assert_eq!(rig.core.lists(), 1);
    assert!(
        rig.state().feature.live.is_none(),
        "listing alone starts no observation"
    );
    rig.click("run-1");
    let (subscription, token) = match &rig.core.starts[..] {
        [
            (
                _,
                DataSource::AgentFrames {
                    scope: s,
                    subscription,
                    token,
                },
            ),
        ] => {
            assert_eq!(*s, scope("run-1"));
            (subscription.clone(), token.clone())
        }
        other => panic!("one agent frame stream expected: {other:?}"),
    };
    assert!(rig.core.subscriptions.contains_key(&subscription));
    assert_ne!(subscription, token);
    assert!(rig.core.starts[0].0 >= TAG_BASE);
    assert_eq!(rig.core.snapshots(), 1);
    assert!(
        rig.core.commands.iter().all(|c| subscribe_of(c).is_some()),
        "only a subscription is ever commanded"
    );

    rig.publish(rig.live_tag(), frame(&scope("run-1"), 1, 1));
    rig.pump();
    assert_eq!(rig.shown_size(), Some([2, 1]));
    rig.harness
        .get_by_label_contains("Showing the active tab Example (https://example.test/a)");
    assert!(rig.harness.query_by_label_contains("action 1").is_none());
    rig.harness.get_by_label_contains("action 7 failed: boom");
}

#[test]
fn the_latest_frame_wins_and_an_older_or_foreign_one_is_not_shown() {
    let mut rig = Rig::new(&["run-1"]);
    rig.click("run-1");
    let tag = rig.live_tag();
    let scope = rig.live_scope();
    assert!(rig.publish(tag, frame(&scope, 1, 1)));
    assert!(rig.publish(tag, frame(&scope, 1, 2)));
    rig.pump();
    let sequence = |rig: &Rig| {
        rig.state()
            .feature
            .live
            .as_ref()
            .unwrap()
            .shown
            .as_ref()
            .unwrap()
            .picture
            .sequence
    };
    assert_eq!(sequence(&rig), Some(2));
    assert!(rig.publish(tag, frame(&scope, 1, 1)));
    rig.pump();
    assert_eq!(sequence(&rig), Some(2), "an older sequence is dropped");
    let mut other = scope.clone();
    other.page_generation += 1;
    assert!(
        !rig.publish(tag, frame(&other, 1, 9)),
        "a frame of another scope never reaches the feature"
    );
}

#[test]
fn a_new_tab_reads_the_snapshot_again_but_the_same_tab_does_not() {
    let mut rig = Rig::new(&["run-1"]);
    rig.click("run-1");
    let (tag, scope) = (rig.live_tag(), rig.live_scope());
    rig.publish(tag, frame(&scope, 1, 1));
    rig.pump();
    let after_first = rig.core.snapshots();
    rig.publish(tag, frame(&scope, 1, 2));
    rig.pump();
    assert_eq!(rig.core.snapshots(), after_first);
    rig.publish(tag, frame(&scope, 2, 3));
    rig.pump();
    assert_eq!(rig.core.snapshots(), after_first + 1);
}

#[test]
fn hiding_releases_the_stream_and_showing_it_again_subscribes_afresh() {
    let mut rig = Rig::new(&["run-1"]);
    rig.watch("run-1", 1);
    let (old_tag, old_token) = match &rig.core.starts[0] {
        (tag, DataSource::AgentFrames { token, .. }) => (*tag, token.clone()),
        _ => unreachable!(),
    };
    rig.visible(false);
    assert_eq!(rig.core.cancels, [old_tag]);
    assert!(rig.core.commands.last().and_then(unsubscribed).is_some());
    assert!(
        rig.core.subscriptions.is_empty(),
        "the core was told to release it"
    );
    assert!(rig.state().feature.live.is_none());
    assert_eq!(
        rig.state().feature.stopped.len(),
        1,
        "the last picture stays"
    );
    assert_eq!(rig.state().inbox.usage().0, 0, "no data slot is held");

    rig.visible(true);
    assert_eq!(rig.core.subscribes(), 2);
    let (new_tag, new_token) = match &rig.core.starts[1] {
        (tag, DataSource::AgentFrames { token, .. }) => (*tag, token.clone()),
        _ => unreachable!(),
    };
    assert_ne!(new_tag, old_tag);
    assert_ne!(new_token, old_token, "nothing of the old bind is replayed");
    let scope = rig.live_scope();
    rig.publish(new_tag, frame(&scope, 1, 1));
    rig.pump();
    assert!(
        rig.state().feature.stopped.is_empty(),
        "the live picture replaces the still one"
    );
}

#[test]
fn a_change_of_selection_drops_the_watch_and_the_still_pictures() {
    let mut rig = Rig::new(&["run-1", "run-2"]);
    rig.watch("run-1", 1);
    rig.watch("run-2", 1);
    assert_eq!(rig.state().feature.stopped.len(), 1);
    let lists = rig.core.lists();
    rig.core.scopes.clear();
    rig.select("t2");
    let feature = &rig.state().feature;
    assert!(feature.live.is_none() && feature.stopped.is_empty());
    assert!(
        feature.target.is_none(),
        "the target belongs to the old thread"
    );
    assert!(rig.core.cancels.contains(&rig.core.starts[1].0));
    assert!(rig.core.subscriptions.is_empty());
    assert_eq!(
        rig.core.lists(),
        lists + 1,
        "the new thread is asked afresh"
    );
}

#[test]
fn a_view_intent_for_the_thread_being_selected_survives_the_selection() {
    let mut rig = Rig::new(&[]);
    rig.core.scopes = vec![scope_in("t2", "run-9", "i1")];
    rig.feature().observe_run(target_in("t2", "run-9"));
    rig.pump();
    assert_eq!(
        rig.core.navigations,
        [Navigation::SelectThread {
            project: ProjectId::new("p").unwrap(),
            thread: ThreadId::new("t2").unwrap()
        }],
        "the thread of the run is selected once"
    );
    rig.select("t2");
    assert_eq!(rig.core.subscribes(), 1);
    assert_eq!(rig.core.navigations.len(), 1);
}

#[test]
fn a_disconnect_forgets_everything_and_a_reconnect_asks_and_subscribes_afresh() {
    let mut rig = Rig::new(&["run-1", "run-2"]);
    rig.watch("run-1", 1);
    rig.watch("run-2", 1);
    let old_token = match &rig.core.starts[1].1 {
        DataSource::AgentFrames { token, .. } => token.clone(),
        _ => unreachable!(),
    };
    let sent = rig.core.commands.len();
    {
        let state = rig.harness.state_mut();
        state.connected = false;
        state.feature.disconnected();
        state.inbox.reset(FeatureId::Browser);
    }
    rig.pump();
    let feature = &rig.state().feature;
    assert!(feature.live.is_none() && feature.stopped.is_empty());
    assert_eq!(
        rig.core.commands.len(),
        sent,
        "nothing is sent while disconnected"
    );

    {
        let state = rig.harness.state_mut();
        state.connected = true;
        state.inbox.reset(FeatureId::Browser);
        state.feature.reset();
    }
    rig.pump();
    assert_eq!(
        rig.core.subscribes(),
        3,
        "the run the user wanted is subscribed again"
    );
    match &rig.core.starts[2].1 {
        DataSource::AgentFrames { token, .. } => assert_ne!(*token, old_token),
        _ => unreachable!(),
    }
}

#[test]
fn a_new_incarnation_revokes_the_run_and_its_pictures() {
    let mut rig = Rig::new(&["run-1"]);
    rig.watch("run-1", 1);
    rig.core.scopes = vec![scope_in("t", "run-1", "i2")];
    rig.click("Refresh agent pages");
    let feature = &rig.state().feature;
    assert!(feature.live.is_none() && feature.stopped.is_empty());
    assert_eq!(
        feature.hold.as_ref().map(|h| h.message.as_str()),
        Some(NOTE_REVOKED)
    );
    assert_eq!(
        rig.core.subscribes(),
        1,
        "a revoked run is not subscribed on its own"
    );
    assert!(rig.core.subscriptions.is_empty());
}

#[test]
fn only_eight_still_pictures_are_kept_per_thread() {
    let runs: Vec<String> = (1..=10).map(|n| format!("run-{n}")).collect();
    let names: Vec<&str> = runs.iter().map(String::as_str).collect();
    let mut rig = Rig::new(&names);
    for run in &names {
        rig.watch(run, 1);
    }
    let kept: Vec<_> = rig
        .state()
        .feature
        .stopped
        .iter()
        .map(|s| s.scope.run.as_str().to_owned())
        .collect();
    assert_eq!(kept.len(), MAX_STOPPED_IMAGES);
    assert!(!kept.contains(&"run-1".to_owned()) && kept.contains(&"run-9".to_owned()));
    assert_eq!(
        rig.state().inbox.usage().0,
        1,
        "one data slot for the one watch"
    );
}

#[test]
fn a_stopped_run_keeps_its_last_picture_and_is_not_started_again() {
    let mut rig = Rig::new(&["run-1"]);
    rig.watch("run-1", 1);
    rig.event(ObservationState::Stopped);
    let feature = &rig.state().feature;
    assert!(feature.live.is_none());
    assert_eq!(feature.stopped.len(), 1);
    assert_eq!(rig.core.cancels.len(), 1);
    assert!(rig.core.subscriptions.is_empty());
    rig.harness.get_by_label_contains(NOTE_STOPPED);
    rig.pump();
    assert_eq!(rig.core.subscribes(), 1);
}

#[test]
fn a_revoked_run_leaves_no_picture() {
    let mut rig = Rig::new(&["run-1"]);
    rig.watch("run-1", 1);
    rig.core.revoked = vec![scope("run-1")];
    rig.event(ObservationState::Revoked);
    let feature = &rig.state().feature;
    assert!(feature.live.is_none() && feature.stopped.is_empty());
    rig.harness.get_by_label_contains(NOTE_REVOKED);
}

#[test]
fn a_refused_subscription_is_shown_and_waits_for_the_user() {
    let mut rig = Rig::new(&["run-1"]);
    rig.core.refuse_subscribe = Some(FeatureError::new("denied"));
    rig.click("run-1");
    rig.pump();
    assert!(rig.state().feature.live.is_none());
    assert_eq!(rig.core.subscribes(), 1, "it is not retried by itself");
    rig.harness.get_by_label_contains("denied");
    rig.click("Refresh agent pages");
    assert_eq!(rig.core.subscribes(), 2);
    assert!(rig.state().feature.live.is_some());
}

#[test]
fn a_subscription_answered_after_the_release_is_released_too() {
    let mut rig = Rig::new(&["run-1"]);
    rig.feature().observe_run(target("run-1"));
    rig.harness.run_steps(2);
    let action = {
        let frame = &mut rig.harness.state_mut().frame;
        let at = frame
            .actions
            .iter()
            .position(
                |a| matches!(&a.request, FeatureRequest::Command(c) if subscribe_of(c).is_some()),
            )
            .expect("the subscribe command");
        let action = frame.actions.remove(at);
        frame.actions.clear();
        action
    };
    rig.core.scopes.clear();
    rig.select("t2");
    rig.answer(action);
    rig.pump();
    assert!(rig.state().feature.live.is_none());
    assert!(
        rig.core.subscriptions.is_empty(),
        "the late subscription was unsubscribed"
    );
}

#[test]
fn a_stream_that_ends_before_its_revocation_still_loses_its_picture() {
    let mut rig = Rig::new(&["run-1"]);
    rig.watch("run-1", 1);
    let subscription = rig.subscription();
    let tag = rig.live_tag();
    rig.feature().data(tag, DataNotice::Completed);
    rig.pump();
    assert!(rig.state().feature.live.is_none());
    assert_eq!(
        rig.state().feature.stopped.len(),
        1,
        "kept as a still picture for now"
    );
    rig.core.revoked = vec![scope("run-1")];
    rig.notify(subscription, ObservationState::Revoked, 1);
    assert!(rig.state().feature.stopped.is_empty());
    rig.harness.get_by_label_contains(NOTE_REVOKED);
}

#[test]
fn a_late_revocation_of_an_old_run_leaves_the_new_watch_alone() {
    let mut rig = Rig::new(&["run-1", "run-2"]);
    rig.watch("run-1", 1);
    let old = rig.subscription();
    rig.watch("run-2", 1);
    assert_eq!(rig.state().feature.stopped.len(), 1);
    rig.core.revoked = vec![scope("run-1")];
    rig.notify(old, ObservationState::Revoked, 1);
    let feature = &rig.state().feature;
    assert!(feature.stopped.is_empty(), "the old run's picture is gone");
    let live = feature.live.as_ref().expect("the new watch stays");
    assert_eq!(live.scope.run.as_str(), "run-2");
    assert!(live.shown.is_some());
    assert!(feature.hold.is_none());
}

#[test]
fn a_late_revocation_names_only_its_own_incarnation() {
    let mut rig = Rig::new(&["run-1"]);
    rig.watch("run-1", 1);
    let old = rig.subscription();
    rig.visible(false);
    rig.core.scopes = vec![scope_in("t", "run-1", "i2")];
    rig.visible(true);
    let tag = rig.live_tag();
    let fresh = rig.live_scope();
    assert_eq!(fresh.incarnation.as_str(), "i2");
    rig.publish(tag, frame(&fresh, 1, 1));
    rig.pump();
    rig.core.revoked = vec![scope("run-1")];
    rig.notify(old, ObservationState::Revoked, 1);
    let live = rig
        .state()
        .feature
        .live
        .as_ref()
        .expect("the new incarnation stays");
    assert_eq!(live.scope.incarnation.as_str(), "i2");
    assert!(live.shown.is_some());
}

#[test]
fn a_revoked_answer_to_a_snapshot_after_the_stream_ended_drops_the_pictures() {
    let mut rig = Rig::new(&["run-1"]);
    rig.watch("run-1", 1);
    let tag = rig.live_tag();
    let scope = rig.live_scope();
    rig.publish(tag, frame(&scope, 2, 2));
    rig.harness.run_steps(2);
    let action = {
        let frame = &mut rig.harness.state_mut().frame;
        let at = frame
            .actions
            .iter()
            .position(|a| {
                matches!(
                    &a.request,
                    FeatureRequest::Query(Query::Browser(BrowserQuery::GetAgentObservation { .. }))
                )
            })
            .expect("the snapshot query of the tab change");
        let action = frame.actions.remove(at);
        frame.actions.clear();
        action
    };
    rig.feature().data(tag, DataNotice::Completed);
    rig.pump();
    assert_eq!(rig.state().feature.stopped.len(), 1);
    rig.feature().reply(
        action.tag,
        FeatureReply::Query(Ok(QueryReply::Browser(BrowserReply::AgentObservation(
            ObservationReply::State(ObservationState::Revoked),
        )))),
    );
    assert!(rig.state().feature.stopped.is_empty());
}

#[test]
fn a_state_change_with_an_unchanged_revision_is_applied() {
    let mut stopped = Rig::new(&["run-1"]);
    stopped.watch("run-1", 1);
    let subscription = stopped.subscription();
    stopped.notify(subscription, ObservationState::Live, 1);
    assert!(
        stopped.state().feature.live.is_some(),
        "Live changes nothing but asks again"
    );
    let subscription = stopped.subscription();
    stopped.notify(subscription, ObservationState::Stopped, 1);
    assert!(stopped.state().feature.live.is_none());
    assert_eq!(
        stopped.state().feature.stopped.len(),
        1,
        "a stopped run keeps its picture"
    );

    let mut revoked = Rig::new(&["run-1"]);
    revoked.watch("run-1", 1);
    let subscription = revoked.subscription();
    revoked.core.revoked = vec![scope("run-1")];
    revoked.notify(subscription, ObservationState::Revoked, 1);
    assert!(revoked.state().feature.live.is_none());
    assert!(
        revoked.state().feature.stopped.is_empty(),
        "a revoked run does not"
    );
}

#[test]
fn the_revocation_of_an_old_subscription_does_not_stop_a_valid_new_watch() {
    let mut rig = Rig::new(&["run-1"]);
    rig.watch("run-1", 1);
    let old = rig.subscription();
    rig.visible(false);
    rig.visible(true);
    let tag = rig.live_tag();
    let scope = rig.live_scope();
    rig.publish(tag, frame(&scope, 1, 1));
    rig.pump();
    assert_eq!(rig.core.subscribes(), 2);
    let checks = rig.core.snapshots();
    rig.notify(old, ObservationState::Revoked, 1);
    assert!(
        rig.core.snapshots() > checks,
        "the scope is checked, not assumed"
    );
    let feature = &rig.state().feature;
    let live = feature.live.as_ref().expect("the watch goes on");
    assert!(live.shown.is_some());
    assert!(feature.hold.is_none());
    assert_eq!(rig.core.subscribes(), 2, "and is not subscribed again");
}

#[test]
fn a_notice_of_a_subscription_nobody_remembers_still_revokes_what_the_core_revoked() {
    let mut rig = Rig::new(&["run-1"]);
    rig.watch("run-1", 1);
    rig.visible(false);
    let picture_of = scope("run-1");
    assert_eq!(rig.state().feature.stopped.len(), 1);
    for _ in 0..20 {
        rig.visible(true);
        rig.visible(false);
    }
    assert_eq!(
        rig.state().feature.stopped.len(),
        1,
        "watches without a frame leave the picture alone"
    );
    rig.core.revoked = vec![picture_of];
    rig.notify(oid(9999), ObservationState::Revoked, 1);
    assert!(rig.state().feature.stopped.is_empty());
}

#[test]
fn a_late_revocation_leaves_the_stopped_view_of_a_newer_incarnation_alone() {
    let mut rig = Rig::new(&["run-1"]);
    rig.watch("run-1", 1);
    let old = rig.subscription();
    rig.visible(false);
    rig.core.scopes = vec![scope_in("t", "run-1", "i2")];
    rig.visible(true);
    let (tag, fresh) = (rig.live_tag(), rig.live_scope());
    rig.publish(tag, frame(&fresh, 1, 1));
    rig.pump();
    rig.event(ObservationState::Stopped);
    rig.core.revoked = vec![scope("run-1")];
    rig.notify(old, ObservationState::Revoked, 1);
    let feature = &rig.state().feature;
    assert_eq!(
        feature.stopped.len(),
        1,
        "the newer incarnation's picture stays"
    );
    assert_eq!(feature.stopped[0].scope.incarnation.as_str(), "i2");
    assert_eq!(
        feature.hold.as_ref().map(|h| h.message.as_str()),
        Some(NOTE_STOPPED),
        "it is not marked revoked"
    );
}

fn scope_gen(run: &str, generation: u64) -> ObservationScope {
    ObservationScope {
        page_generation: generation,
        ..scope(run)
    }
}

#[test]
fn a_notice_of_an_older_page_generation_does_not_stop_the_watch_of_a_newer_one() {
    let mut rig = Rig::new(&["run-1"]);
    rig.watch("run-1", 1);
    let old = rig.subscription();
    rig.visible(false);
    rig.core.scopes = vec![scope_gen("run-1", 2)];
    rig.visible(true);
    assert_eq!(rig.live_scope().page_generation, 2);
    assert_eq!(
        rig.state().feature.stopped.len(),
        1,
        "the old picture still waits"
    );
    rig.core.revoked = vec![scope_gen("run-1", 1)];
    let before = rig.core.snapshots();
    rig.notify(old, ObservationState::Revoked, 1);
    assert_eq!(
        rig.core.snapshots(),
        before + 2,
        "each exact scope is asked"
    );
    let feature = &rig.state().feature;
    assert!(
        feature.stopped.is_empty(),
        "the old generation's picture goes"
    );
    assert_eq!(
        feature
            .live
            .as_ref()
            .expect("the new watch stays")
            .scope
            .page_generation,
        2
    );
    assert!(feature.hold.is_none());
}

#[test]
fn a_confirmation_the_host_refuses_drops_the_picture() {
    let mut rig = Rig::new(&["run-1"]);
    rig.watch("run-1", 1);
    rig.visible(false);
    assert_eq!(rig.state().feature.stopped.len(), 1);
    rig.core.denied = vec![scope("run-1")];
    rig.notify(oid(9999), ObservationState::Revoked, 1);
    assert!(
        rig.state().feature.stopped.is_empty(),
        "unconfirmed counts as lost"
    );
}

#[test]
fn a_refused_confirmation_of_the_live_scope_ends_the_watch_and_its_picture() {
    let mut rig = Rig::new(&["run-1"]);
    rig.watch("run-1", 1);
    rig.core.denied = vec![scope("run-1")];
    rig.notify(oid(9999), ObservationState::Revoked, 1);
    let feature = &rig.state().feature;
    assert!(feature.live.is_none() && feature.stopped.is_empty());
    assert_eq!(
        feature.hold.as_ref().map(|h| h.message.as_str()),
        Some(NOTE_REVOKED)
    );
    assert!(rig.core.subscriptions.is_empty());
}

#[test]
fn the_live_subscription_ending_with_a_refused_confirmation_drops_its_picture() {
    let mut rig = Rig::new(&["run-1"]);
    rig.watch("run-1", 1);
    rig.core.denied = vec![scope("run-1")];
    rig.event(ObservationState::Revoked);
    let feature = &rig.state().feature;
    assert!(feature.live.is_none() && feature.stopped.is_empty());
}

#[test]
fn a_run_that_only_stopped_keeps_its_picture_when_confirmed() {
    let mut rig = Rig::new(&["run-1"]);
    rig.watch("run-1", 1);
    rig.visible(false);
    rig.core.stopped_scopes = vec![scope("run-1")];
    rig.notify(oid(9999), ObservationState::Revoked, 1);
    assert_eq!(rig.state().feature.stopped.len(), 1);
}

#[test]
fn one_snapshot_answers_both_the_refresh_and_the_confirmation() {
    let mut rig = Rig::new(&["run-1"]);
    rig.watch("run-1", 1);
    let before = rig.core.snapshots();
    rig.notify(oid(9999), ObservationState::Revoked, 1);
    assert_eq!(
        rig.core.snapshots(),
        before + 1,
        "no second query for the same scope"
    );
    assert!(rig.state().feature.live.is_some());
}

#[test]
fn a_revocation_of_the_new_generation_before_its_stream_ends_still_checks_the_old_picture() {
    let mut rig = Rig::new(&["run-1"]);
    rig.watch("run-1", 1);
    rig.visible(false);
    rig.core.scopes = vec![scope_gen("run-1", 2)];
    rig.visible(true);
    let tag = rig.live_tag();
    assert_eq!(rig.live_scope().page_generation, 2);
    assert_eq!(
        rig.state().feature.stopped.len(),
        1,
        "generation 1 waits, 2 has no frame"
    );
    rig.core.revoked = vec![scope_gen("run-1", 1)];
    rig.event(ObservationState::Revoked);
    assert!(
        rig.state().feature.live.is_none(),
        "its own subscription ended"
    );
    assert!(
        rig.state().feature.stopped.is_empty(),
        "the generation 1 picture was checked and dropped"
    );
    rig.feature().data(tag, DataNotice::Completed);
    rig.pump();
    assert!(rig.state().feature.stopped.is_empty());
}

#[test]
fn a_hidden_view_still_checks_what_an_old_notice_put_in_question() {
    let mut rig = Rig::new(&["run-1"]);
    rig.watch("run-1", 1);
    let old = rig.subscription();
    rig.visible(false);
    rig.core.revoked = vec![scope("run-1")];
    rig.harness.state_mut().visible = true;
    rig.harness.run_steps(2);
    let before = rig.core.snapshots();
    rig.feature()
        .event(&UiEvent::Browser(BrowserEvent::AgentObservationChanged(
            ObservationEvent {
                subscription: old,
                revision: 1,
                state: ObservationState::Revoked,
            },
        )));
    rig.harness.state_mut().visible = false;
    rig.pump();
    assert_eq!(rig.core.snapshots(), before + 1, "the check was still sent");
    assert!(
        rig.state().feature.stopped.is_empty(),
        "and the picture went"
    );
    assert!(rig.state().feature.verify.is_empty());
}

#[test]
fn a_pending_check_survives_the_end_of_the_watch_and_leaves_with_its_answer() {
    let mut rig = Rig::new(&["run-1"]);
    rig.watch("run-1", 1);
    let scope = rig.live_scope();
    let subscription = rig.subscription();
    rig.feature()
        .event(&UiEvent::Browser(BrowserEvent::AgentObservationChanged(
            ObservationEvent {
                subscription,
                revision: 1,
                state: ObservationState::Revoked,
            },
        )));
    assert!(rig.state().feature.live.is_none());
    assert_eq!(
        rig.state().feature.verify,
        [scope],
        "not tied to the live watch"
    );
    rig.pump();
    assert!(rig.state().feature.verify.is_empty());
}
