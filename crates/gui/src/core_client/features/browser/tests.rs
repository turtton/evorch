//! The browser feature without a core: a scripted fake answers its requests,
//! time is a clock the test moves, and completion is awaited through explicit
//! signals (replies, notices, the decoder's channel), never through sleeps.
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    io::Cursor,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use egui::{Event, Key, Modifiers, PointerButton, Rect, pos2, vec2};
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use serde_json::{Value, json};
use workbench_client::renderer::Frame;
use workbench_protocol::{
    commands::{CommandReply, CommandResult},
    ids::{ProjectId, ThreadId},
    namespaces::{
        artifacts::ArtifactsResult,
        browser::{
            BrowserResult, RendererFailure, RendererOperation, RendererOperationState,
            RendererOwnerKind, RendererSessionRef, RendererTabRef,
            data::{BlobStart, FrameStart},
        },
    },
};

use super::{
    super::{
        FeatureAction, FeatureFrame, FeatureView, Features, Selection,
        data::{Blob, DataAction, DataInbox, DataKey, DataRequest},
    },
    frame::{PixelError, bgra_to_image, decode_capture},
    input::{Button, Input, InputQueue, Surface, Translator},
    preview::MAX_NOTICES,
    *,
};
use crate::core_client::cache::Cache;

const SIZE: egui::Vec2 = egui::vec2(640.0, 480.0);

// ---- the fake core -----------------------------------------------------

#[derive(Clone)]
enum Script {
    Refuse(FeatureError),
    /// The reply is lost and the command was not applied.
    Lost,
    /// The reply is lost but the command was applied.
    LostApplied,
    /// Accepted, and the operation stays `Running` until released.
    Hold,
    Fail(RendererFailure),
    Cancel(RendererFailure),
}

fn kind_of(command: &Command) -> &'static str {
    match command {
        Command::Artifacts(ArtifactsCommand::OpenPreview { .. }) => "open",
        Command::Browser(command) => match command {
            BrowserCommand::CreateRendererSession {} => "create",
            BrowserCommand::ResumeRendererSession { .. } => "resume",
            BrowserCommand::ResizeRendererTab { .. } => "resize",
            BrowserCommand::SetRendererStreaming { .. } => "streaming",
            BrowserCommand::RendererCdp { .. } => "cdp",
            BrowserCommand::TakeRendererNotices { .. } => "notices",
            BrowserCommand::CaptureRendererTab { .. } => "capture",
            BrowserCommand::CloseRendererTab { .. } => "close",
            BrowserCommand::SubscribeRendererFrames { .. } => "subscribe",
            BrowserCommand::UnsubscribeRendererFrames { .. } => "unsubscribe",
            _ => "other",
        },
        _ => "other",
    }
}

fn rid(n: u64) -> RendererId {
    RendererId::new(format!("{n:032x}")).unwrap()
}

struct Core {
    epoch: u64,
    generation: u64,
    serial: u64,
    token: u64,
    session: Option<RendererSessionRef>,
    tabs: Vec<RendererTab>,
    detached: Vec<RendererTab>,
    states: BTreeMap<String, RendererOperationState>,
    held: BTreeMap<String, RendererOutput>,
    scripts: HashMap<&'static str, VecDeque<Script>>,
    commands: Vec<Command>,
    queries: Vec<Query>,
    starts: Vec<(u64, DataSource)>,
    cancels: Vec<u64>,
    keys: HashMap<u64, DataKey>,
    capture: Vec<u8>,
    capture_len: Option<u64>,
    notices: (Vec<String>, Vec<String>),
    cdp_result: Value,
    /// Operation queries that arrived while the session was detached.
    queried_detached: usize,
}

impl Core {
    fn new() -> Self {
        Self {
            epoch: 7,
            generation: 2,
            serial: 0,
            token: 0,
            session: None,
            tabs: Vec::new(),
            detached: Vec::new(),
            states: BTreeMap::new(),
            held: BTreeMap::new(),
            scripts: HashMap::new(),
            commands: Vec::new(),
            queries: Vec::new(),
            starts: Vec::new(),
            cancels: Vec::new(),
            keys: HashMap::new(),
            capture: png(8, 4),
            capture_len: None,
            notices: (Vec::new(), Vec::new()),
            cdp_result: json!({"ok": true}),
            queried_detached: 0,
        }
    }

    fn id(&mut self) -> RendererId {
        self.serial += 1;
        rid(self.serial)
    }

    fn count(&self, kind: &str) -> usize {
        self.commands.iter().filter(|c| kind_of(c) == kind).count()
    }

    fn script(&mut self, kind: &'static str, script: Script) {
        self.scripts.entry(kind).or_default().push_back(script);
    }

    fn script_n(&mut self, kind: &'static str, script: Script, n: usize) {
        for _ in 0..n {
            self.script(kind, script.clone());
        }
    }

    fn tab_mut(&mut self, tab: &RendererTabRef) -> Option<&mut RendererTab> {
        self.tabs.iter_mut().find(|t| &t.reference == tab)
    }

    fn apply(&mut self, command: &Command) -> RendererOutput {
        match command {
            Command::Browser(BrowserCommand::CreateRendererSession {}) => {
                let session = RendererSessionRef {
                    core_epoch: self.epoch,
                    id: self.id(),
                };
                self.session = Some(session.clone());
                self.token += 1;
                RendererOutput::Session {
                    session,
                    resume_token: rid(1_000 + self.token),
                }
            }
            Command::Browser(BrowserCommand::ResumeRendererSession { session, .. }) => {
                self.tabs.append(&mut self.detached);
                self.token += 1;
                RendererOutput::Session {
                    session: session.clone(),
                    resume_token: rid(1_000 + self.token),
                }
            }
            Command::Artifacts(ArtifactsCommand::OpenPreview {
                artifact,
                session,
                viewport,
            }) => {
                let tab = RendererTab {
                    reference: RendererTabRef {
                        core_epoch: self.epoch,
                        generation: self.generation,
                        id: self.id(),
                    },
                    session: session.clone(),
                    owner: RendererOwnerKind::ClientPreview,
                    artifact: artifact.clone(),
                    viewport: *viewport,
                    fps: 30,
                    streaming: false,
                };
                self.tabs.push(tab.clone());
                RendererOutput::Tab(Box::new(tab))
            }
            Command::Browser(BrowserCommand::SubscribeRendererFrames { tab }) => {
                RendererOutput::Subscription {
                    subscription: self.id(),
                    token: self.id(),
                    tab: tab.clone(),
                }
            }
            Command::Browser(BrowserCommand::SetRendererStreaming { tab, enabled }) => {
                if let Some(t) = self.tab_mut(tab) {
                    t.streaming = *enabled;
                }
                RendererOutput::Done
            }
            Command::Browser(BrowserCommand::ResizeRendererTab { tab, viewport }) => {
                if let Some(t) = self.tab_mut(tab) {
                    t.viewport = *viewport;
                }
                RendererOutput::Done
            }
            Command::Browser(BrowserCommand::RendererCdp { .. }) => RendererOutput::Cdp {
                result: self.cdp_result.clone(),
            },
            Command::Browser(BrowserCommand::CaptureRendererTab { .. }) => {
                RendererOutput::Capture {
                    capture_id: self.id(),
                    byte_len: self.capture_len.unwrap_or(self.capture.len() as u64),
                }
            }
            Command::Browser(BrowserCommand::TakeRendererNotices { .. }) => {
                RendererOutput::Notices {
                    blocked: self.notices.0.clone(),
                    dialogs: self.notices.1.clone(),
                }
            }
            Command::Browser(BrowserCommand::CloseRendererTab { tab }) => {
                self.tabs.retain(|t| &t.reference != tab);
                RendererOutput::Done
            }
            _ => RendererOutput::Done,
        }
    }

    fn command(&mut self, command: &Command) -> FeatureReply {
        self.commands.push(command.clone());
        let script = self
            .scripts
            .get_mut(kind_of(command))
            .and_then(VecDeque::pop_front);
        match &script {
            Some(Script::Refuse(error)) => return FeatureReply::Command(Err(error.clone())),
            Some(Script::Lost) => {
                return FeatureReply::Command(Err(FeatureError::outcome_unknown("lost")));
            }
            _ => {}
        }
        let output = self.apply(command);
        let operation = self.id();
        let state = match script {
            Some(Script::Hold) => {
                self.held.insert(operation.as_str().to_owned(), output);
                RendererOperationState::Running
            }
            Some(Script::Fail(reason)) => RendererOperationState::Failed { reason },
            Some(Script::Cancel(reason)) => RendererOperationState::Cancelled { reason },
            _ => RendererOperationState::Completed { output },
        };
        let lost = matches!(script, Some(Script::LostApplied));
        self.states.insert(operation.as_str().to_owned(), state);
        if lost {
            return FeatureReply::Command(Err(FeatureError::outcome_unknown("lost")));
        }
        let result = match command {
            Command::Artifacts(_) => CommandResult::Artifacts(ArtifactsResult::Accepted {
                operation_id: operation,
            }),
            _ => CommandResult::Browser(BrowserResult::Accepted {
                operation_id: operation,
            }),
        };
        FeatureReply::Command(Ok(CommandReply::with_result(result)))
    }

    fn query(&mut self, query: &Query) -> FeatureReply {
        self.queries.push(query.clone());
        let reply = match query {
            Query::Browser(BrowserQuery::GetRendererOperation { operation_id }) => {
                if !self.detached.is_empty() {
                    // A detached session answers nothing about its operations.
                    self.queried_detached += 1;
                    return FeatureReply::Query(Err(
                        FeatureError::new("session is detached").with_code(ErrorCode::NotFound)
                    ));
                }
                let state = self.states[operation_id.as_str()].clone();
                QueryReply::Browser(BrowserReply::Operation(RendererOperation {
                    operation_id: operation_id.clone(),
                    state,
                }))
            }
            Query::Browser(BrowserQuery::ListRendererTabs { .. }) => {
                QueryReply::Browser(BrowserReply::Tabs {
                    tabs: self.tabs.clone(),
                    more: false,
                })
            }
            other => panic!("unexpected query {other:?}"),
        };
        FeatureReply::Query(Ok(reply))
    }
}

// ---- the rig -----------------------------------------------------------

struct State {
    feature: BrowserFeature,
    selection: Selection,
    cache: Cache,
    frame: FeatureFrame,
    inbox: DataInbox,
}

struct Rig {
    harness: Harness<'static, State>,
    core: Core,
    base: Instant,
    now_ms: Arc<AtomicU64>,
}

fn artifact(n: u64) -> ArtifactRef {
    ArtifactRef {
        project: ProjectId::new("p").unwrap(),
        thread: ThreadId::new("t").unwrap(),
        artifact_id: format!("artifact-{n:032x}"),
        digest: "a".repeat(64),
    }
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::new();
    image::RgbaImage::new(w, h)
        .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
        .unwrap();
    out
}

fn bgra_frame(tab: &RendererTab, sequence: u64, w: u32, h: u32) -> Frame {
    Frame {
        start: FrameStart {
            core_epoch: tab.reference.core_epoch,
            generation: tab.reference.generation,
            tab: tab.reference.id.clone(),
            sequence,
            width: w,
            height: h,
            stride: w * 4,
            total_bytes: u64::from(w * h * 4),
        },
        bgra: vec![sequence as u8; (w * h * 4) as usize],
    }
}

fn busy(advice: RetryAdvice) -> FeatureError {
    FeatureError::new("busy")
        .with_code(ErrorCode::Busy)
        .with_retry(advice)
}

impl Rig {
    fn new() -> Self {
        let base = Instant::now();
        let now_ms = Arc::new(AtomicU64::new(0));
        let clock = {
            let now_ms = now_ms.clone();
            Clock::from_fn(move || base + Duration::from_millis(now_ms.load(Ordering::SeqCst)))
        };
        let state = State {
            feature: BrowserFeature::with_clock(clock),
            selection: Selection {
                project: Some(ProjectId::new("p").unwrap()),
                thread: None,
            },
            cache: Cache::default(),
            frame: FeatureFrame::default(),
            inbox: DataInbox::default(),
        };
        let mut harness = Harness::builder().with_size(SIZE).build_ui_state(
            |ui, state: &mut State| {
                let capabilities = [Capability::Queries, Capability::Commands];
                let view = FeatureView {
                    connected: true,
                    capabilities: &capabilities,
                    cache: &state.cache,
                    selection: &state.selection,
                };
                let mut ctx = FeatureContext {
                    data: &mut state.inbox,
                    feature: FeatureId::Browser,
                    view,
                    frame: &mut state.frame,
                };
                state.feature.render(ui, &mut ctx);
            },
            state,
        );
        harness.run_steps(1);
        Self {
            harness,
            core: Core::new(),
            base,
            now_ms,
        }
    }

    fn feature(&mut self) -> &mut BrowserFeature {
        &mut self.harness.state_mut().feature
    }

    fn preview(&self, index: usize) -> &Preview {
        &self.harness.state().feature.previews[index]
    }

    fn advance(&self, by: Duration) {
        self.now_ms
            .fetch_add(by.as_millis() as u64, Ordering::SeqCst);
    }

    fn now(&self) -> Instant {
        self.base + Duration::from_millis(self.now_ms.load(Ordering::SeqCst))
    }

    /// Renders and answers what the feature asked until it asks for nothing.
    fn pump(&mut self) {
        for _ in 0..60 {
            self.harness.run_steps(2);
            let (actions, data) = {
                let frame = &mut self.harness.state_mut().frame;
                (
                    std::mem::take(&mut frame.actions),
                    std::mem::take(&mut frame.data),
                )
            };
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

    fn publish(&mut self, tag: u64, payload: DataPayload) -> bool {
        let key = self.core.keys[&tag];
        let state = self.harness.state_mut();
        let stored = state.inbox.publish(key, payload);
        state.feature.data(tag, DataNotice::Ready);
        stored
    }

    fn reset(&mut self) {
        self.core.keys.clear();
        let state = self.harness.state_mut();
        state.inbox.reset(FeatureId::Browser);
        state.feature.reset();
    }

    fn release(&mut self, operation: &str) {
        let output = self.core.held.remove(operation).unwrap();
        self.core.states.insert(
            operation.to_owned(),
            RendererOperationState::Completed { output },
        );
        let id = rid(u64::from_str_radix(operation, 16).unwrap());
        self.feature()
            .event(&UiEvent::Browser(BrowserEvent::OperationChanged { id }));
    }

    /// A preview opened and streaming; returns its data tag.
    fn open(&mut self, n: u64) -> u64 {
        self.feature().open_preview(artifact(n));
        self.pump();
        self.live_tag(self.harness.state().feature.previews.len() - 1)
    }

    fn live_tag(&self, index: usize) -> u64 {
        match &self.preview(index).stream {
            Stream::Live { tag, .. } => *tag,
            other => panic!("not live: {other:?} / {:?}", self.preview(index).phase),
        }
    }

    fn tab(&self, index: usize) -> RendererTab {
        self.preview(index).tab.clone().expect("an open tab")
    }

    /// Waits for a decode that is still running (it may be done already: the
    /// render only polls), then lets the feature take the result.
    fn finish_decode(&mut self) {
        if let Capture::Decoding(job) = &self.preview(0).capture {
            job.wait_ready();
        }
        self.pump();
    }

    fn request(&mut self, request: UiRequest) {
        self.feature().requests.push(request);
        self.pump();
    }

    fn cdp_methods(&self) -> Vec<String> {
        self.core
            .commands
            .iter()
            .filter_map(|c| match c {
                Command::Browser(BrowserCommand::RendererCdp { params, .. }) => {
                    Some(params["type"].as_str().unwrap_or("").to_owned())
                }
                _ => None,
            })
            .collect()
    }
}

// ---- input ---------------------------------------------------------------

fn surface(rect: Rect, css: egui::Vec2) -> Surface {
    Surface {
        rect,
        css,
        hovered: true,
        focused: true,
        time: 1.0,
        hover_pos: None,
    }
}

fn drain(queue: &mut InputQueue) -> Vec<Input> {
    std::iter::from_fn(|| queue.pop()).collect()
}

fn key_event(key: Key, pressed: bool) -> Event {
    Event::Key {
        key,
        physical_key: Some(key),
        pressed,
        repeat: false,
        modifiers: Modifiers::NONE,
    }
}

fn click(pos: egui::Pos2, pressed: bool) -> Event {
    Event::PointerButton {
        pos,
        button: PointerButton::Primary,
        pressed,
        modifiers: Modifiers::NONE,
    }
}

#[test]
fn pointer_positions_follow_the_rectangle_and_the_css_size() {
    let rect = Rect::from_min_size(pos2(10.0, 20.0), vec2(200.0, 100.0));
    let mut translator = Translator::default();
    let mut queue = InputQueue::default();
    let events = [Event::PointerMoved(pos2(110.0, 70.0))];
    translator.translate(&events, &surface(rect, vec2(400.0, 200.0)), &mut queue);
    // The page is laid out at twice the points: the middle stays the middle.
    assert_eq!(
        drain(&mut queue),
        vec![Input::Move {
            x: 200.0,
            y: 100.0,
            buttons: 0
        }]
    );
    // A resize changes the ratio, not the point on screen.
    translator.translate(&events, &surface(rect, vec2(800.0, 400.0)), &mut queue);
    assert_eq!(
        drain(&mut queue),
        vec![Input::Move {
            x: 400.0,
            y: 200.0,
            buttons: 0
        }]
    );
    // A drag that leaves the area stays inside the page.
    translator.translate(
        &[
            click(pos2(20.0, 30.0), true),
            Event::PointerMoved(pos2(900.0, -50.0)),
        ],
        &surface(rect, vec2(200.0, 100.0)),
        &mut queue,
    );
    let inputs = drain(&mut queue);
    assert!(matches!(
        inputs.last(),
        Some(Input::Move { x, y, buttons: 1 }) if *x == 200.0 && *y == 0.0
    ));
}

#[test]
fn a_denser_display_does_not_change_the_page_coordinates() {
    // egui positions are points whatever the pixels per point; the page size
    // is the area in points, so the same point gives the same coordinate.
    for ppp in [1.0f32, 1.5, 2.0] {
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(300.0, 150.0));
        let mut translator = Translator::default();
        let mut queue = InputQueue::default();
        let mut s = surface(rect, vec2(300.0, 150.0));
        s.time = f64::from(ppp);
        translator.translate(&[Event::PointerMoved(pos2(75.0, 75.0))], &s, &mut queue);
        assert_eq!(
            drain(&mut queue),
            vec![Input::Move {
                x: 75.0,
                y: 75.0,
                buttons: 0
            }]
        );
    }
}

#[test]
fn press_and_release_keep_their_order_while_moves_are_merged() {
    let mut queue = InputQueue::default();
    let mv = |x| Input::Move {
        x,
        y: 0.0,
        buttons: 0,
    };
    let press = Input::Press {
        x: 1.0,
        y: 1.0,
        button: Button::Left,
        buttons: 1,
        modifiers: 0,
        count: 1,
    };
    let release = Input::Release {
        x: 1.0,
        y: 1.0,
        button: Button::Left,
        buttons: 0,
        modifiers: 0,
        count: 1,
    };
    for input in [
        mv(1.0),
        mv(2.0),
        press.clone(),
        mv(3.0),
        mv(4.0),
        release.clone(),
    ] {
        queue.push(input);
    }
    assert_eq!(drain(&mut queue), vec![mv(2.0), press, mv(4.0), release]);
}

#[test]
fn the_queue_is_bounded_and_a_dropped_press_drops_its_release() {
    let mut queue = InputQueue::default();
    for i in 0..1_000 {
        queue.push(Input::Char {
            text: i.to_string(),
            modifiers: 0,
        });
    }
    assert!(queue.len() <= 128 + 32, "{}", queue.len());
    assert!(queue.dropped() >= 800);
    queue.push(Input::Press {
        x: 0.0,
        y: 0.0,
        button: Button::Left,
        buttons: 1,
        modifiers: 0,
        count: 1,
    });
    queue.push(Input::Release {
        x: 0.0,
        y: 0.0,
        button: Button::Left,
        buttons: 0,
        modifiers: 0,
        count: 1,
    });
    assert!(
        drain(&mut queue)
            .iter()
            .all(|i| !matches!(i, Input::Press { .. } | Input::Release { .. })),
        "a release without its press would leave a button down in the page"
    );
}

#[test]
fn losing_focus_releases_what_is_held() {
    let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 100.0));
    let mut translator = Translator::default();
    let mut queue = InputQueue::default();
    translator.translate(
        &[click(pos2(5.0, 5.0), true), key_event(Key::A, true)],
        &surface(rect, vec2(100.0, 100.0)),
        &mut queue,
    );
    drain(&mut queue);
    translator.release_all(&mut queue);
    let inputs = drain(&mut queue);
    assert!(inputs.iter().any(|i| matches!(i, Input::Release { .. })));
    assert!(inputs.iter().any(|i| matches!(i, Input::KeyUp { .. })));
}

#[test]
fn wheel_steps_are_merged_and_scroll_the_other_way() {
    let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 100.0));
    let mut translator = Translator::default();
    let mut queue = InputQueue::default();
    let wheel = |dy| Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: vec2(0.0, dy),
        modifiers: Modifiers::NONE,
        phase: egui::TouchPhase::Move,
    };
    translator.translate(
        &[wheel(10.0), wheel(10.0), wheel(10.0)],
        &surface(rect, vec2(100.0, 100.0)),
        &mut queue,
    );
    let inputs = drain(&mut queue);
    assert_eq!(inputs.len(), 1);
    assert!(matches!(&inputs[0], Input::Wheel { dy, .. } if *dy == -30.0));
}

#[test]
fn ime_composition_withholds_keys_and_the_committing_enter() {
    let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 100.0));
    let s = surface(rect, vec2(100.0, 100.0));
    let mut translator = Translator::default();
    let mut queue = InputQueue::default();
    translator.translate(
        &[
            Event::Ime(egui::ImeEvent::Preedit {
                text: "にほ".to_owned(),
                active_range_chars: None,
            }),
            key_event(Key::ArrowLeft, true),
            key_event(Key::ArrowLeft, false),
        ],
        &s,
        &mut queue,
    );
    assert!(translator.composing());
    assert_eq!(
        drain(&mut queue),
        vec![Input::ImeSet {
            text: "にほ".to_owned(),
            start: 2,
            end: 2
        }]
    );
    translator.translate(
        &[
            Event::Ime(egui::ImeEvent::Commit("日本".to_owned())),
            key_event(Key::Enter, true),
            key_event(Key::Enter, false),
        ],
        &s,
        &mut queue,
    );
    assert!(!translator.composing());
    assert_eq!(
        drain(&mut queue),
        vec![Input::Insert {
            text: "日本".to_owned()
        }]
    );
    translator.translate(&[key_event(Key::Enter, true)], &s, &mut queue);
    assert!(
        drain(&mut queue)
            .iter()
            .any(|i| matches!(i, Input::KeyDown { .. })),
        "an Enter after the commit is the page's again"
    );
}

#[test]
fn an_emptied_composition_is_cleared_in_the_page() {
    let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 100.0));
    let s = surface(rect, vec2(100.0, 100.0));
    let mut translator = Translator::default();
    let mut queue = InputQueue::default();
    let preedit = |text: &str| {
        Event::Ime(egui::ImeEvent::Preedit {
            text: text.to_owned(),
            active_range_chars: None,
        })
    };
    translator.translate(&[preedit("a"), preedit("")], &s, &mut queue);
    let inputs = drain(&mut queue);
    assert!(matches!(
        inputs.last(),
        Some(Input::ImeSet { text, .. }) if text.is_empty()
    ));
    assert!(!translator.composing());
}

fn allowed_cdp_methods() -> String {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets the manifest dir");
    let path = std::path::Path::new(&manifest).join("../renderer-ipc/src/protocol.rs");
    let source = std::fs::read_to_string(path).expect("the renderer protocol source");
    let start = source.find("CDP_ALLOWED_METHODS").expect("the allowlist");
    source[start..].chars().take(6_000).collect()
}

#[test]
fn everything_sent_is_a_valid_command_of_the_cdp_allowlist() {
    let allow = allowed_cdp_methods();
    let key = input::KeyInfo {
        key: "a".to_owned(),
        code: "KeyA",
        vk: 65,
    };
    let inputs = [
        Input::Move {
            x: 1.0,
            y: 2.0,
            buttons: 0,
        },
        Input::Press {
            x: 1.0,
            y: 2.0,
            button: Button::Right,
            buttons: 2,
            modifiers: 0,
            count: 1,
        },
        Input::Wheel {
            x: 1.0,
            y: 2.0,
            dx: 0.0,
            dy: 5.0,
            modifiers: 0,
        },
        Input::KeyDown {
            key: key.clone(),
            modifiers: 0,
        },
        Input::KeyUp { key, modifiers: 0 },
        Input::Char {
            text: "a".to_owned(),
            modifiers: 0,
        },
        Input::ImeSet {
            text: "あ".to_owned(),
            start: 1,
            end: 1,
        },
        Input::Insert {
            text: "あ".to_owned(),
        },
    ];
    let mut methods: Vec<(&str, Value)> = inputs.iter().map(Input::cdp).collect();
    methods.extend(InspectKind::ALL.iter().map(|k| (k.method(), k.params())));
    let tab = RendererTabRef {
        core_epoch: 1,
        generation: 1,
        id: rid(1),
    };
    for (method, params) in methods {
        assert!(allow.contains(&format!("\"{method}\"")), "{method}");
        BrowserCommand::RendererCdp {
            tab: tab.clone(),
            method: method.to_owned(),
            params,
        }
        .validate()
        .unwrap();
    }
}

// ---- pixels -------------------------------------------------------------

fn frame_of(w: u32, h: u32, stride: u32, bgra: Vec<u8>) -> Frame {
    Frame {
        start: FrameStart {
            core_epoch: 1,
            generation: 1,
            tab: rid(1),
            sequence: 1,
            width: w,
            height: h,
            stride,
            total_bytes: bgra.len() as u64,
        },
        bgra,
    }
}

#[test]
fn bgra_becomes_rgba_and_the_row_padding_is_dropped() {
    // Two rows of two pixels with 4 bytes of padding after each row.
    let mut bgra = Vec::new();
    for row in 0..2u8 {
        for px in 0..2u8 {
            bgra.extend_from_slice(&[10 + px, 20 + row, 30, 255]);
        }
        bgra.extend_from_slice(&[0xEE; 4]);
    }
    let image = bgra_to_image(&frame_of(2, 2, 12, bgra)).unwrap();
    assert_eq!(image.size, [2, 2]);
    assert_eq!(
        image.pixels[3],
        egui::Color32::from_rgba_premultiplied(30, 21, 11, 255)
    );
    assert_eq!(
        image.pixels[2],
        egui::Color32::from_rgba_premultiplied(30, 21, 10, 255)
    );
}

#[test]
fn a_frame_that_does_not_fit_its_size_is_refused() {
    assert_eq!(
        bgra_to_image(&frame_of(2, 2, 4, vec![0; 16])).unwrap_err(),
        PixelError::Stride
    );
    assert_eq!(
        bgra_to_image(&frame_of(2, 2, 8, vec![0; 15])).unwrap_err(),
        PixelError::Short
    );
    assert_eq!(
        bgra_to_image(&frame_of(0, 2, 8, vec![0; 16])).unwrap_err(),
        PixelError::Size
    );
    assert_eq!(
        bgra_to_image(&frame_of(3841, 1, 3841 * 4, vec![0; 3841 * 4])).unwrap_err(),
        PixelError::Size
    );
    // The last row needs no padding after it.
    assert!(bgra_to_image(&frame_of(1, 2, 8, vec![0; 12])).is_ok());
}

#[test]
fn a_capture_larger_than_the_limit_is_not_decoded() {
    assert_eq!(decode_capture(&png(8, 4)).unwrap().size, [8, 4]);
    assert!(matches!(
        decode_capture(&png(3841, 1)),
        Err(PixelError::Decode(_))
    ));
    assert!(matches!(
        decode_capture(b"not an image"),
        Err(PixelError::Decode(_))
    ));
}

#[test]
fn shown_text_is_bounded_on_character_boundaries() {
    let (text, cut) = bounded(&"あ".repeat(100), 10);
    assert!(cut);
    assert_eq!(text, "あ".repeat(3));
    assert_eq!(bounded("short", 10), ("short".to_owned(), false));
    let lines: Vec<String> = (0..1_000).map(|_| "x".repeat(10_000)).collect();
    let (shown, cut) = bounded_lines(&lines);
    assert!(cut);
    assert_eq!(shown.len(), MAX_NOTICES);
    assert!(shown.iter().all(|l| l.len() <= preview::MAX_NOTICE_CHARS));
}

// ---- the flow ------------------------------------------------------------

#[test]
fn a_preview_goes_through_session_open_query_subscribe_and_streaming() {
    let mut rig = Rig::new();
    rig.open(1);
    assert_eq!(rig.preview(0).phase, Phase::Open);
    let kinds: Vec<_> = rig.core.commands.iter().map(kind_of).collect();
    assert_eq!(kinds, ["create", "open", "subscribe", "streaming"]);
    // The operation was asked for, not assumed from `Accepted`.
    assert!(
        rig.core
            .queries
            .iter()
            .any(|q| matches!(q, Query::Browser(BrowserQuery::GetRendererOperation { .. })))
    );
    assert!(
        rig.core
            .starts
            .iter()
            .all(|(_, s)| matches!(s, DataSource::Frames { .. }))
    );
    assert_eq!(rig.core.starts.len(), 1);
}

#[test]
fn accepted_is_not_completion() {
    let mut rig = Rig::new();
    rig.core.script("open", Script::Hold);
    rig.feature().open_preview(artifact(1));
    rig.pump();
    assert!(matches!(rig.preview(0).phase, Phase::Opening(_)));
    assert_eq!(rig.core.count("subscribe"), 0);
    // The result arrives with the event that says the operation changed.
    let operation = rig.core.held.keys().next().unwrap().clone();
    rig.release(&operation);
    rig.pump();
    assert_eq!(rig.preview(0).phase, Phase::Open);
    assert_eq!(rig.core.count("subscribe"), 1);
    assert_eq!(rig.core.count("open"), 1);
}

#[test]
fn opening_the_same_artifact_again_selects_it() {
    let mut rig = Rig::new();
    rig.open(1);
    rig.feature().open_preview(artifact(1));
    rig.pump();
    assert_eq!(rig.harness.state().feature.previews.len(), 1);
    assert_eq!(rig.core.count("open"), 1);
}

#[test]
fn frames_are_shown_and_a_slow_reader_keeps_one_slot() {
    let mut rig = Rig::new();
    let tag = rig.open(1);
    let tab = rig.tab(0);
    // The reader publishes a hundred frames while the screen draws none.
    for sequence in 1..=100 {
        assert!(rig.publish(tag, DataPayload::Frame(bgra_frame(&tab, sequence, 4, 2))));
    }
    let (slots, bytes, _) = rig.harness.state().inbox.usage();
    assert_eq!((slots, bytes), (1, 32));
    rig.pump();
    assert_eq!(rig.preview(0).picture.sequence, Some(100));
    assert!(rig.preview(0).picture.texture().is_some());
    assert_eq!(rig.preview(0).shown, Some([4, 2]));
    assert_eq!(rig.harness.state().inbox.usage().1, 0);
}

#[test]
fn a_frame_of_another_epoch_or_generation_is_not_shown() {
    let mut rig = Rig::new();
    let tag = rig.open(1);
    let tab = rig.tab(0);
    let mut stale = bgra_frame(&tab, 1, 2, 2);
    stale.start.core_epoch += 1;
    rig.publish(tag, DataPayload::Frame(stale));
    rig.pump();
    assert!(rig.preview(0).picture.texture().is_none());
    let mut old = bgra_frame(&tab, 2, 2, 2);
    old.start.generation += 1;
    rig.publish(tag, DataPayload::Frame(old));
    rig.pump();
    assert!(rig.preview(0).picture.texture().is_none());
    rig.publish(tag, DataPayload::Frame(bgra_frame(&tab, 3, 2, 2)));
    rig.pump();
    assert_eq!(rig.preview(0).picture.sequence, Some(3));
}

#[test]
fn the_size_of_the_frames_may_change() {
    let mut rig = Rig::new();
    let tag = rig.open(1);
    let tab = rig.tab(0);
    rig.publish(tag, DataPayload::Frame(bgra_frame(&tab, 1, 4, 2)));
    rig.pump();
    rig.publish(tag, DataPayload::Frame(bgra_frame(&tab, 2, 8, 6)));
    rig.pump();
    assert_eq!(rig.preview(0).picture.texture().unwrap().size(), [8, 6]);
    assert_eq!(rig.preview(0).shown, Some([8, 6]));
}

#[test]
fn a_tab_that_is_not_shown_stops_its_stream_and_drops_the_picture() {
    let mut rig = Rig::new();
    let first = rig.open(1);
    let tab = rig.tab(0);
    rig.publish(first, DataPayload::Frame(bgra_frame(&tab, 1, 2, 2)));
    rig.pump();
    assert!(rig.preview(0).picture.texture().is_some());
    // A second preview takes the screen.
    rig.open(2);
    assert!(rig.core.cancels.contains(&first));
    assert!(rig.preview(0).picture.texture().is_none());
    assert_eq!(rig.preview(0).stream, Stream::Off);
    assert_eq!(rig.core.count("unsubscribe"), 1);
    // Only the selected tab keeps a slot.
    assert_eq!(rig.harness.state().inbox.usage().0, 1);
    let streaming_off = rig.core.commands.iter().any(|c| {
        matches!(
            c,
            Command::Browser(BrowserCommand::SetRendererStreaming { enabled: false, .. })
        )
    });
    assert!(streaming_off);
    // Going back starts a fresh subscription after the cool-down.
    let id = rig.preview(0).id;
    rig.feature().selected = Some(id);
    rig.advance(Duration::from_secs(1));
    rig.pump();
    assert!(matches!(rig.preview(0).stream, Stream::Live { .. }));
    assert_eq!(rig.core.count("subscribe"), 3);
}

#[test]
fn a_stream_that_ends_or_fails_drops_the_picture_and_gives_up_after_a_few_tries() {
    let mut rig = Rig::new();
    let tag = rig.open(1);
    let tab = rig.tab(0);
    rig.publish(tag, DataPayload::Frame(bgra_frame(&tab, 1, 2, 2)));
    rig.pump();
    rig.feature()
        .data(tag, DataNotice::Failed(FeatureError::new("reader failed")));
    assert!(rig.preview(0).picture.texture().is_none());
    rig.pump();
    for _ in 0..20 {
        rig.advance(Duration::from_secs(10));
        rig.pump();
        if let Stream::Live { tag, .. } = rig.preview(0).stream.clone() {
            rig.feature().data(tag, DataNotice::Completed);
        }
    }
    assert!(matches!(rig.preview(0).stream, Stream::Blocked(_)));
    assert!(rig.core.count("subscribe") <= 1 + usize::from(MAX_STREAM_ATTEMPTS));
}

// ---- busy and unknown ----------------------------------------------------

#[test]
fn busy_is_retried_only_as_advised_and_only_a_few_times() {
    let mut rig = Rig::new();
    rig.core
        .script_n("open", Script::Refuse(busy(RetryAdvice::SameRequest)), 20);
    rig.feature().open_preview(artifact(1));
    for _ in 0..20 {
        rig.pump();
        rig.advance(Duration::from_secs(10));
    }
    assert!(matches!(rig.preview(0).phase, Phase::Lost(_)));
    assert_eq!(
        rig.core.count("open"),
        1 + usize::from(ops::MAX_AUTO_RETRIES)
    );
}

#[test]
fn busy_without_the_advice_to_repeat_is_not_repeated() {
    for advice in [RetryAdvice::Never, RetryAdvice::AfterResync] {
        let mut rig = Rig::new();
        rig.core.script_n("open", Script::Refuse(busy(advice)), 5);
        rig.feature().open_preview(artifact(1));
        for _ in 0..5 {
            rig.pump();
            rig.advance(Duration::from_secs(10));
        }
        assert_eq!(rig.core.count("open"), 1, "{advice:?}");
        assert!(matches!(rig.preview(0).phase, Phase::Lost(_)));
    }
}

#[test]
fn a_failed_subscription_follows_the_advice_and_waits_for_the_user_otherwise() {
    let mut rig = Rig::new();
    rig.core
        .script("subscribe", Script::Refuse(busy(RetryAdvice::AfterResync)));
    rig.feature().open_preview(artifact(1));
    rig.pump();
    for _ in 0..5 {
        rig.advance(Duration::from_secs(10));
        rig.pump();
    }
    assert!(matches!(rig.preview(0).stream, Stream::Blocked(_)));
    assert_eq!(rig.core.count("subscribe"), 1);
    let id = rig.preview(0).id;
    rig.request(UiRequest::Retry(id));
    assert!(matches!(rig.preview(0).stream, Stream::Live { .. }));
    assert_eq!(rig.core.count("subscribe"), 2);

    let mut rig = Rig::new();
    rig.core.script_n(
        "subscribe",
        Script::Refuse(busy(RetryAdvice::SameRequest)),
        20,
    );
    rig.feature().open_preview(artifact(1));
    for _ in 0..20 {
        rig.pump();
        rig.advance(Duration::from_secs(10));
    }
    assert!(matches!(rig.preview(0).stream, Stream::Blocked(_)));
    assert_eq!(
        rig.core.count("subscribe"),
        1 + usize::from(ops::MAX_AUTO_RETRIES)
    );
}

#[test]
fn an_unknown_outcome_is_never_sent_again_and_the_tab_list_settles_it() {
    // Applied, but the reply was lost: the tab is found in the list.
    let mut rig = Rig::new();
    rig.core.script("open", Script::LostApplied);
    rig.feature().open_preview(artifact(1));
    rig.pump();
    assert!(matches!(rig.preview(0).phase, Phase::Opening(_)));
    rig.advance(ops::OP_TIMEOUT + Duration::from_secs(1));
    rig.pump();
    assert_eq!(rig.preview(0).phase, Phase::Open);
    assert_eq!(rig.core.count("open"), 1);
    assert_eq!(rig.core.tabs.len(), 1);

    // Not applied: nothing is found, and nothing is opened by itself.
    let mut rig = Rig::new();
    rig.core.script("open", Script::Lost);
    rig.feature().open_preview(artifact(1));
    rig.pump();
    rig.advance(ops::OP_TIMEOUT + Duration::from_secs(1));
    rig.pump();
    assert!(matches!(rig.preview(0).phase, Phase::Lost(_)));
    rig.advance(Duration::from_secs(120));
    rig.pump();
    assert_eq!(rig.core.count("open"), 1);
    // The user opens it again.
    let id = rig.preview(0).id;
    rig.request(UiRequest::Reopen(id));
    assert_eq!(rig.core.count("open"), 2);
    assert_eq!(rig.preview(0).phase, Phase::Open);
}

#[test]
fn an_operation_that_never_finishes_ends_as_timed_out() {
    let mut rig = Rig::new();
    rig.core.script("open", Script::Hold);
    rig.feature().open_preview(artifact(1));
    rig.pump();
    rig.advance(ops::OP_TIMEOUT + Duration::from_secs(1));
    rig.pump();
    assert!(matches!(rig.preview(0).phase, Phase::Lost(_)));
    assert_eq!(rig.core.count("open"), 1);
}

// ---- helper crash and reconnection ----------------------------------------

#[test]
fn a_helper_crash_loses_the_preview_and_it_is_opened_again_only_by_the_user() {
    let mut rig = Rig::new();
    let tag = rig.open(1);
    let tab = rig.tab(0);
    rig.publish(tag, DataPayload::Frame(bgra_frame(&tab, 1, 2, 2)));
    rig.pump();
    rig.core
        .script("capture", Script::Fail(RendererFailure::Closed));
    let id = rig.preview(0).id;
    rig.request(UiRequest::Do(id, Action::Capture));
    assert!(matches!(rig.preview(0).phase, Phase::Lost(_)));
    assert!(rig.preview(0).picture.texture().is_none());
    assert!(rig.core.cancels.contains(&tag));
    rig.advance(Duration::from_secs(120));
    rig.pump();
    assert_eq!(rig.core.count("open"), 1);
    rig.request(UiRequest::Reopen(id));
    assert_eq!(rig.core.count("open"), 2);
    assert_eq!(rig.preview(0).phase, Phase::Open);
    assert!(matches!(rig.preview(0).stream, Stream::Live { .. }));
}

#[test]
fn a_reset_with_the_tab_still_there_finds_it_and_subscribes_again() {
    let mut rig = Rig::new();
    let tag = rig.open(1);
    let tab = rig.tab(0);
    rig.publish(tag, DataPayload::Frame(bgra_frame(&tab, 1, 2, 2)));
    rig.pump();
    assert!(rig.preview(0).picture.texture().is_some());
    rig.reset();
    // What came from the old connection is gone at once.
    assert!(rig.preview(0).picture.texture().is_none());
    assert_eq!(rig.preview(0).phase, Phase::Suspended);
    rig.pump();
    assert_eq!(rig.preview(0).phase, Phase::Open);
    assert_eq!(rig.core.count("resume"), 0);
    assert_eq!(rig.core.count("open"), 1);
    assert_eq!(rig.core.count("subscribe"), 2);
    assert!(matches!(rig.preview(0).stream, Stream::Live { .. }));
}

#[test]
fn a_short_disconnect_inside_the_grace_resumes_the_session() {
    let mut rig = Rig::new();
    rig.open(1);
    rig.core.detached = std::mem::take(&mut rig.core.tabs);
    rig.reset();
    rig.advance(Duration::from_secs(5));
    rig.pump();
    assert_eq!(rig.core.count("resume"), 1);
    assert_eq!(rig.preview(0).phase, Phase::Open);
    assert_eq!(rig.core.count("open"), 1);
    assert_eq!(rig.core.count("subscribe"), 2);
}

#[test]
fn after_the_grace_or_a_core_restart_nothing_is_reopened_by_itself() {
    // The grace ran out.
    let mut rig = Rig::new();
    rig.open(1);
    rig.core.detached = std::mem::take(&mut rig.core.tabs);
    rig.reset();
    rig.advance(Duration::from_secs(31));
    rig.pump();
    assert!(matches!(rig.preview(0).phase, Phase::Lost(_)));
    assert_eq!(rig.core.count("resume"), 0);
    assert_eq!(rig.core.count("open"), 1);

    // The core restarted: it does not know the session.
    let mut rig = Rig::new();
    rig.open(1);
    rig.core.tabs.clear();
    rig.core.script(
        "resume",
        Script::Refuse(FeatureError::new("no such session").with_code(ErrorCode::NotFound)),
    );
    rig.reset();
    rig.pump();
    assert_eq!(rig.core.count("resume"), 1);
    assert!(matches!(rig.preview(0).phase, Phase::Lost(_)));
    rig.advance(Duration::from_secs(120));
    rig.pump();
    assert_eq!(rig.core.count("open"), 1);
    // A new open goes through a new session.
    let id = rig.preview(0).id;
    rig.request(UiRequest::Reopen(id));
    assert_eq!(rig.preview(0).phase, Phase::Open);
    assert_eq!(rig.core.count("create"), 2);
}

#[test]
fn a_reset_in_the_middle_of_an_operation_asks_for_its_result_again() {
    let mut rig = Rig::new();
    rig.core.script("open", Script::Hold);
    rig.feature().open_preview(artifact(1));
    rig.pump();
    let operation = rig.core.held.keys().next().unwrap().clone();
    rig.core.keys.clear();
    rig.reset();
    rig.core
        .states
        .insert(operation.clone(), RendererOperationState::Running);
    rig.pump();
    let before = rig.core.queries.len();
    rig.release(&operation);
    rig.pump();
    assert!(rig.core.queries.len() > before);
    assert_eq!(rig.preview(0).phase, Phase::Open);
    assert_eq!(rig.core.count("open"), 1);
}

// ---- input through the core ----------------------------------------------

#[test]
fn input_leaves_one_at_a_time_in_the_order_it_was_made() {
    let mut rig = Rig::new();
    rig.open(1);
    let press = Input::Press {
        x: 1.0,
        y: 1.0,
        button: Button::Left,
        buttons: 1,
        modifiers: 0,
        count: 1,
    };
    let release = Input::Release {
        x: 1.0,
        y: 1.0,
        button: Button::Left,
        buttons: 0,
        modifiers: 0,
        count: 1,
    };
    {
        let queue = &mut rig.feature().previews[0].input;
        queue.push(press);
        queue.push(Input::Move {
            x: 2.0,
            y: 2.0,
            buttons: 1,
        });
        queue.push(release);
    }
    rig.harness.run_steps(1);
    let first = std::mem::take(&mut rig.harness.state_mut().frame.actions);
    assert_eq!(first.len(), 1, "one request per tab until it is answered");
    for action in first {
        rig.answer(action);
    }
    rig.pump();
    assert_eq!(
        rig.cdp_methods(),
        ["mousePressed", "mouseMoved", "mouseReleased"]
    );
    assert_eq!(
        rig.preview(0).status,
        None,
        "delivered input is not a problem"
    );
}

#[test]
fn input_is_not_sent_while_the_preview_is_not_open() {
    let mut rig = Rig::new();
    rig.open(1);
    rig.reset();
    rig.feature().previews[0].input.push(Input::Move {
        x: 1.0,
        y: 1.0,
        buttons: 0,
    });
    rig.core.detached = std::mem::take(&mut rig.core.tabs);
    rig.core.scripts.clear();
    rig.harness.run_steps(2);
    assert_eq!(rig.core.cdp_methods_sent(), 0);
}

impl Core {
    fn cdp_methods_sent(&self) -> usize {
        self.count("cdp")
    }
}

// ---- resize ---------------------------------------------------------------

#[test]
fn resizes_are_merged_and_limited() {
    let mut rig = Rig::new();
    rig.open(1);
    assert_eq!(rig.core.count("resize"), 0);
    for width in [600.0, 560.0, 520.0, 480.0, 440.0] {
        rig.harness.set_size(vec2(width, 480.0));
        rig.pump();
    }
    assert_eq!(rig.core.count("resize"), 1);
    rig.advance(Duration::from_millis(200));
    rig.pump();
    assert_eq!(rig.core.count("resize"), 2);
    let want = rig.preview(0).want_viewport.unwrap();
    let last = rig
        .core
        .commands
        .iter()
        .rev()
        .find_map(|c| match c {
            Command::Browser(BrowserCommand::ResizeRendererTab { viewport, .. }) => Some(*viewport),
            _ => None,
        })
        .unwrap();
    assert_eq!(last, want);
    last_is_valid(last);
    // Nothing more is sent while the size stays.
    rig.advance(Duration::from_secs(5));
    rig.pump();
    assert_eq!(rig.core.count("resize"), 2);
}

fn last_is_valid(viewport: RendererViewport) {
    viewport.validate().unwrap();
    assert!(viewport.width >= 64 && viewport.height >= 64);
}

// ---- results ----------------------------------------------------------------

#[test]
fn a_capture_is_taken_as_a_blob_decoded_in_the_background_and_shown() {
    let mut rig = Rig::new();
    let frames = rig.open(1);
    let id = rig.preview(0).id;
    rig.request(UiRequest::Do(id, Action::Capture));
    let (tag, source) = rig.core.starts.last().cloned().unwrap();
    assert_ne!(tag, frames);
    assert!(matches!(source, DataSource::Capture { .. }));
    assert!(matches!(rig.preview(0).capture, Capture::Receiving(Some(t)) if t == tag));
    let blob = Blob {
        start: BlobStart {
            core_epoch: 7,
            transfer_id: 1,
            source: BlobSource::Capture,
            media_type: MediaType::Png,
            total_bytes: rig.core.capture.len() as u64,
            digest: [0; 32],
        },
        bytes: rig.core.capture.clone(),
    };
    assert!(rig.publish(tag, DataPayload::Blob(blob)));
    rig.harness.run_steps(1);
    // The blob slot is given back as soon as the bytes are taken.
    for action in std::mem::take(&mut rig.harness.state_mut().frame.data) {
        rig.data_action(action);
    }
    assert!(rig.core.cancels.contains(&tag));
    assert_eq!(rig.harness.state().inbox.usage().0, 1);
    rig.finish_decode();
    assert!(matches!(rig.preview(0).capture, Capture::Idle));
    assert_eq!(
        rig.preview(0).capture_picture.texture().unwrap().size(),
        [8, 4]
    );
}

#[test]
fn a_capture_over_the_limit_or_not_an_image_is_refused() {
    let mut rig = Rig::new();
    rig.open(1);
    let id = rig.preview(0).id;
    rig.core.capture_len = Some(MAX_CAPTURE_BLOB_BYTES + 1);
    let starts = rig.core.starts.len();
    rig.request(UiRequest::Do(id, Action::Capture));
    assert!(matches!(rig.preview(0).capture, Capture::Failed(_)));
    assert_eq!(rig.core.starts.len(), starts);

    rig.core.capture_len = None;
    rig.request(UiRequest::Do(id, Action::Capture));
    let tag = rig.core.starts.last().unwrap().0;
    let blob = Blob {
        start: BlobStart {
            core_epoch: 7,
            transfer_id: 2,
            source: BlobSource::Capture,
            media_type: MediaType::Png,
            total_bytes: 12,
            digest: [0; 32],
        },
        bytes: b"not a png!!!".to_vec(),
    };
    rig.publish(tag, DataPayload::Blob(blob));
    rig.harness.run_steps(1);
    rig.finish_decode();
    assert!(matches!(rig.preview(0).capture, Capture::Failed(_)));
    assert!(rig.preview(0).capture_picture.texture().is_none());
}

#[test]
fn a_cancelled_capture_and_a_reset_drop_the_image() {
    let mut rig = Rig::new();
    rig.open(1);
    let id = rig.preview(0).id;
    rig.request(UiRequest::Do(id, Action::Capture));
    let tag = rig.core.starts.last().unwrap().0;
    let blob = Blob {
        start: BlobStart {
            core_epoch: 7,
            transfer_id: 1,
            source: BlobSource::Capture,
            media_type: MediaType::Png,
            total_bytes: rig.core.capture.len() as u64,
            digest: [0; 32],
        },
        bytes: rig.core.capture.clone(),
    };
    rig.publish(tag, DataPayload::Blob(blob));
    rig.harness.run_steps(1);
    rig.finish_decode();
    assert!(rig.preview(0).capture_picture.texture().is_some());

    // The next capture is cancelled: the earlier image is not left as its result.
    rig.core
        .script("capture", Script::Cancel(RendererFailure::TimedOut));
    rig.request(UiRequest::Do(id, Action::Capture));
    assert!(matches!(rig.preview(0).capture, Capture::Failed(_)));
    assert!(rig.preview(0).capture_picture.texture().is_none());

    // A decode in progress when the connection is replaced is discarded.
    let (job, release) = held_decode(&mut rig);
    rig.feature().previews[0].capture = Capture::Decoding(job);
    rig.reset();
    release.send(()).unwrap();
    assert!(matches!(rig.preview(0).capture, Capture::Idle));
    assert!(rig.preview(0).capture_picture.texture().is_none());
    assert!(rig.preview(0).picture.texture().is_none());
}

#[test]
fn notices_and_inspection_results_are_bounded() {
    let mut rig = Rig::new();
    rig.open(1);
    let id = rig.preview(0).id;
    rig.core.notices = (
        (0..1_000).map(|_| "b".repeat(5_000)).collect(),
        vec!["dialog".to_owned()],
    );
    rig.request(UiRequest::Do(id, Action::Notices));
    let notices = rig.preview(0).notices.clone().unwrap();
    assert_eq!(notices.blocked.len(), MAX_NOTICES);
    assert!(
        notices
            .blocked
            .iter()
            .all(|l| l.len() <= preview::MAX_NOTICE_CHARS)
    );
    assert_eq!(notices.dialogs, ["dialog"]);
    assert!(notices.truncated);

    rig.core.cdp_result = json!({ "text": "あ".repeat(100_000) });
    rig.request(UiRequest::Do(id, Action::Inspect(InspectKind::Document)));
    let inspection = rig.preview(0).inspection.clone().unwrap();
    assert!(inspection.text.len() <= MAX_RESULT_BYTES);
    assert!(inspection.truncated);
    assert!(
        rig.cdp_methods().iter().all(String::is_empty),
        "inspection is not an input"
    );
    assert!(rig.core.commands.iter().any(|c| matches!(
        c,
        Command::Browser(BrowserCommand::RendererCdp { method, .. }) if method == "DOM.getDocument"
    )));
}

#[test]
fn closing_a_preview_closes_its_tab_and_frees_its_slots() {
    let mut rig = Rig::new();
    let tag = rig.open(1);
    let id = rig.preview(0).id;
    rig.request(UiRequest::Close(id));
    assert!(rig.harness.state().feature.previews.is_empty());
    assert_eq!(rig.core.count("close"), 1);
    assert!(rig.core.tabs.is_empty());
    assert!(rig.core.cancels.contains(&tag));
    assert_eq!(rig.harness.state().inbox.usage().0, 0);
}

#[test]
fn leaving_names_only_the_tabs_this_client_opened_for_its_previews() {
    let mut rig = Rig::new();
    rig.open(1);
    rig.open(2);
    // A tab that is lost (no tab any more) names nothing.
    let lost = rig.preview(1).id;
    let index = rig.feature().index(lost).unwrap();
    rig.feature().previews[index].tab = None;
    let own = rig.tab(0);
    let commands = rig.feature().release();
    assert_eq!(commands.len(), 1, "{commands:?}");
    assert!(matches!(
        &commands[0],
        Command::Browser(BrowserCommand::CloseRendererTab { tab }) if *tab == own.reference
    ));
    // Asking does not change anything by itself: a kill sends nothing, a clean exit
    // sends exactly these.
    assert_eq!(rig.feature().previews.len(), 2);
    assert_eq!(rig.core.count("close"), 0);
}

#[test]
fn a_failed_resize_has_an_explicit_retry_that_sends_the_same_size_again() {
    let mut rig = Rig::new();
    rig.open(1);
    rig.core
        .script("resize", Script::Refuse(busy(RetryAdvice::Never)));
    rig.harness.set_size(vec2(600.0, 480.0));
    rig.pump();
    assert_eq!(rig.core.count("resize"), 1);
    assert!(rig.preview(0).resize_failed.is_some());
    rig.advance(Duration::from_secs(5));
    rig.pump();
    assert_eq!(rig.core.count("resize"), 1, "not sent again by itself");
    rig.harness.get_by_label("Retry resize").click();
    rig.harness.run_steps(2);
    rig.advance(Duration::from_millis(200));
    rig.pump();
    assert_eq!(rig.core.count("resize"), 2, "the button sends it again");
    assert!(rig.preview(0).resize_failed.is_none());
}

#[test]
fn queued_artifacts_are_bounded_and_open_preview_does_no_io() {
    let mut rig = Rig::new();
    for n in 0..100 {
        rig.feature().open_preview(artifact(n));
    }
    assert!(rig.feature().queue.len() <= MAX_QUEUED);
    assert!(rig.harness.state().frame.actions.is_empty());
    rig.pump();
    assert!(rig.harness.state().feature.previews.len() <= MAX_PREVIEWS);
    let _ = rig.now();
}

// ---- review fixes ------------------------------------------------------------

fn key_info(name: &str) -> input::KeyInfo {
    input::KeyInfo {
        key: name.to_owned(),
        code: "KeyX",
        vk: 88,
    }
}

fn key_down(name: &str) -> Input {
    Input::KeyDown {
        key: key_info(name),
        modifiers: 0,
    }
}

fn key_up(name: &str) -> Input {
    Input::KeyUp {
        key: key_info(name),
        modifiers: 0,
    }
}

fn fill_with_chars(queue: &mut InputQueue) {
    for i in 0..500 {
        queue.push(Input::Char {
            text: i.to_string(),
            modifiers: 0,
        });
    }
}

#[test]
fn a_failed_or_unknown_resize_is_not_sent_again_by_itself() {
    for script in [Script::Refuse(busy(RetryAdvice::Never)), Script::Lost] {
        let mut rig = Rig::new();
        rig.open(1);
        rig.core.script_n("resize", script, 50);
        rig.harness.set_size(vec2(500.0, 480.0));
        for _ in 0..12 {
            rig.pump();
            rig.advance(ops::OP_TIMEOUT + Duration::from_secs(1));
        }
        assert_eq!(rig.core.count("resize"), 1);
        // A new sync of the same tab does not make it a new wish.
        rig.reset();
        rig.pump();
        rig.advance(ops::OP_TIMEOUT + Duration::from_secs(1));
        rig.pump();
        assert_eq!(rig.core.count("resize"), 1);
        // Another size is the next try.
        rig.harness.set_size(vec2(420.0, 480.0));
        rig.pump();
        assert_eq!(rig.core.count("resize"), 2);
    }
}

#[test]
fn a_key_repeat_never_loses_the_release_of_a_sent_press() {
    let mut queue = InputQueue::default();
    queue.push(key_down("a"));
    fill_with_chars(&mut queue);
    for _ in 0..10_000 {
        queue.push(key_down("a"));
    }
    assert!(queue.bookkeeping() <= 2, "{}", queue.bookkeeping());
    queue.push(key_up("a"));
    assert!(
        drain(&mut queue).contains(&key_up("a")),
        "the page would keep the key down"
    );
}

#[test]
fn a_dropped_first_press_drops_its_repeats_and_its_release_with_bounded_bookkeeping() {
    let mut queue = InputQueue::default();
    fill_with_chars(&mut queue);
    for _ in 0..10_000 {
        queue.push(key_down("b"));
    }
    assert!(queue.bookkeeping() <= 1);
    queue.push(key_up("b"));
    let inputs = drain(&mut queue);
    assert!(!inputs.iter().any(|i| matches!(i, Input::KeyUp { .. })));
    assert_eq!(queue.bookkeeping(), 0);
}

#[test]
fn a_press_repeated_while_the_queue_is_full_keeps_its_release() {
    let mut queue = InputQueue::default();
    let press = |x| Input::Press {
        x,
        y: 0.0,
        button: Button::Left,
        buttons: 1,
        modifiers: 0,
        count: 1,
    };
    queue.push(press(1.0));
    fill_with_chars(&mut queue);
    queue.push(press(2.0));
    queue.push(Input::Release {
        x: 2.0,
        y: 0.0,
        button: Button::Left,
        buttons: 0,
        modifiers: 0,
        count: 1,
    });
    assert!(
        drain(&mut queue)
            .iter()
            .any(|i| matches!(i, Input::Release { .. }))
    );
}

#[test]
fn the_keys_held_at_once_are_limited_so_every_release_fits() {
    let mut queue = InputQueue::default();
    for n in 0..200 {
        queue.push(key_down(&format!("k{n}")));
    }
    for n in 0..200 {
        queue.push(key_up(&format!("k{n}")));
    }
    let inputs = drain(&mut queue);
    let downs = inputs
        .iter()
        .filter(|i| matches!(i, Input::KeyDown { .. }))
        .count();
    let ups = inputs
        .iter()
        .filter(|i| matches!(i, Input::KeyUp { .. }))
        .count();
    assert_eq!(downs, ups, "every key that went down comes up");
    assert!(queue.bookkeeping() == 0);
}

fn capture_blob(rig: &Rig, transfer_id: u64) -> DataPayload {
    DataPayload::Blob(Blob {
        start: BlobStart {
            core_epoch: 7,
            transfer_id,
            source: BlobSource::Capture,
            media_type: MediaType::Png,
            total_bytes: rig.core.capture.len() as u64,
            digest: [0; 32],
        },
        bytes: rig.core.capture.clone(),
    })
}

#[test]
fn a_second_capture_while_one_is_on_its_way_is_not_started() {
    let mut rig = Rig::new();
    rig.open(1);
    let id = rig.preview(0).id;
    rig.request(UiRequest::Do(id, Action::Capture));
    let tag = rig.core.starts.last().unwrap().0;
    let starts = rig.core.starts.len();
    rig.request(UiRequest::Do(id, Action::Capture));
    assert_eq!(rig.core.count("capture"), 1);
    assert_eq!(rig.core.starts.len(), starts);
    // The blob slot of the one capture is given back when its bytes are taken.
    let blob = capture_blob(&rig, 1);
    rig.publish(tag, blob);
    rig.harness.run_steps(1);
    for action in std::mem::take(&mut rig.harness.state_mut().frame.data) {
        rig.data_action(action);
    }
    assert_eq!(rig.core.cancels.iter().filter(|t| **t == tag).count(), 1);
    rig.finish_decode();
    assert!(matches!(rig.preview(0).capture, Capture::Idle));
    // Then it is allowed again.
    rig.request(UiRequest::Do(id, Action::Capture));
    assert_eq!(rig.core.count("capture"), 2);
}

#[test]
fn an_operation_in_flight_during_a_short_disconnect_waits_for_the_resume() {
    let mut rig = Rig::new();
    rig.open(1);
    rig.core.script("capture", Script::Hold);
    let id = rig.preview(0).id;
    rig.request(UiRequest::Do(id, Action::Capture));
    let operation = rig.core.held.keys().next().unwrap().clone();
    rig.core.detached = std::mem::take(&mut rig.core.tabs);
    rig.reset();
    rig.advance(Duration::from_secs(1));
    rig.pump();
    assert_eq!(rig.core.queried_detached, 0, "asked before the resume");
    assert_eq!(rig.core.count("resume"), 1);
    assert_eq!(rig.preview(0).phase, Phase::Open);
    // The operation is asked about again once the session is back.
    rig.release(&operation);
    rig.pump();
    assert_eq!(rig.preview(0).phase, Phase::Open);
    assert_eq!(rig.core.count("capture"), 1);
    assert_eq!(rig.core.count("open"), 1);
}

#[test]
fn what_waited_for_a_lost_preview_is_forgotten() {
    let mut rig = Rig::new();
    rig.open(1);
    rig.core.script("capture", Script::Hold);
    let id = rig.preview(0).id;
    rig.request(UiRequest::Do(id, Action::Capture));
    rig.core.detached = std::mem::take(&mut rig.core.tabs);
    rig.reset();
    rig.advance(Duration::from_secs(31));
    rig.pump();
    assert!(matches!(rig.preview(0).phase, Phase::Lost(_)));
    assert!(rig.preview(0).lane.is_none());
    let queries = rig.core.queries.len();
    rig.advance(Duration::from_secs(120));
    rig.pump();
    assert_eq!(rig.core.queries.len(), queries);
}

#[test]
fn decoder_threads_are_limited_and_count_until_they_really_end() {
    use std::sync::mpsc;
    let decoders = frame::Decoders::default();
    let ctx = egui::Context::default();
    let blocked = |decoders: &frame::Decoders| {
        let (release, wait) = mpsc::channel::<()>();
        let job = frame::DecodeJob::start_with(vec![0; 16], ctx.clone(), decoders, move |_| {
            wait.recv().ok();
            Ok(egui::ColorImage::new([1, 1], vec![egui::Color32::BLACK]))
        });
        (job, release)
    };
    let (first, release_first) = blocked(&decoders);
    let (second, release_second) = blocked(&decoders);
    assert!(first.is_some() && second.is_some());
    let (third, _) = blocked(&decoders);
    assert!(third.is_none(), "every decoder is busy");
    // A dropped job still holds its thread until the decode ends.
    drop(first);
    assert_eq!(decoders.active(), 2);
    assert!(blocked(&decoders).0.is_none());
    release_first.send(()).unwrap();
    decoders.wait_at_most(1);
    let (fourth, release_fourth) = blocked(&decoders);
    assert!(fourth.is_some());
    release_second.send(()).unwrap();
    release_fourth.send(()).unwrap();
    decoders.wait_at_most(0);
    assert_eq!(decoders.active(), 0);
    drop(second);
}

// ---- the real composition ----------------------------------------------------

/// The browser behind the shared `Features`, so the tests can look inside it.
struct Forward(Arc<std::sync::Mutex<BrowserFeature>>);

impl Forward {
    fn with<T>(&self, f: impl FnOnce(&mut BrowserFeature) -> T) -> T {
        f(&mut self.0.lock().unwrap())
    }
}

impl Feature for Forward {
    fn id(&self) -> FeatureId {
        FeatureId::Browser
    }
    fn title(&self) -> &'static str {
        "Browser"
    }
    fn availability(&self) -> Availability {
        self.with(|f| f.availability())
    }
    fn reset(&mut self) {
        self.with(|f| f.reset());
    }
    fn disconnected(&mut self) {
        self.with(|f| f.disconnected());
    }
    fn selection_changed(&mut self, ctx: &mut FeatureContext<'_>) {
        self.with(|f| f.selection_changed(ctx));
    }
    fn maintain(&mut self, ctx: &mut FeatureContext<'_>, visibility: Visibility) {
        self.with(|f| f.maintain(ctx, visibility));
    }
    fn event(&mut self, event: &UiEvent) {
        self.with(|f| f.event(event));
    }
    fn reply(&mut self, tag: u64, reply: FeatureReply) {
        self.with(|f| f.reply(tag, reply));
    }
    fn data(&mut self, tag: u64, notice: DataNotice) {
        self.with(|f| f.data(tag, notice));
    }
    fn open_preview(&mut self, artifact: ArtifactRef) {
        self.with(|f| f.open_preview(artifact));
    }
    fn render(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        self.with(|f| f.render(ui, ctx));
    }
}

struct CompState {
    features: Features,
    selection: Selection,
    cache: Cache,
    frames: Vec<FeatureFrame>,
}

struct Comp {
    harness: Harness<'static, CompState>,
    feature: Arc<std::sync::Mutex<BrowserFeature>>,
    core: Core,
    now_ms: Arc<AtomicU64>,
}

const CAPS: [Capability; 2] = [Capability::Queries, Capability::Commands];

impl Comp {
    fn new() -> Self {
        let base = Instant::now();
        let now_ms = Arc::new(AtomicU64::new(0));
        let clock = {
            let now_ms = now_ms.clone();
            Clock::from_fn(move || base + Duration::from_millis(now_ms.load(Ordering::SeqCst)))
        };
        let feature = Arc::new(std::sync::Mutex::new(BrowserFeature::with_clock(clock)));
        let state = CompState {
            features: Features {
                features: vec![Box::new(Forward(feature.clone()))],
                data: DataInbox::default(),
                selection: None,
                expanded: Default::default(),
                pins: Default::default(),
                pending_pages: Default::default(),
            },
            selection: Selection {
                project: Some(ProjectId::new("p").unwrap()),
                thread: None,
            },
            cache: Cache::default(),
            frames: Vec::new(),
        };
        let harness = Harness::builder().with_size(SIZE).build_ui_state(
            |ui, state: &mut CompState| {
                let view = FeatureView {
                    connected: true,
                    capabilities: &CAPS,
                    cache: &state.cache,
                    selection: &state.selection,
                };
                let frame = state.features.render(ui, view);
                state.frames.push(frame);
            },
            state,
        );
        Self {
            harness,
            feature,
            core: Core::new(),
            now_ms,
        }
    }

    fn with<T>(&self, f: impl FnOnce(&mut BrowserFeature) -> T) -> T {
        f(&mut self.feature.lock().unwrap())
    }

    fn advance(&self, by: Duration) {
        self.now_ms
            .fetch_add(by.as_millis() as u64, Ordering::SeqCst);
    }

    /// Answers what a frame asked for, the way the shared connection would.
    fn serve(&mut self, frame: FeatureFrame) -> bool {
        let busy = !frame.actions.is_empty() || !frame.data.is_empty();
        let state = self.harness.state_mut();
        for action in frame.data {
            let tag = action.tag;
            match &action.request {
                DataRequest::Start(source) => {
                    self.core.starts.push((tag, source.clone()));
                    match state.features.data.claim(&action, source.clone()) {
                        Ok(key) => {
                            self.core.keys.insert(tag, key);
                            state.features.data_notice(
                                FeatureId::Browser,
                                tag,
                                DataNotice::Started,
                            );
                        }
                        Err(error) => state.features.data_notice(
                            FeatureId::Browser,
                            tag,
                            DataNotice::Failed(error),
                        ),
                    }
                }
                DataRequest::Cancel => {
                    self.core.cancels.push(tag);
                    self.core.keys.remove(&tag);
                    state.features.data.remove(FeatureId::Browser, tag);
                    state
                        .features
                        .data_notice(FeatureId::Browser, tag, DataNotice::Cancelled);
                }
            }
        }
        for action in frame.actions {
            let reply = match &action.request {
                FeatureRequest::Command(command) => self.core.command(command),
                FeatureRequest::Query(query) => self.core.query(query),
            };
            state.features.reply(action.feature, action.tag, reply);
        }
        busy
    }

    /// Composes and serves until nothing more is asked.
    fn pump(&mut self) {
        for _ in 0..60 {
            self.harness.run_steps(2);
            let frames = std::mem::take(&mut self.harness.state_mut().frames);
            let mut busy = false;
            for frame in frames {
                busy |= self.serve(frame);
            }
            if !busy {
                return;
            }
        }
        panic!("the feature kept asking");
    }

    /// The native logic path of a minimized window: no render, no texture
    /// context, only the shared maintenance.
    fn pump_minimized(&mut self) {
        for _ in 0..60 {
            let state = self.harness.state_mut();
            let view = FeatureView {
                connected: true,
                capabilities: &CAPS,
                cache: &state.cache,
                selection: &state.selection,
            };
            let frame = state.features.maintain_minimized(view);
            if !self.serve(frame) {
                return;
            }
        }
        panic!("the feature kept asking");
    }

    fn open(&mut self, n: u64) -> u64 {
        self.harness.state_mut().features.open_preview(artifact(n));
        self.pump();
        self.with(|f| match &f.previews[0].stream {
            Stream::Live { tag, .. } => *tag,
            other => panic!("not live: {other:?}"),
        })
    }

    fn toggle_header(&mut self) {
        self.harness.get_by_label("Browser").click();
        self.harness.run_steps(2);
    }
}

#[test]
fn a_collapsed_area_ends_its_stream_and_a_shown_one_starts_it_again() {
    let mut comp = Comp::new();
    let tag = comp.open(1);
    assert!(comp.core.cancels.is_empty());
    comp.toggle_header();
    comp.pump();
    assert!(
        comp.core.cancels.contains(&tag),
        "the cancel reached the connection"
    );
    assert_eq!(comp.core.count("unsubscribe"), 1);
    assert_eq!(comp.core.count("streaming"), 2, "on, then off");
    comp.with(|f| {
        assert!(matches!(f.previews[0].stream, Stream::Off));
        assert!(f.previews[0].picture.texture().is_none());
    });
    assert_eq!(comp.harness.state().features.data.usage().0, 0);
    // Still collapsed: it stays off, however long it takes.
    comp.advance(Duration::from_secs(10));
    comp.pump();
    assert_eq!(comp.core.count("subscribe"), 1);
    // Expanded again: subscribed again once the cooldown is over.
    comp.toggle_header();
    comp.advance(Duration::from_secs(1));
    comp.pump();
    assert_eq!(comp.core.count("subscribe"), 2);
    comp.with(|f| assert!(matches!(f.previews[0].stream, Stream::Live { .. })));
}

#[test]
fn a_minimized_window_ends_its_stream_without_a_render() {
    let mut comp = Comp::new();
    let tag = comp.open(1);
    comp.pump_minimized();
    assert!(comp.core.cancels.contains(&tag));
    assert_eq!(comp.core.count("unsubscribe"), 1);
    comp.with(|f| assert!(matches!(f.previews[0].stream, Stream::Off)));
    // The window is back: the next render subscribes again.
    comp.advance(Duration::from_secs(1));
    comp.pump();
    assert_eq!(comp.core.count("subscribe"), 2);
}

#[test]
fn a_disconnect_drops_the_pictures_at_once() {
    let mut comp = Comp::new();
    let tag = comp.open(1);
    let frame = {
        let tab = comp.with(|f| f.previews[0].tab.clone().unwrap());
        bgra_frame(&tab, 1, 8, 4)
    };
    let key = comp.core.keys[&tag];
    comp.harness
        .state_mut()
        .features
        .data
        .publish(key, DataPayload::Frame(frame));
    comp.harness
        .state_mut()
        .features
        .data_notice(FeatureId::Browser, tag, DataNotice::Ready);
    comp.pump();
    comp.with(|f| assert!(f.previews[0].picture.texture().is_some()));
    comp.harness.state_mut().features.disconnected();
    comp.with(|f| {
        let p = &f.previews[0];
        assert!(p.picture.texture().is_none());
        assert!(matches!(p.stream, Stream::Off));
        assert!(f.data_tags.is_empty());
    });
}

// ---- decode in flight ----------------------------------------------------------

/// A decode that stays in flight until the returned sender is used.
fn held_decode(rig: &mut Rig) -> (frame::DecodeJob, std::sync::mpsc::Sender<()>) {
    let decoders = rig.feature().decoders.clone();
    let (release, wait) = std::sync::mpsc::channel::<()>();
    let job = frame::DecodeJob::start_with(
        vec![0; 16],
        egui::Context::default(),
        &decoders,
        move |_| {
            wait.recv().ok();
            Ok(egui::ColorImage::new([1, 1], vec![egui::Color32::BLACK]))
        },
    )
    .expect("a free decoder");
    (job, release)
}

#[test]
fn a_capture_asked_for_while_one_decodes_is_not_started() {
    let mut rig = Rig::new();
    rig.open(1);
    let id = rig.preview(0).id;
    let (job, release) = held_decode(&mut rig);
    rig.feature().previews[0].capture = Capture::Decoding(job);
    rig.request(UiRequest::Do(id, Action::Capture));
    assert_eq!(rig.core.count("capture"), 0);
    assert!(matches!(rig.preview(0).capture, Capture::Decoding(_)));
    release.send(()).unwrap();
    let Capture::Decoding(job) = &rig.preview(0).capture else {
        unreachable!()
    };
    job.wait_ready();
    rig.pump();
    assert!(matches!(rig.preview(0).capture, Capture::Idle));
    rig.request(UiRequest::Do(id, Action::Capture));
    assert_eq!(rig.core.count("capture"), 1);
}

#[test]
fn a_reset_or_a_closed_preview_drops_the_image_but_the_thread_counts_until_it_ends() {
    for close in [false, true] {
        let mut rig = Rig::new();
        rig.open(1);
        let id = rig.preview(0).id;
        let decoders = rig.feature().decoders.clone();
        let (job, release) = held_decode(&mut rig);
        rig.feature().previews[0].capture = Capture::Decoding(job);
        if close {
            rig.request(UiRequest::Close(id));
            assert!(rig.harness.state().feature.previews.is_empty());
        } else {
            rig.reset();
            assert!(matches!(rig.preview(0).capture, Capture::Idle));
        }
        // The image is discarded, but the thread has not ended.
        assert_eq!(decoders.active(), 1);
        release.send(()).unwrap();
        decoders.wait_at_most(0);
        assert_eq!(decoders.active(), 0);
    }
}

#[test]
fn a_capture_that_finds_every_decoder_busy_is_refused_and_gives_its_slot_back() {
    let mut rig = Rig::new();
    rig.open(1);
    let id = rig.preview(0).id;
    let decoders = rig.feature().decoders.clone();
    let held: Vec<_> = (0..frame::MAX_DECODERS)
        .map(|_| held_decode(&mut rig))
        .collect();
    rig.request(UiRequest::Do(id, Action::Capture));
    let tag = rig.core.starts.last().unwrap().0;
    let blob = capture_blob(&rig, 1);
    rig.publish(tag, blob);
    rig.harness.run_steps(1);
    for action in std::mem::take(&mut rig.harness.state_mut().frame.data) {
        rig.data_action(action);
    }
    assert!(matches!(rig.preview(0).capture, Capture::Failed(_)));
    assert!(rig.core.cancels.contains(&tag));
    assert_eq!(decoders.active(), frame::MAX_DECODERS);
    for (_job, release) in held {
        release.send(()).unwrap();
    }
    decoders.wait_at_most(0);
}

#[test]
fn a_reserve_is_not_used_up_by_new_presses() {
    let mut queue = InputQueue::default();
    for n in 0..128 {
        if n % 2 == 0 {
            queue.push(Input::Move {
                x: f64::from(n),
                y: 0.0,
                buttons: 0,
            });
        } else {
            queue.push(Input::Wheel {
                x: 0.0,
                y: 0.0,
                dx: 1.0,
                dy: 1.0,
                modifiers: n,
            });
        }
    }
    for n in 0..33 {
        queue.push(key_down(&format!("k{n}")));
        queue.push(key_up(&format!("k{n}")));
    }
    let inputs = drain(&mut queue);
    let downs = inputs
        .iter()
        .filter(|i| matches!(i, Input::KeyDown { .. }))
        .count();
    let ups = inputs
        .iter()
        .filter(|i| matches!(i, Input::KeyUp { .. }))
        .count();
    assert_eq!(downs, ups, "{downs} down, {ups} up");
    assert_eq!(queue.bookkeeping(), 0);
}

#[test]
fn a_stream_that_ended_by_itself_gives_its_slot_back() {
    let mut rig = Rig::new();
    let tag = rig.open(1);
    rig.feature().data(tag, DataNotice::Completed);
    rig.pump();
    assert!(rig.core.cancels.contains(&tag), "the slot would stay taken");
}

#[test]
fn going_back_to_a_size_that_failed_sends_the_resize_again() {
    let mut rig = Rig::new();
    rig.open(1);
    rig.core
        .script("resize", Script::Refuse(busy(RetryAdvice::Never)));
    let step = Duration::from_millis(500);
    rig.harness.set_size(vec2(500.0, 480.0));
    rig.pump();
    assert_eq!(rig.core.count("resize"), 1, "A fails");
    rig.advance(step);
    rig.harness.set_size(vec2(420.0, 480.0));
    rig.pump();
    assert_eq!(rig.core.count("resize"), 2, "B is sent");
    rig.advance(step);
    rig.harness.set_size(vec2(500.0, 480.0));
    rig.pump();
    assert_eq!(rig.core.count("resize"), 3, "A is a new wish");
}

fn has_cjk(text: &str) -> bool {
    text.chars()
        .any(|c| matches!(c, '\u{3000}'..='\u{30ff}' | '\u{3400}'..='\u{9fff}' | '\u{ff00}'..='\u{ffef}'))
}

fn assert_english(rig: &Rig) {
    for node in rig.harness.root().children_recursive() {
        let node = node.accesskit_node();
        for text in [node.label(), node.value()].into_iter().flatten() {
            let text = text.to_string();
            assert!(!has_cjk(&text), "{text}");
        }
    }
}

#[test]
fn the_preview_reads_in_english() {
    let mut rig = Rig::new();
    rig.harness.run_steps(2);
    assert!(
        rig.harness
            .query_by_label_contains("No preview is open")
            .is_some()
    );

    rig.open(1);
    let id = rig.preview(0).id;
    let title = rig.preview(0).title.clone();
    rig.core
        .script("resize", Script::Refuse(busy(RetryAdvice::Never)));
    rig.harness.set_size(vec2(600.0, 480.0));
    rig.pump();
    rig.request(UiRequest::Do(id, Action::Notices));
    rig.request(UiRequest::Do(id, Action::Inspect(InspectKind::Layout)));
    rig.harness.run_steps(2);
    for label in [
        "Capture",
        "Notices",
        "Inspect",
        "Retry resize",
        "Page notices",
        "No notices",
        "Inspection: Layout",
    ] {
        assert!(rig.harness.query_by_label(label).is_some(), "{label}");
    }
    assert!(
        rig.harness
            .query_by_label_contains("Resize failed")
            .is_some()
    );
    assert_english(&rig);

    rig.harness.get_by_label(&format!("Close {title}")).click();
    rig.harness.run_steps(2);
    rig.pump();
    assert!(rig.harness.state().feature.previews.is_empty());
    assert!(
        rig.harness
            .query_by_label_contains("No preview is open")
            .is_some()
    );
}

// ---- an open nobody took before the session or the conversation changed -------

// Given: previews asked for through `Features` (a card's Open, an artifact's preview), not yet taken
//        by the browser area
// Then: a disconnect, a new snapshot or another selected project drops them, and one asked for while
//       disconnected is gone when the session is back: no session is created and nothing is opened
#[test]
fn an_open_not_yet_taken_does_not_outlive_its_session_or_conversation() {
    let mut comp = Comp::new();
    comp.harness.state_mut().features.open_preview(artifact(1));
    comp.harness.state_mut().features.disconnected();
    comp.pump();
    comp.harness.state_mut().features.open_preview(artifact(2));
    comp.harness
        .state_mut()
        .features
        .begin_session(&[], Vec::new());
    comp.pump();
    comp.harness.state_mut().features.open_preview(artifact(3));
    comp.harness.state_mut().selection.project = Some(ProjectId::new("other").unwrap());
    comp.pump();
    assert_eq!(comp.core.count("create"), 0);
    assert_eq!(comp.core.count("open"), 0);
    comp.with(|f| {
        assert!(f.queue.is_empty());
        assert!(f.previews.is_empty());
    });
    // The same ask, with nothing in between, still opens.
    comp.harness.state_mut().selection.project = Some(ProjectId::new("p").unwrap());
    comp.pump();
    comp.harness.state_mut().features.open_preview(artifact(4));
    comp.pump();
    assert_eq!(comp.core.count("open"), 1);
}

// Given: a preview that was accepted and is waiting for its session
// Then: a new snapshot makes it lost instead of opening it later, and only an explicit reopen does
#[test]
fn a_preview_waiting_for_its_session_is_lost_when_the_snapshot_is_replaced() {
    let mut rig = Rig::new();
    rig.core.script("create", Script::Hold);
    rig.feature().open_preview(artifact(1));
    rig.pump();
    assert_eq!(rig.preview(0).phase, Phase::Waiting);
    let operation = rig.core.held.keys().next().unwrap().clone();
    rig.core.keys.clear();
    rig.reset();
    assert!(matches!(rig.preview(0).phase, Phase::Lost(_)));
    rig.core
        .states
        .insert(operation.clone(), RendererOperationState::Running);
    rig.pump();
    rig.release(&operation);
    rig.pump();
    assert_eq!(rig.core.count("open"), 0);
    assert!(matches!(rig.preview(0).phase, Phase::Lost(_)));
    let id = rig.preview(0).id;
    rig.request(UiRequest::Reopen(id));
    assert_eq!(rig.preview(0).phase, Phase::Open);
    assert_eq!(rig.core.count("open"), 1);
}
