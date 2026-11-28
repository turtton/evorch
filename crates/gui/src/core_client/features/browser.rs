//! Desktop feature of the `browser` area (Stage 16b, PR B): the preview of an
//! artifact in the renderer the core owns, its input and what it reports.
//!
//! The renderer lives in the core. This feature owns no helper process and no
//! CEF: it asks the core for a session, opens a tab for an artifact
//! (`OpenPreview`), subscribes to the tab's frames, shows them as a texture and
//! sends mouse, key and IME input back as the DevTools input methods the renderer
//! allows. Everything arrives through the shared connection: requests leave
//! through the [`FeatureContext`], replies and events come to [`Feature::reply`] and
//! [`Feature::event`], and the frames are taken from the shared latest-slot
//! (`take_data`) while rendering.
//!
//! ## Flow
//!
//! `open_preview(ArtifactRef)` (from the artifacts feature) only queues the
//! reference. A later render creates the session (once), sends `OpenPreview`,
//! and takes the tab from the operation's result: `Accepted` is not a result
//! (see [`ops`]). The visible, selected tab then subscribes to its frames
//! (`SubscribeRendererFrames`, then `start_data(DataSource::Frames)`) and turns
//! streaming on; Capture results are taken with `DataSource::Capture`.
//!
//! ## Rules
//!
//! * Receiving is the shared actor's (background); converting a BGRA frame and
//!   uploading the texture happen here, on the GUI thread, once per render.
//! * Mutations of one tab go out one at a time (subscription changes, resize,
//!   user actions, then input), so a press is never overtaken by its release. Moves,
//!   wheel steps and resizes are coalesced; the queue is bounded.
//! * A stream stops when its tab is not selected, its area is not on screen, or the
//!   whole area is not visible. [`BrowserFeature::stop_stream`] is the one place that
//!   does it; `Feature::maintain` calls it (through the drive step) while the area is
//!   collapsed, minimized or unavailable, where `render` is not called. Nothing is
//!   ingested or drawn then, and `disconnected` drops pictures at once.
//! * After a reconnect within the same core epoch the tab is found again
//!   (`ListRendererTabs`) and, inside the 30 seconds grace, the session is
//!   resumed. After a core restart, an expired grace or a helper crash the preview is
//!   `Lost` and the user opens it again; nothing is reopened or replayed by itself.
//! * A command whose outcome is unknown is not sent again; `Busy` is retried
//!   only as the core advises, a few times.
mod frame;
mod input;
pub mod observation;
mod ops;
mod preview;
mod view;

#[cfg(test)]
mod tests;

use std::{
    collections::{BTreeMap, VecDeque},
    mem,
    time::{Duration, Instant},
};

use super::{
    Availability, Feature, FeatureContext, FeatureError, FeatureId, FeatureReply, FeatureRequest,
    Visibility,
    data::{DataNotice, DataPayload, DataSource},
};
use frame::{DecodeJob, Decoders, bgra_to_image};
use ops::{Clock, Failure, Finished, OpId, Ops};
use preview::{
    Action, Capture, InspectKind, Inspection, MAX_RESULT_BYTES, Notices, Phase, Preview, PreviewId,
    Stream, bounded, bounded_lines,
};
use workbench_protocol::{
    commands::Command,
    errors::{ErrorCode, RetryAdvice},
    events::UiEvent,
    handshake::Capability,
    namespaces::{
        artifacts::{ArtifactRef, ArtifactsCommand},
        browser::{
            BrowserCommand, BrowserEvent, BrowserQuery, BrowserReply, MAX_RENDERER_TABS,
            RendererId, RendererOutput, RendererOwnerKind, RendererSessionRef, RendererTab,
            RendererViewport,
            data::{BlobSource, MAX_CAPTURE_BLOB_BYTES, MediaType},
        },
    },
    queries::{Query, QueryReply},
};

/// Previews open at once: the core's tab limit.
const MAX_PREVIEWS: usize = MAX_RENDERER_TABS;
/// Artifacts waiting for a later render; the newest are kept.
const MAX_QUEUED: usize = 8;

/// Why a preview that was asked for in another session or conversation is not opened.
const LOST_STALE_OPEN: &str = "The connection or the conversation changed before this opened";
/// A preview opens at this size and is resized to its area afterwards.
const OPEN_VIEWPORT: (u32, u32) = (960, 640);
const MIN_VIEWPORT: u32 = 64;
/// At most one resize per tab in this interval.
const RESIZE_INTERVAL: Duration = Duration::from_millis(100);
/// A stream that stopped is not started again before this.
const STREAM_COOLDOWN: Duration = Duration::from_millis(300);
/// How long the core keeps a preview session after its connection ended.
const GRACE: Duration = Duration::from_secs(30);
/// The tab list is read at most this often because of tab events.
const PROBE_INTERVAL: Duration = Duration::from_secs(1);
const MAX_STREAM_ATTEMPTS: u8 = 3;
/// Data tags are the feature's own numbers, apart from request tags.
const DATA_TAG_BASE: u64 = 1 << 32;
const PROBE_TAG_BASE: u64 = 1 << 40;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Purpose {
    CreateSession,
    ResumeSession,
    OpenPreview(PreviewId),
    Subscribe(PreviewId),
    Streaming(PreviewId, bool),
    Unsubscribe(PreviewId),
    Resize(PreviewId, RendererViewport),
    Input(PreviewId),
    Capture(PreviewId),
    Notices(PreviewId),
    Inspect(PreviewId, InspectKind),
    CloseTab(PreviewId),
}

impl Purpose {
    fn preview(&self) -> Option<PreviewId> {
        match self {
            Self::CreateSession | Self::ResumeSession => None,
            Self::OpenPreview(id)
            | Self::Subscribe(id)
            | Self::Streaming(id, _)
            | Self::Unsubscribe(id)
            | Self::Resize(id, _)
            | Self::Input(id)
            | Self::Capture(id)
            | Self::Notices(id)
            | Self::Inspect(id, _)
            | Self::CloseTab(id) => Some(*id),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DataKind {
    Frames,
    Capture,
}

struct Recovery {
    session: RendererSessionRef,
    token: RendererId,
    /// When the connection of the session was lost.
    since: Instant,
    resumed: bool,
    resuming: Option<OpId>,
    /// The resume was asked for because a tab was missing (not only because an
    /// open was in flight): a refusal then means the tab is gone.
    for_missing: bool,
    /// The tab list is to be read (again) before anything else is decided.
    probe_due: bool,
}

enum SessionState {
    None,
    Creating(OpId),
    Ready {
        session: RendererSessionRef,
        token: RendererId,
    },
    Recovering(Recovery),
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probe {
    Idle,
    Need,
    Sent(u64),
}

/// What the screen asked for while drawing; applied in the drive step, which
/// has the context to send with.
#[derive(Debug, Clone, PartialEq, Eq)]
enum UiRequest {
    Close(PreviewId),
    Reopen(PreviewId),
    Retry(PreviewId),
    Do(PreviewId, Action),
}

pub struct BrowserFeature {
    observation: observation::ObservationFeature,
    clock: Clock,
    decoders: Decoders,
    /// Data tags whose stream ended by itself: the shared slot is kept until
    /// it is cancelled, which needs a context (the next pass).
    pending_cancel: Vec<u64>,
    queue: VecDeque<ArtifactRef>,
    ops: Ops<Purpose>,
    session: SessionState,
    previews: Vec<Preview>,
    selected: Option<PreviewId>,
    next_preview: PreviewId,
    data_tags: BTreeMap<u64, (PreviewId, DataKind)>,
    next_data_tag: u64,
    probe: Probe,
    next_probe_tag: u64,
    probe_wanted: bool,
    last_probe: Option<Instant>,
    /// When the connection was first seen lost; the grace counts from here.
    lost_at: Option<Instant>,
    /// Finished operations and the tab list, held for the next render (the
    /// callbacks that deliver them have no context to send with).
    done: Vec<Finished<Purpose>>,
    tabs: Option<Result<Vec<RendererTab>, FeatureError>>,
    requests: Vec<UiRequest>,
    wake: Option<Instant>,
    notice: Option<String>,
}

impl Default for BrowserFeature {
    fn default() -> Self {
        Self::with_clock(Clock::system())
    }
}

fn merge_wake(wake: &mut Option<Instant>, at: Instant) {
    *wake = Some(wake.map_or(at, |w| w.min(at)));
}

fn backoff(attempts: u8) -> Duration {
    STREAM_COOLDOWN * 2u32.pow(u32::from(attempts.min(6)))
}

impl BrowserFeature {
    fn with_clock(clock: Clock) -> Self {
        Self {
            observation: observation::ObservationFeature::with_clock(clock.clone()),
            clock,
            decoders: Decoders::default(),
            pending_cancel: Vec::new(),
            queue: VecDeque::new(),
            ops: Ops::default(),
            session: SessionState::None,
            previews: Vec::new(),
            selected: None,
            next_preview: 0,
            data_tags: BTreeMap::new(),
            next_data_tag: DATA_TAG_BASE,
            probe: Probe::Idle,
            next_probe_tag: PROBE_TAG_BASE,
            probe_wanted: false,
            last_probe: None,
            lost_at: None,
            done: Vec::new(),
            tabs: None,
            requests: Vec::new(),
            wake: None,
            notice: None,
        }
    }

    /// What came back since the last pass: finished operations and the tab list.
    fn settle(&mut self, ctx: &mut FeatureContext<'_>) {
        let now = self.clock.now();
        for f in mem::take(&mut self.done) {
            self.on_finished(f, ctx, now);
        }
        if let Some(tabs) = self.tabs.take() {
            self.on_tabs(tabs, now);
        }
    }

    fn index(&self, id: PreviewId) -> Option<usize> {
        self.previews.iter().position(|p| p.id == id)
    }

    fn data_tag(&mut self, preview: PreviewId, kind: DataKind) -> u64 {
        self.next_data_tag += 1;
        self.data_tags.insert(self.next_data_tag, (preview, kind));
        self.next_data_tag
    }

    fn wake_at(&mut self, at: Instant) {
        merge_wake(&mut self.wake, at);
    }

    fn select_default(&mut self) {
        if self.selected.is_none_or(|id| self.index(id).is_none()) {
            self.selected = self.previews.first().map(|p| p.id);
        }
    }

    /// The artifacts that came through `open_preview` become previews.
    fn accept_queue(&mut self) {
        while let Some(artifact) = self.queue.pop_front() {
            if let Some(existing) = self.previews.iter().find(|p| p.artifact == artifact) {
                let id = existing.id;
                self.selected = Some(id);
                if matches!(existing.phase, Phase::Lost(_)) {
                    self.requests.push(UiRequest::Reopen(id));
                }
                continue;
            }
            if self.previews.len() >= MAX_PREVIEWS {
                self.notice = Some(format!(
                    "Up to {MAX_PREVIEWS} previews can be open at once. Close one, then open this again."
                ));
                continue;
            }
            self.next_preview += 1;
            self.previews
                .push(Preview::new(self.next_preview, artifact));
            self.selected = Some(self.next_preview);
        }
    }

    fn wants_stream(&self, p: &Preview, now: Instant) -> bool {
        p.is_open()
            && p.visible
            && self.selected == Some(p.id)
            && p.cooldown.is_none_or(|until| until <= now)
    }

    /// The one place that ends a stream: the data connection is cancelled, the
    /// frame picture dropped and the subscription and the tab's streaming are
    /// undone (queued with the tab's other requests, so their order holds). A
    /// Capture (receiving, decoding or shown) is separate and is not touched.
    fn stop_stream(&mut self, idx: usize, ctx: &mut FeatureContext<'_>) {
        let now = self.clock.now();
        let p = &mut self.previews[idx];
        let (subscription, tag) = match mem::replace(&mut p.stream, Stream::Off) {
            Stream::Binding { subscription, tag } | Stream::Live { subscription, tag } => {
                (Some(subscription), Some(tag))
            }
            other => {
                p.stream = other;
                (None, None)
            }
        };
        if let Some(tag) = tag {
            ctx.cancel_data(tag);
            self.data_tags.remove(&tag);
        }
        let p = &mut self.previews[idx];
        p.picture.clear();
        p.frame_ready = false;
        p.shown = None;
        p.cooldown = Some(now + STREAM_COOLDOWN);
        if let Some(subscription) = subscription {
            p.queue(Action::Unsubscribe(subscription));
        }
        if p.streaming_asked || p.tab.as_ref().is_some_and(|t| t.streaming) {
            p.queue(Action::StreamingOff);
        }
        self.wake_at(now + STREAM_COOLDOWN);
    }

    fn remove_preview(&mut self, id: PreviewId, ctx: &mut FeatureContext<'_>) {
        let tags: Vec<u64> = self
            .data_tags
            .iter()
            .filter(|(_, (p, _))| *p == id)
            .map(|(t, _)| *t)
            .collect();
        for tag in tags {
            ctx.cancel_data(tag);
            self.data_tags.remove(&tag);
        }
        self.ops.abandon_where(|p| p.preview() == Some(id));
        self.previews.retain(|p| p.id != id);
        self.select_default();
    }

    // ---- drive: everything that sends --------------------------------

    fn drive(&mut self, ctx: &mut FeatureContext<'_>) {
        let now = self.clock.now();
        self.wake = None;
        for tag in mem::take(&mut self.pending_cancel) {
            ctx.cancel_data(tag);
        }
        for request in mem::take(&mut self.requests) {
            self.apply_request(request, ctx);
        }
        self.drive_session(now);
        for idx in 0..self.previews.len() {
            self.drive_preview(idx, ctx, now);
        }
        self.send_probe(ctx, now);
        self.flush(ctx, now);
    }

    fn flush(&mut self, ctx: &mut FeatureContext<'_>, now: Instant) {
        for _ in 0..8 {
            let mut sent = 0;
            let recovering = matches!(&self.session, SessionState::Recovering(rec) if !rec.resumed);
            let (finished, wake) = self.ops.drive(
                now,
                &|purpose| recovering && purpose.preview().is_some(),
                &mut |tag, request| {
                    sent += 1;
                    match request {
                        FeatureRequest::Command(command) => ctx.command(tag, command),
                        FeatureRequest::Query(query) => ctx.query(tag, query),
                    }
                },
            );
            if let Some(at) = wake {
                self.wake_at(at);
            }
            let ended = finished.len();
            for f in finished {
                self.on_finished(f, ctx, now);
            }
            if sent == 0 && ended == 0 {
                break;
            }
        }
    }

    fn apply_request(&mut self, request: UiRequest, ctx: &mut FeatureContext<'_>) {
        match request {
            UiRequest::Close(id) => {
                let Some(idx) = self.index(id) else { return };
                if self.previews[idx].is_open() {
                    self.previews[idx].queue(Action::Close);
                } else {
                    self.remove_preview(id, ctx);
                }
            }
            UiRequest::Reopen(id) => {
                let Some(idx) = self.index(id) else { return };
                let p = &mut self.previews[idx];
                if matches!(p.phase, Phase::Lost(_)) {
                    p.forget_connection();
                    p.tab = None;
                    p.resize_failed = None;
                    p.status = None;
                    p.phase = Phase::Waiting;
                    self.selected = Some(id);
                }
            }
            UiRequest::Retry(id) => {
                let Some(idx) = self.index(id) else { return };
                let p = &mut self.previews[idx];
                p.resize_failed = None;
                if matches!(p.stream, Stream::Blocked(_)) {
                    p.stream = Stream::Off;
                    p.attempts = 0;
                    p.cooldown = None;
                }
            }
            UiRequest::Do(id, action) => {
                if let Some(idx) = self.index(id)
                    && self.previews[idx].is_open()
                {
                    self.previews[idx].queue(action);
                }
            }
        }
    }

    fn drive_session(&mut self, now: Instant) {
        let waiting = self.previews.iter().any(|p| p.phase == Phase::Waiting);
        match &self.session {
            SessionState::None | SessionState::Failed if waiting => {
                let op = self.ops.submit(
                    Purpose::CreateSession,
                    Command::Browser(BrowserCommand::CreateRendererSession {}),
                    now,
                );
                self.session = SessionState::Creating(op);
            }
            SessionState::Ready { session, .. } => {
                let session = session.clone();
                for idx in 0..self.previews.len() {
                    if self.previews[idx].phase == Phase::Waiting {
                        self.submit_open(idx, &session, now);
                    }
                }
            }
            _ => {}
        }
        if let SessionState::Recovering(rec) = &mut self.session
            && rec.resuming.is_none()
            && rec.probe_due
            && self.probe == Probe::Idle
        {
            rec.probe_due = false;
            self.probe = Probe::Need;
        }
        if self.probe_wanted
            && self.probe == Probe::Idle
            && matches!(self.session, SessionState::Ready { .. })
            && self.previews.iter().any(|p| p.tab.is_some())
        {
            let due = self
                .last_probe
                .is_none_or(|at| now.duration_since(at) >= PROBE_INTERVAL);
            if due {
                self.probe = Probe::Need;
            } else if let Some(at) = self.last_probe {
                self.wake_at(at + PROBE_INTERVAL);
            }
        }
    }

    /// Ends what a preview that `keep` does not keep was asked for before the session or the
    /// conversation changed, so that nothing is opened in a later one:
    /// - Waiting for a session, or an `OpenPreview` whose next send is not yet handed to the
    ///   connection (not sent, or refused as busy and waiting to be sent again): the preview is
    ///   lost and the command is dropped.
    /// - An `OpenPreview` already handed over: it is not repeated by itself any more, so a busy
    ///   answer that comes later loses the preview; an accepted or unknown outcome is unchanged.
    ///
    /// Only an explicit reopen opens a lost preview again.
    fn lose_stale_opens(&mut self, why: &str, keep: impl Fn(&Preview) -> bool) {
        for p in &mut self.previews {
            if keep(p) {
                continue;
            }
            match p.phase {
                Phase::Waiting => p.phase = Phase::Lost(why.to_owned()),
                Phase::Opening(op) if self.ops.awaits_send(op) => {
                    self.ops.abandon(op);
                    p.phase = Phase::Lost(why.to_owned());
                }
                Phase::Opening(op) => self.ops.forbid_retry(op),
                _ => {}
            }
        }
    }

    fn submit_open(&mut self, idx: usize, session: &RendererSessionRef, now: Instant) {
        let p = &mut self.previews[idx];
        let viewport = p.want_viewport.unwrap_or(RendererViewport {
            width: OPEN_VIEWPORT.0,
            height: OPEN_VIEWPORT.1,
        });
        let command = Command::Artifacts(ArtifactsCommand::OpenPreview {
            artifact: p.artifact.clone(),
            session: session.clone(),
            viewport,
        });
        let op = self.ops.submit(Purpose::OpenPreview(p.id), command, now);
        p.phase = Phase::Opening(op);
        p.status = None;
    }

    fn send_probe(&mut self, ctx: &mut FeatureContext<'_>, now: Instant) {
        if self.probe != Probe::Need {
            return;
        }
        if !matches!(
            self.session,
            SessionState::Ready { .. } | SessionState::Recovering(_)
        ) {
            self.probe = Probe::Idle;
            return;
        }
        self.next_probe_tag += 1;
        let tag = self.next_probe_tag;
        self.probe = Probe::Sent(tag);
        self.probe_wanted = false;
        self.last_probe = Some(now);
        ctx.query(
            tag,
            Query::Browser(BrowserQuery::ListRendererTabs {
                offset: 0,
                limit: MAX_RENDERER_TABS as u32,
            }),
        );
    }

    /// The next request for a tab, when none is in flight: undoing a
    /// subscription, starting one, resizing, what the user asked for, input.
    fn drive_preview(&mut self, idx: usize, ctx: &mut FeatureContext<'_>, now: Instant) {
        if !matches!(self.session, SessionState::Ready { .. }) || !self.previews[idx].is_open() {
            return;
        }
        let want = self.wants_stream(&self.previews[idx], now);
        if !want
            && matches!(
                self.previews[idx].stream,
                Stream::Binding { .. } | Stream::Live { .. }
            )
        {
            self.stop_stream(idx, ctx);
        }
        let p = &mut self.previews[idx];
        if p.lane.is_some() {
            return;
        }
        let Some(tab) = p.tab.clone() else { return };
        let id = p.id;
        let reference = tab.reference.clone();
        let submit = |ops: &mut Ops<Purpose>, purpose: Purpose, command: BrowserCommand| {
            ops.submit(purpose, Command::Browser(command), now)
        };

        // 1. Undo what a stopped stream left behind.
        if let Some(at) = p
            .actions
            .iter()
            .position(|a| matches!(a, Action::Unsubscribe(_) | Action::StreamingOff))
        {
            let op = match p.actions.remove(at).unwrap() {
                Action::Unsubscribe(subscription) => submit(
                    &mut self.ops,
                    Purpose::Unsubscribe(id),
                    BrowserCommand::UnsubscribeRendererFrames { subscription },
                ),
                _ => {
                    p.streaming_asked = false;
                    submit(
                        &mut self.ops,
                        Purpose::Streaming(id, false),
                        BrowserCommand::SetRendererStreaming {
                            tab: reference,
                            enabled: false,
                        },
                    )
                }
            };
            p.lane = Some(op);
            return;
        }
        // 2. Start the stream of a tab that is shown.
        if want {
            match p.stream {
                Stream::Off => {
                    let op = submit(
                        &mut self.ops,
                        Purpose::Subscribe(id),
                        BrowserCommand::SubscribeRendererFrames { tab: reference },
                    );
                    p.stream = Stream::Subscribing(op);
                    p.lane = Some(op);
                    return;
                }
                Stream::Binding { .. } | Stream::Live { .. }
                    if !tab.streaming && !p.streaming_asked =>
                {
                    p.streaming_asked = true;
                    let op = submit(
                        &mut self.ops,
                        Purpose::Streaming(id, true),
                        BrowserCommand::SetRendererStreaming {
                            tab: reference,
                            enabled: true,
                        },
                    );
                    p.lane = Some(op);
                    return;
                }
                _ => {}
            }
        } else if let Some(until) = p.cooldown
            && until > now
            && p.visible
        {
            merge_wake(&mut self.wake, until);
        }
        // 3. Resize to the area, at most once in an interval.
        // A failure holds only for the size that failed: a different wish lifts it.
        if p.resize_failed.is_some() && p.resize_failed != p.want_viewport {
            p.resize_failed = None;
        }
        if let Some(want) = p.want_viewport
            && want != tab.viewport
            && p.resize_failed != Some(want)
        {
            match p.last_resize {
                Some(last) if now.duration_since(last) < RESIZE_INTERVAL => {
                    merge_wake(&mut self.wake, last + RESIZE_INTERVAL);
                }
                _ => {
                    p.last_resize = Some(now);
                    let op = submit(
                        &mut self.ops,
                        Purpose::Resize(id, want),
                        BrowserCommand::ResizeRendererTab {
                            tab: reference,
                            viewport: want,
                        },
                    );
                    p.lane = Some(op);
                    return;
                }
            }
        }
        // 4. What the user asked for. One Capture at a time: a second one would
        // overwrite the data tag of the first and leave its blob slot taken.
        if matches!(p.capture, Capture::Receiving(_) | Capture::Decoding(_)) {
            p.actions.retain(|a| *a != Action::Capture);
        }
        if let Some(action) = p.actions.pop_front() {
            let (purpose, command) = match action {
                Action::Capture => {
                    p.capture_picture.clear();
                    p.capture = Capture::Receiving(None);
                    (
                        Purpose::Capture(id),
                        BrowserCommand::CaptureRendererTab { tab: reference },
                    )
                }
                Action::Notices => (
                    Purpose::Notices(id),
                    BrowserCommand::TakeRendererNotices { tab: reference },
                ),
                Action::Inspect(kind) => (
                    Purpose::Inspect(id, kind),
                    BrowserCommand::RendererCdp {
                        tab: reference,
                        method: kind.method().to_owned(),
                        params: kind.params(),
                    },
                ),
                Action::Close => (
                    Purpose::CloseTab(id),
                    BrowserCommand::CloseRendererTab { tab: reference },
                ),
                Action::Unsubscribe(_) | Action::StreamingOff => unreachable!("handled above"),
            };
            let op = submit(&mut self.ops, purpose, command);
            p.lane = Some(op);
            return;
        }
        // 5. Input, in the order it was made.
        if let Some(input) = p.input.pop() {
            let (method, params) = input.cdp();
            let op = submit(
                &mut self.ops,
                Purpose::Input(id),
                BrowserCommand::RendererCdp {
                    tab: reference,
                    method: method.to_owned(),
                    params,
                },
            );
            p.lane = Some(op);
        }
    }

    // ---- results ------------------------------------------------------

    fn is_gone(failure: &Failure) -> bool {
        use workbench_protocol::namespaces::browser::RendererFailure::*;
        match failure {
            Failure::Renderer(Closed | StaleGeneration | Restart | Unavailable) => true,
            Failure::Refused(e) => matches!(
                e.code,
                Some(ErrorCode::PermissionDenied | ErrorCode::NotFound | ErrorCode::StaleEpoch)
            ),
            _ => false,
        }
    }

    fn lose(&mut self, idx: usize, why: String, ctx: &mut FeatureContext<'_>) {
        for tag in self.tags_of(self.previews[idx].id) {
            ctx.cancel_data(tag);
            self.data_tags.remove(&tag);
        }
        let id = self.previews[idx].id;
        self.ops.abandon_where(|p| p.preview() == Some(id));
        let p = &mut self.previews[idx];
        p.forget_connection();
        p.lane = None;
        p.actions.clear();
        p.phase = Phase::Lost(why);
    }

    fn tags_of(&self, id: PreviewId) -> Vec<u64> {
        self.data_tags
            .iter()
            .filter(|(_, (p, _))| *p == id)
            .map(|(t, _)| *t)
            .collect()
    }

    fn on_finished(&mut self, f: Finished<Purpose>, ctx: &mut FeatureContext<'_>, now: Instant) {
        let Finished {
            id: op,
            purpose,
            operation,
            result,
        } = f;
        if let Some(pid) = purpose.preview()
            && let Some(idx) = self.index(pid)
            && self.previews[idx].lane == Some(op)
        {
            self.previews[idx].lane = None;
        }
        match purpose {
            Purpose::CreateSession => self.on_session(result),
            Purpose::ResumeSession => self.on_resumed(result),
            Purpose::OpenPreview(pid) => self.on_opened(pid, result, ctx),
            Purpose::Subscribe(pid) => self.on_subscribed(pid, result, ctx, now),
            Purpose::Capture(pid) => self.on_captured(pid, operation, result, ctx),
            purpose => {
                let Some(pid) = purpose.preview() else { return };
                let Some(idx) = self.index(pid) else { return };
                match (&purpose, result) {
                    (Purpose::Streaming(_, enabled), Ok(_)) => {
                        if let Some(tab) = self.previews[idx].tab.as_mut() {
                            tab.streaming = *enabled;
                        }
                    }
                    (Purpose::Resize(_, viewport), Ok(_)) => {
                        if let Some(tab) = self.previews[idx].tab.as_mut() {
                            tab.viewport = *viewport;
                        }
                    }
                    (Purpose::Notices(_), Ok(RendererOutput::Notices { blocked, dialogs })) => {
                        let (blocked, a) = bounded_lines(&blocked);
                        let (dialogs, b) = bounded_lines(&dialogs);
                        self.previews[idx].notices = Some(Notices {
                            blocked,
                            dialogs,
                            truncated: a || b,
                        });
                    }
                    (Purpose::Inspect(_, kind), Ok(RendererOutput::Cdp { result })) => {
                        let pretty = serde_json::to_string_pretty(&result).unwrap_or_default();
                        let (text, truncated) = bounded(&pretty, MAX_RESULT_BYTES);
                        self.previews[idx].inspection = Some(Inspection {
                            label: kind.label(),
                            text,
                            truncated,
                        });
                    }
                    (Purpose::CloseTab(_), Ok(_)) => self.remove_preview(pid, ctx),
                    (Purpose::CloseTab(_), Err(failure)) if Self::is_gone(&failure) => {
                        self.remove_preview(pid, ctx)
                    }
                    (Purpose::Unsubscribe(_), _) | (Purpose::Input(_), Ok(_)) => {}
                    (_, Ok(_)) => {
                        self.previews[idx].status =
                            Some("Unexpected reply from the core".to_owned());
                    }
                    (purpose, Err(failure)) => {
                        if Self::is_gone(&failure) {
                            self.lose(
                                idx,
                                format!("The preview was closed ({})", failure.describe()),
                                ctx,
                            );
                        } else {
                            if let Purpose::Resize(_, viewport) = purpose {
                                // Not sent again for this size, whatever the reason: a
                                // new size is the next try.
                                self.previews[idx].resize_failed = Some(*viewport);
                            }
                            let what = match purpose {
                                Purpose::Input(_) => "Input",
                                Purpose::Resize(..) => "Resize",
                                Purpose::Streaming(..) => "Streaming",
                                Purpose::Notices(_) => "Reading notices",
                                Purpose::Inspect(..) => "Inspection",
                                _ => "Operation",
                            };
                            self.previews[idx].status =
                                Some(format!("{what} failed: {}", failure.describe()));
                        }
                    }
                }
            }
        }
    }

    fn on_session(&mut self, result: Result<RendererOutput, Failure>) {
        match result {
            Ok(RendererOutput::Session {
                session,
                resume_token,
            }) => {
                self.session = SessionState::Ready {
                    session,
                    token: resume_token,
                }
            }
            other => {
                let why = match other {
                    Err(failure) => failure.describe(),
                    Ok(_) => "Unexpected reply from the core".to_owned(),
                };
                self.session = SessionState::Failed;
                for p in &mut self.previews {
                    if p.phase == Phase::Waiting {
                        p.phase = Phase::Lost(format!("Could not create a session: {why}"));
                    }
                }
            }
        }
    }

    fn on_resumed(&mut self, result: Result<RendererOutput, Failure>) {
        let SessionState::Recovering(rec) = &mut self.session else {
            return;
        };
        match result {
            Ok(RendererOutput::Session {
                session,
                resume_token,
            }) => {
                rec.session = session;
                rec.token = resume_token;
                rec.resumed = true;
                rec.resuming = None;
                self.probe = Probe::Need;
            }
            other => {
                let why = match other {
                    Err(failure) => failure.describe(),
                    Ok(_) => "Unexpected reply from the core".to_owned(),
                };
                if rec.for_missing {
                    self.abandon_recovery(&why);
                } else {
                    // Nothing was missing: the session was not detached, and what
                    // is in flight is asked about as usual.
                    rec.resuming = None;
                    rec.resumed = true;
                    self.probe = Probe::Need;
                }
            }
        }
    }

    /// The session is gone: what was waiting for it is lost, to be opened again.
    fn abandon_recovery(&mut self, why: &str) {
        self.session = SessionState::None;
        self.lost_at = None;
        self.probe = Probe::Idle;
        for p in &mut self.previews {
            if p.phase == Phase::Suspended {
                p.phase = Phase::Lost(format!(
                    "Could not resume the session ({why}). Open it again."
                ));
            }
        }
        self.forget_lost_operations();
    }

    /// What was asked for a preview that is lost is not waited for any more.
    fn forget_lost_operations(&mut self) {
        let lost: Vec<PreviewId> = self
            .previews
            .iter()
            .filter(|p| matches!(p.phase, Phase::Lost(_)))
            .map(|p| p.id)
            .collect();
        self.ops
            .abandon_where(|purpose| purpose.preview().is_some_and(|id| lost.contains(&id)));
        for p in &mut self.previews {
            if matches!(p.phase, Phase::Lost(_)) {
                p.lane = None;
            }
        }
    }

    fn on_opened(
        &mut self,
        pid: PreviewId,
        result: Result<RendererOutput, Failure>,
        ctx: &mut FeatureContext<'_>,
    ) {
        let Some(idx) = self.index(pid) else { return };
        let ready = matches!(self.session, SessionState::Ready { .. });
        match result {
            Ok(RendererOutput::Tab(tab)) if tab.artifact == self.previews[idx].artifact => {
                let p = &mut self.previews[idx];
                p.tab = Some(*tab);
                p.phase = if ready { Phase::Open } else { Phase::Suspended };
                if let SessionState::Recovering(rec) = &mut self.session {
                    rec.probe_due = true;
                }
            }
            Ok(_) => self.lose(idx, "Unexpected reply from the core".to_owned(), ctx),
            Err(Failure::Unknown) => {
                self.previews[idx].phase = Phase::OpenUnknown;
                self.previews[idx].status = Some(
                    "Not known whether it opened (not sent again). Checking the tab list."
                        .to_owned(),
                );
                self.probe = if ready { Probe::Need } else { self.probe };
                if let SessionState::Recovering(rec) = &mut self.session {
                    rec.probe_due = true;
                }
            }
            Err(failure) => self.lose(
                idx,
                format!("Could not open the preview: {}", failure.describe()),
                ctx,
            ),
        }
    }

    fn on_subscribed(
        &mut self,
        pid: PreviewId,
        result: Result<RendererOutput, Failure>,
        ctx: &mut FeatureContext<'_>,
        now: Instant,
    ) {
        let Some(idx) = self.index(pid) else { return };
        match result {
            Ok(RendererOutput::Subscription {
                subscription,
                token,
                tab,
            }) if self.previews[idx]
                .tab
                .as_ref()
                .is_some_and(|t| t.reference == tab) =>
            {
                let want = self.wants_stream(&self.previews[idx], now);
                let p = &mut self.previews[idx];
                if !want {
                    p.stream = Stream::Off;
                    p.queue(Action::Unsubscribe(subscription));
                    return;
                }
                let tag = self.data_tag(pid, DataKind::Frames);
                self.previews[idx].stream = Stream::Binding {
                    subscription: subscription.clone(),
                    tag,
                };
                ctx.start_data(
                    tag,
                    DataSource::Frames {
                        tab,
                        subscription,
                        token,
                    },
                );
            }
            Ok(_) => {
                self.previews[idx].stream =
                    Stream::Blocked("Unexpected reply from the core".to_owned())
            }
            Err(failure) => {
                if Self::is_gone(&failure) {
                    self.lose(
                        idx,
                        format!("The preview was closed ({})", failure.describe()),
                        ctx,
                    );
                } else {
                    // The command was already repeated as far as its advice
                    // allows (see `ops`); what is left waits for the user.
                    let p = &mut self.previews[idx];
                    p.stream = Stream::Blocked(failure.describe());
                    if matches!(&failure, Failure::Refused(e) if e.retry == Some(RetryAdvice::AfterResync))
                    {
                        self.probe_wanted = true;
                    }
                }
            }
        }
    }

    /// A stream could not start or ended with an error. Only the advice that
    /// allows it is followed, and only a few times; the rest waits for the
    /// user, after the state is read again.
    fn stream_failed(
        &mut self,
        idx: usize,
        message: String,
        error: Option<&FeatureError>,
        now: Instant,
    ) {
        let p = &mut self.previews[idx];
        p.attempts = p.attempts.saturating_add(1);
        let advice = error.and_then(|e| e.retry);
        let again = error.is_none_or(|e| e.code == Some(ErrorCode::Busy))
            && advice == Some(RetryAdvice::SameRequest)
            && p.attempts < MAX_STREAM_ATTEMPTS;
        if again {
            p.stream = Stream::Off;
            p.cooldown = Some(now + backoff(p.attempts));
        } else {
            p.stream = Stream::Blocked(message);
            if advice == Some(RetryAdvice::AfterResync) {
                self.probe_wanted = true;
            }
        }
    }

    fn on_captured(
        &mut self,
        pid: PreviewId,
        operation: Option<RendererId>,
        result: Result<RendererOutput, Failure>,
        ctx: &mut FeatureContext<'_>,
    ) {
        let Some(idx) = self.index(pid) else { return };
        // After a reset the capture is no longer waited for.
        if !matches!(self.previews[idx].capture, Capture::Receiving(None)) {
            return;
        }
        match (result, operation) {
            (
                Ok(RendererOutput::Capture {
                    capture_id,
                    byte_len,
                }),
                Some(operation_id),
            ) => {
                if byte_len > MAX_CAPTURE_BLOB_BYTES {
                    self.previews[idx].capture =
                        Capture::Failed(format!("The capture is too large ({byte_len} bytes)"));
                    return;
                }
                let tag = self.data_tag(pid, DataKind::Capture);
                self.previews[idx].capture = Capture::Receiving(Some(tag));
                ctx.start_data(
                    tag,
                    DataSource::Capture {
                        operation_id,
                        capture_id,
                    },
                );
            }
            (Err(failure), _) => {
                if Self::is_gone(&failure) {
                    self.lose(
                        idx,
                        format!("The preview was closed ({})", failure.describe()),
                        ctx,
                    );
                } else {
                    self.previews[idx].capture_picture.clear();
                    self.previews[idx].capture =
                        Capture::Failed(format!("Capture failed: {}", failure.describe()));
                }
            }
            _ => {
                self.previews[idx].capture =
                    Capture::Failed("Unexpected reply from the core".to_owned())
            }
        }
    }

    /// The tab list: what is found again after a reconnect, what a lost reply
    /// opened, and what has gone.
    fn on_tabs(&mut self, result: Result<Vec<RendererTab>, FeatureError>, now: Instant) {
        let recovering = matches!(self.session, SessionState::Recovering(_));
        let tabs = match result {
            Ok(tabs) => tabs,
            Err(_) if recovering => Vec::new(),
            Err(_) => return,
        };
        let claimed: Vec<_> = self
            .previews
            .iter()
            .filter_map(|p| p.tab.as_ref().map(|t| t.reference.clone()))
            .collect();
        let mut missing = false;
        for p in &mut self.previews {
            match p.phase {
                Phase::OpenUnknown => {
                    match tabs
                        .iter()
                        .find(|t| t.artifact == p.artifact && !claimed.contains(&t.reference))
                    {
                        Some(tab) => {
                            p.tab = Some(tab.clone());
                            p.phase = Phase::Open;
                            p.status = None;
                        }
                        None => {
                            p.phase = Phase::Lost(
                                "Could not confirm that it opened. Open it again.".to_owned(),
                            )
                        }
                    }
                }
                Phase::Open | Phase::Suspended if p.tab.is_some() => {
                    let reference = &p.tab.as_ref().unwrap().reference;
                    match tabs.iter().find(|t| &t.reference == reference) {
                        Some(tab) => {
                            p.tab = Some(tab.clone());
                            p.phase = Phase::Open;
                        }
                        None if p.phase == Phase::Suspended => missing = true,
                        None => {
                            p.forget_connection();
                            p.phase = Phase::Lost(
                                "The tab was closed (for example, the helper restarted). Open it again."
                                    .to_owned(),
                            );
                        }
                    }
                }
                _ => {}
            }
        }
        let SessionState::Recovering(rec) = &mut self.session else {
            return;
        };
        let opening = self
            .previews
            .iter()
            .any(|p| matches!(p.phase, Phase::Opening(_)));
        if (missing || opening) && !rec.resumed && now.duration_since(rec.since) < GRACE {
            rec.for_missing = missing;
            let command = Command::Browser(BrowserCommand::ResumeRendererSession {
                session: rec.session.clone(),
                token: rec.token.clone(),
            });
            rec.resuming = Some(self.ops.submit(Purpose::ResumeSession, command, now));
            return;
        }
        // An open that is still in flight needs the session to stay.
        if opening && rec.resumed {
            return;
        }
        let rec = match mem::replace(&mut self.session, SessionState::None) {
            SessionState::Recovering(rec) => rec,
            other => {
                self.session = other;
                return;
            }
        };
        let kept = self.previews.iter().any(|p| p.is_open());
        for p in &mut self.previews {
            if p.phase == Phase::Suspended {
                p.phase = Phase::Lost(
                    "The session's grace period ran out or the core restarted. Open it again."
                        .to_owned(),
                );
            }
        }
        self.forget_lost_operations();
        self.lost_at = None;
        if kept || rec.resumed {
            self.session = SessionState::Ready {
                session: rec.session,
                token: rec.token,
            };
        }
    }

    // ---- frames -------------------------------------------------------

    fn ingest(&mut self, ctx: &mut FeatureContext<'_>, egui_ctx: &egui::Context) {
        let now = self.clock.now();
        for idx in 0..self.previews.len() {
            let want = self.wants_stream(&self.previews[idx], now);
            let tag = match &self.previews[idx].stream {
                Stream::Binding { tag, .. } | Stream::Live { tag, .. } => Some(*tag),
                _ => None,
            };
            if let Some(tag) = tag {
                self.previews[idx].frame_ready = false;
                if let Some(payload) = ctx.take_data(tag) {
                    let p = &mut self.previews[idx];
                    if let (DataPayload::Frame(frame), true, Some(tab)) = (payload, want, &p.tab)
                        && frame.start.tab == tab.reference.id
                        && frame.start.core_epoch == tab.reference.core_epoch
                        && frame.start.generation == tab.reference.generation
                    {
                        match bgra_to_image(&frame) {
                            Ok(image) => {
                                p.shown = Some([frame.start.width, frame.start.height]);
                                p.picture.show(
                                    egui_ctx,
                                    "browser-preview",
                                    image,
                                    Some(frame.start.sequence),
                                );
                                p.attempts = 0;
                                p.status = None;
                            }
                            Err(error) => p.status = Some(error.describe()),
                        }
                    }
                }
            }
            let capture_tag = match &self.previews[idx].capture {
                Capture::Receiving(Some(tag)) => Some(*tag),
                _ => None,
            };
            if let Some(tag) = capture_tag
                && let Some(payload) = ctx.take_data(tag)
            {
                // The blob slot is only freed by an explicit cancel.
                ctx.cancel_data(tag);
                self.data_tags.remove(&tag);
                let p = &mut self.previews[idx];
                p.capture = match payload {
                    DataPayload::Blob(blob)
                        if blob.start.source == BlobSource::Capture
                            && blob.start.media_type == MediaType::Png =>
                    {
                        match DecodeJob::start(blob.bytes, egui_ctx.clone(), &self.decoders) {
                            Some(job) => Capture::Decoding(job),
                            None => Capture::Failed(
                                "Too many images are being decoded. Wait a moment and try again."
                                    .to_owned(),
                            ),
                        }
                    }
                    _ => Capture::Failed("The capture is not in a supported format".to_owned()),
                };
            }
            let p = &mut self.previews[idx];
            if let Capture::Decoding(job) = &p.capture
                && let Some(result) = job.poll()
            {
                match result {
                    Ok(image) => {
                        p.capture_picture
                            .show(egui_ctx, "browser-capture", image, None);
                        p.capture = Capture::Idle;
                    }
                    Err(error) => {
                        p.capture_picture.clear();
                        p.capture = Capture::Failed(error.describe());
                    }
                }
            }
        }
    }

    /// What the notices of the data connection mean for a preview's state. The
    /// callback only changes state; the next render acts on it.
    fn on_data(&mut self, tag: u64, notice: DataNotice) {
        let Some(&(pid, kind)) = self.data_tags.get(&tag) else {
            return;
        };
        let now = self.clock.now();
        let Some(idx) = self.index(pid) else {
            self.data_tags.remove(&tag);
            return;
        };
        match (kind, notice) {
            (DataKind::Frames, DataNotice::Started) => {
                let p = &mut self.previews[idx];
                if let Stream::Binding {
                    subscription,
                    tag: t,
                } = &p.stream
                    && *t == tag
                {
                    p.stream = Stream::Live {
                        subscription: subscription.clone(),
                        tag,
                    };
                }
            }
            (DataKind::Frames, DataNotice::Ready) => self.previews[idx].frame_ready = true,
            (DataKind::Frames, DataNotice::Completed) => {
                self.data_tags.remove(&tag);
                if self.pending_cancel.len() < 16 {
                    self.pending_cancel.push(tag);
                }
                let p = &mut self.previews[idx];
                p.picture.clear();
                p.shown = None;
                if let Stream::Binding { subscription, .. } | Stream::Live { subscription, .. } =
                    mem::replace(&mut p.stream, Stream::Off)
                {
                    p.queue(Action::Unsubscribe(subscription));
                }
                p.attempts = p.attempts.saturating_add(1);
                if p.attempts >= MAX_STREAM_ATTEMPTS {
                    p.stream = Stream::Blocked("The stream ended repeatedly".to_owned());
                } else {
                    p.cooldown = Some(now + backoff(p.attempts));
                }
                self.probe_wanted = true;
            }
            (DataKind::Frames, DataNotice::Failed(error)) => {
                self.data_tags.remove(&tag);
                let p = &mut self.previews[idx];
                p.picture.clear();
                p.shown = None;
                if let Stream::Binding { subscription, .. } | Stream::Live { subscription, .. } =
                    mem::replace(&mut p.stream, Stream::Off)
                {
                    p.queue(Action::Unsubscribe(subscription));
                }
                let message = error.message.clone();
                self.stream_failed(idx, message, Some(&error), now);
            }
            (DataKind::Frames, DataNotice::Cancelled) => {
                self.data_tags.remove(&tag);
                let p = &mut self.previews[idx];
                p.picture.clear();
                p.shown = None;
                p.frame_ready = false;
                p.stream = Stream::Off;
                self.lost_at.get_or_insert(now);
            }
            (DataKind::Capture, DataNotice::Failed(error)) => {
                self.data_tags.remove(&tag);
                self.previews[idx].capture =
                    Capture::Failed(format!("Could not receive the capture: {}", error.message));
            }
            (DataKind::Capture, DataNotice::Cancelled) => {
                self.data_tags.remove(&tag);
                let p = &mut self.previews[idx];
                if matches!(p.capture, Capture::Receiving(Some(t)) if t == tag) {
                    p.capture = Capture::Idle;
                    p.capture_picture.clear();
                }
            }
            (DataKind::Capture, _) => {}
        }
    }
}

impl Feature for BrowserFeature {
    fn id(&self) -> FeatureId {
        FeatureId::Browser
    }

    fn title(&self) -> &'static str {
        "Browser"
    }

    fn availability(&self) -> Availability {
        Availability::Implemented {
            required: &[Capability::Commands, Capability::Queries],
        }
    }

    fn reset(&mut self) {
        self.observation.reset();
        let now = self.clock.now();
        // An open that was asked for and not yet taken belongs to the session it was asked in
        // (a card's Open, an artifact's preview): a new snapshot does not carry it over.
        self.queue.clear();
        self.lose_stale_opens(LOST_STALE_OPEN, |_| false);
        self.pending_cancel.clear();
        self.ops.on_reset(now);
        self.data_tags.clear();
        self.probe = Probe::Idle;
        self.probe_wanted = false;
        // A finished opening is settled by the next pass: dropping it would leave its preview
        // opening for ever. What else finished belonged to the connection that is gone.
        self.done
            .retain(|f| matches!(f.purpose, Purpose::OpenPreview(_)));
        self.tabs = None;
        let uses_session = self
            .previews
            .iter()
            .any(|p| p.tab.is_some() || matches!(p.phase, Phase::Opening(_) | Phase::OpenUnknown));
        for p in &mut self.previews {
            p.forget_connection();
            p.lane = p.lane.filter(|op| self.ops.contains(*op));
            if p.tab.is_some() && matches!(p.phase, Phase::Open | Phase::Suspended) {
                p.phase = Phase::Suspended;
            }
        }
        match mem::replace(&mut self.session, SessionState::None) {
            SessionState::Ready { session, token } if uses_session => {
                let since = *self.lost_at.get_or_insert(now);
                self.session = SessionState::Recovering(Recovery {
                    session,
                    token,
                    since,
                    resumed: false,
                    resuming: None,
                    for_missing: false,
                    probe_due: true,
                });
            }
            SessionState::Recovering(mut rec) => {
                rec.resuming = rec.resuming.filter(|op| self.ops.contains(*op));
                rec.probe_due = true;
                self.session = SessionState::Recovering(rec);
            }
            SessionState::Creating(op) if self.ops.contains(op) => {
                self.session = SessionState::Creating(op);
            }
            _ => self.lost_at = None,
        }
    }

    fn event(&mut self, event: &UiEvent) {
        self.observation.event(event);
        match event {
            UiEvent::Browser(BrowserEvent::OperationChanged { .. }) => self.ops.on_event(),
            UiEvent::Browser(
                BrowserEvent::TabChanged { .. } | BrowserEvent::RendererChanged { .. },
            ) => self.probe_wanted = true,
            _ => {}
        }
    }

    fn reply(&mut self, tag: u64, reply: FeatureReply) {
        if observation::owns_tag(tag) {
            self.observation.reply(tag, reply);
            return;
        }
        if self.probe == Probe::Sent(tag) {
            self.probe = Probe::Idle;
            self.tabs = Some(match reply {
                FeatureReply::Query(Ok(QueryReply::Browser(BrowserReply::Tabs {
                    tabs, ..
                }))) => Ok(tabs),
                FeatureReply::Query(Ok(_)) | FeatureReply::Command(Ok(_)) => {
                    Err(FeatureError::new("Unexpected reply from the core"))
                }
                FeatureReply::Query(Err(e)) | FeatureReply::Command(Err(e)) => Err(e),
            });
            return;
        }
        let now = self.clock.now();
        if let Some(finished) = self.ops.on_reply(tag, reply, now) {
            self.done.push(finished);
        }
    }

    fn data(&mut self, tag: u64, notice: DataNotice) {
        if observation::owns_tag(tag) {
            self.observation.data(tag, notice);
            return;
        }
        self.on_data(tag, notice);
    }

    fn open_preview(&mut self, artifact: ArtifactRef) {
        if self.queue.contains(&artifact) {
            return;
        }
        if self.queue.len() >= MAX_QUEUED {
            self.queue.pop_front();
        }
        self.queue.push_back(artifact);
    }

    /// A clean exit lets go of this client's own preview tabs, which also ends their
    /// frame subscriptions. A tab of a run (never in `previews`) and every tab of
    /// another client stay as they are.
    fn release(&mut self) -> Vec<Command> {
        self.previews
            .iter()
            .filter_map(|p| p.tab.as_ref())
            .filter(|tab| tab.owner == RendererOwnerKind::ClientPreview)
            .map(|tab| {
                Command::Browser(BrowserCommand::CloseRendererTab {
                    tab: tab.reference.clone(),
                })
            })
            .collect()
    }

    fn disconnected(&mut self) {
        self.observation.disconnected();
        self.queue.clear();
        self.lose_stale_opens(LOST_STALE_OPEN, |_| false);
        // The data connections are gone with the control session: no texture,
        // decoded image or stream state may outlive it. What was asked of the
        // core is settled by `reset` when a session is back.
        self.pending_cancel.clear();
        self.data_tags.clear();
        for p in &mut self.previews {
            p.forget_connection();
            p.visible = false;
        }
    }

    fn observe_run(
        &mut self,
        target: workbench_protocol::namespaces::browser::observation::ObservationTarget,
    ) {
        self.observation.observe_run(target);
    }

    fn selection_changed(&mut self, ctx: &mut FeatureContext<'_>) {
        // What waits to be taken or sent was asked for in the conversation that was selected then.
        let now = ctx.selection();
        let selected = |artifact: &ArtifactRef| {
            now.project.as_ref() == Some(&artifact.project)
                && now.thread.as_ref() == Some(&artifact.thread)
        };
        self.queue.retain(&selected);
        self.lose_stale_opens(LOST_STALE_OPEN, |p| selected(&p.artifact));
        self.observation.selection_changed(ctx);
    }

    fn maintain(&mut self, ctx: &mut FeatureContext<'_>, visibility: Visibility) {
        self.observation.maintain(ctx, visibility);
        if visibility.is_visible() {
            // `render` follows and does the work with the texture context.
            return;
        }
        // Collapsed, minimized or unavailable: nothing is on screen, so no
        // frame is wanted. The drive step ends the stream through `stop_stream`
        // and sends what that queued; there is no texture context here, so
        // nothing is ingested or drawn until the area is shown again.
        for p in &mut self.previews {
            p.visible = false;
        }
        if !ctx.connected() {
            return;
        }
        self.settle(ctx);
        self.drive(ctx);
    }

    fn render(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        self.observation.render(ui, ctx);
        let egui_ctx = ui.ctx().clone();
        self.accept_queue();
        self.settle(ctx);
        self.ingest(ctx, &egui_ctx);
        self.draw(ui);
        self.drive(ctx);
        if let Some(at) = self.wake {
            egui_ctx.request_repaint_after(at.saturating_duration_since(self.clock.now()));
        }
        if let Some(wait) = self.observation.relist_in(self.clock.now()) {
            egui_ctx.request_repaint_after(wait);
        }
    }
}
