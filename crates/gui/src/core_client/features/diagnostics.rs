//! Desktop feature of the `diagnostics` area (PR 13), drawn as the legacy Usage tab
//! (PR 17): the usage of the core over a range, and what a run of the selected thread
//! holds in its saved context. Read only.
//!
//! Both are read operations ([`read`]): the screen asks, the core answers at once
//! with the operation, and the data arrives when the area's event says it
//! settled. Nothing here sends a command.
//!
//! The footer's cost of today is a read of its own, asked whether or not the tab is
//! drawn and again every [`TODAY_REFRESH`], as the embedded workbench refreshes it.
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use super::layout::paged_list;
use super::{Availability, Feature, FeatureContext, FeatureId, FeatureReply, Visibility};
use crate::panes::usage_widgets::{SortColumn, segment};
use crate::theme::text::{h3, muted};
use read::{Phase, Read};
use usage::{BreakdownControls, Group, UsageData};
use workbench_protocol::{
    errors::ErrorCode,
    events::UiEvent,
    handshake::Capability,
    ids::{OperationId, PageCursor, RunId},
    namespaces::diagnostics::{
        ContextMessage, DiagnosticsEvent, DiagnosticsQuery, DiagnosticsReply, GetLiveUsage,
        GetRunContext, GetUsageSummary, LiveUsage, RunContextOverview, ThreadUsage, UsageRange,
        UsageTotals,
    },
    operation::OperationState,
    queries::{PageRequest, Query, QueryReply},
};

mod context;
mod live;
pub mod quota;
pub(crate) mod read;
mod thread_usage;
mod usage;

pub(crate) use thread_usage::{cost_hint, cost_text};

pub const TODAY_REFRESH: Duration = Duration::from_secs(60);

/// The open tab reads its range again this often, as the legacy Usage tab does.
pub const USAGE_REFRESH: Duration = Duration::from_secs(30);

/// The open Live view reads again this often, so its minutes keep moving.
pub const LIVE_REFRESH: Duration = Duration::from_secs(30);

/// The shortest time between two reads of the Live view that a notice asks for. Each
/// read goes through the ledger of the month, so a busy run must not set the pace.
pub const LIVE_REASK_GAP: Duration = Duration::from_secs(5);

const RANGES: [(UsageRange, &str); 6] = [
    (UsageRange::Today, "Today"),
    (UsageRange::Last7Days, "Last 7 days"),
    (UsageRange::Last30Days, "Last 30 days"),
    (UsageRange::ThisMonth, "This month"),
    (UsageRange::Last90Days, "Last 90 days"),
    (UsageRange::AllTime, "All time"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum View {
    #[default]
    Overview,
    Breakdown,
    Live,
    Context,
}

impl View {
    const ALL: [Self; 4] = [Self::Overview, Self::Breakdown, Self::Live, Self::Context];

    const fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Breakdown => "Breakdown",
            Self::Live => "Live",
            Self::Context => "Context",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Pending {
    Quota,
    Usage { generation: u64 },
    Today { generation: u64 },
    Thread { generation: u64 },
    Live { generation: u64 },
    Context { generation: u64, append: bool },
}

pub struct DiagnosticsFeature {
    pub quotas: quota::Quotas,
    next_tag: u64,
    pending: BTreeMap<u64, Pending>,
    view: View,
    range: UsageRange,
    distribution: Group,
    group: Group,
    show_models: bool,
    sort: Option<(SortColumn, bool)>,
    usage_read: Read,
    usage_request: Option<GetUsageSummary>,
    usage: Option<UsageData>,
    usage_due: bool,
    usage_next: Option<Instant>,
    run: Option<RunId>,
    context_read: Read,
    context_request: Option<GetRunContext>,
    context_missing: bool,
    overview: Option<RunContextOverview>,
    messages: Vec<ContextMessage>,
    next_cursor: Option<PageCursor>,
    context_due: bool,
    more_requested: bool,
    today_read: Read,
    today_request: Option<GetUsageSummary>,
    today: Option<UsageTotals>,
    today_next: Option<Instant>,
    thread_usage: thread_usage::ThreadUsageState,
    live_read: Read,
    live_request: Option<GetLiveUsage>,
    live: Option<LiveUsage>,
    live_due: bool,
    live_asked: Option<Instant>,
    live_next: Option<Instant>,
    live_window: usize,
}

impl Default for DiagnosticsFeature {
    fn default() -> Self {
        Self {
            quotas: quota::Quotas::default(),
            next_tag: 0,
            pending: BTreeMap::new(),
            view: View::default(),
            range: UsageRange::Last7Days,
            distribution: Group::Model,
            group: Group::Day,
            show_models: false,
            sort: None,
            usage_read: Read::default(),
            usage_request: None,
            usage: None,
            usage_due: true,
            usage_next: None,
            run: None,
            context_read: Read::default(),
            context_request: None,
            context_missing: false,
            overview: None,
            messages: Vec::new(),
            next_cursor: None,
            context_due: false,
            more_requested: false,
            today_read: Read::default(),
            today_request: None,
            today: None,
            today_next: None,
            thread_usage: thread_usage::ThreadUsageState::default(),
            live_read: Read::default(),
            live_request: None,
            live: None,
            live_due: true,
            live_asked: None,
            live_next: None,
            live_window: live::DEFAULT_WINDOW,
        }
    }
}

fn range_label(range: UsageRange) -> &'static str {
    RANGES
        .iter()
        .find(|(candidate, _)| *candidate == range)
        .map_or("", |(_, label)| label)
}

impl DiagnosticsFeature {
    pub fn today_cost(&self) -> Option<String> {
        self.today
            .as_ref()
            .map(|totals| usage::cells(totals).cost_label())
    }

    /// How long until the cost of today is asked again, so the screen wakes for it.
    pub fn today_refresh_in(&self, now: Instant) -> Option<Duration> {
        self.today_next
            .map(|next| next.saturating_duration_since(now))
    }

    fn keep_today(&mut self, ctx: &mut FeatureContext<'_>, visibility: Visibility, now: Instant) {
        if !visibility.available {
            return;
        }
        if self.today_next.is_none_or(|next| now >= next) {
            self.today_next = Some(now + TODAY_REFRESH);
            let request = GetUsageSummary {
                operation_id: self.today_read.begin("diagnostics-today"),
                range: UsageRange::Today,
            };
            self.today_request = Some(request);
        } else if !self.today_read.take_ask_again() {
            return;
        }
        if let Some(request) = self.today_request.clone() {
            let tag = self.tag(Pending::Today {
                generation: self.today_read.generation(),
            });
            ctx.query(
                tag,
                Query::Diagnostics(DiagnosticsQuery::GetUsageSummary(request)),
            );
        }
    }

    /// The usage of the range while its view is drawn: asked when due, asked again once
    /// the operation settled, and read anew every [`USAGE_REFRESH`] after an answer.
    fn keep_usage(&mut self, ctx: &mut FeatureContext<'_>, now: Instant) {
        let settled = matches!(self.usage_read.phase(), Phase::Ready | Phase::Failed(_));
        let stale = settled && self.usage_next.is_some_and(|next| now >= next);
        if std::mem::take(&mut self.usage_due) || stale {
            self.usage_next = Some(now + USAGE_REFRESH);
            self.begin_usage(ctx);
        } else if self.usage_read.take_ask_again() {
            self.ask_usage_again(ctx);
        }
    }

    /// How long until the settled usage is read again, so the screen wakes for it. A
    /// read that waits wakes the screen with its answer instead.
    fn usage_refresh_in(&self, now: Instant) -> Option<Duration> {
        let settled = matches!(self.usage_read.phase(), Phase::Ready | Phase::Failed(_));
        self.usage_next
            .filter(|next| settled && *next > now)
            .map(|next| next - now)
    }

    /// The cost of `thread`, once the core answered for it.
    pub fn thread_usage(&self, thread: &workbench_protocol::ids::ThreadId) -> Option<&ThreadUsage> {
        self.thread_usage.usage(thread)
    }

    /// How long until a notice about the selected thread may be asked, so the screen
    /// wakes for it.
    pub fn thread_usage_wake_in(&self, now: Instant) -> Option<Duration> {
        self.thread_usage.wake_in(now)
    }

    /// Lets a test that drives a real core read the thread again without waiting out the
    /// gap between two reads.
    #[doc(hidden)]
    pub fn set_thread_reask_gap(&mut self, gap: Duration) {
        self.thread_usage.set_gap(gap);
    }

    fn keep_thread_usage(
        &mut self,
        ctx: &mut FeatureContext<'_>,
        visibility: Visibility,
        now: Instant,
    ) {
        let active = visibility.available && !visibility.minimized;
        let selected = ctx.selection().thread.clone();
        if let Some(request) = self
            .thread_usage
            .due(selected.as_ref(), ctx.cache(), now, active)
        {
            let tag = self.tag(Pending::Thread {
                generation: self.thread_usage.generation(),
            });
            ctx.query(
                tag,
                Query::Diagnostics(DiagnosticsQuery::GetThreadUsage(request)),
            );
        }
    }

    /// The live usage while its view is drawn: asked when due (the first time, after a
    /// notice, never sooner than [`LIVE_REASK_GAP`] after the last ask), asked again once
    /// the operation settled, and read anew every [`LIVE_REFRESH`].
    fn keep_live(&mut self, ctx: &mut FeatureContext<'_>, now: Instant) {
        if *self.live_read.phase() == Phase::Waiting {
            if self.live_read.take_ask_again()
                && let Some(request) = self.live_request.clone()
            {
                self.send_live(ctx, request);
            }
            return;
        }
        let refresh = self.live_next.is_some_and(|next| now >= next);
        let gap_over = self
            .live_asked
            .is_none_or(|asked| now >= asked + LIVE_REASK_GAP);
        if refresh || (self.live_due && gap_over) {
            self.live_due = false;
            self.live_asked = Some(now);
            self.live_next = Some(now + LIVE_REFRESH);
            let request = GetLiveUsage {
                operation_id: self.live_read.begin("diagnostics-live"),
            };
            self.live_request = Some(request.clone());
            self.send_live(ctx, request);
        }
    }

    fn send_live(&mut self, ctx: &mut FeatureContext<'_>, request: GetLiveUsage) {
        let tag = self.tag(Pending::Live {
            generation: self.live_read.generation(),
        });
        ctx.query(
            tag,
            Query::Diagnostics(DiagnosticsQuery::GetLiveUsage(request)),
        );
    }

    /// How long until the settled live usage is read again, so the screen wakes for it.
    fn live_refresh_in(&self, now: Instant) -> Option<Duration> {
        if *self.live_read.phase() == Phase::Waiting {
            return None;
        }
        let mut wake = self
            .live_next
            .map(|next| next.saturating_duration_since(now));
        if self.live_due
            && let Some(asked) = self.live_asked
        {
            let gap = (asked + LIVE_REASK_GAP).saturating_duration_since(now);
            wake = Some(wake.map_or(gap, |wake| wake.min(gap)));
        }
        wake.filter(|wake| !wake.is_zero())
    }

    /// Whether a further page of the run's messages was asked and not yet answered.
    fn paging(&self) -> bool {
        let generation = self.context_read.generation();
        self.pending.values().any(|pending| {
            matches!(pending, Pending::Context { generation: asked, append: true } if *asked == generation)
        })
    }

    fn tag(&mut self, pending: Pending) -> u64 {
        self.next_tag += 1;
        self.pending.insert(self.next_tag, pending);
        self.next_tag
    }

    fn begin_usage(&mut self, ctx: &mut FeatureContext<'_>) {
        let operation_id = self.usage_read.begin("diagnostics-usage");
        let request = GetUsageSummary {
            operation_id,
            range: self.range,
        };
        let tag = self.tag(Pending::Usage {
            generation: self.usage_read.generation(),
        });
        ctx.query(
            tag,
            Query::Diagnostics(DiagnosticsQuery::GetUsageSummary(request.clone())),
        );
        self.usage_request = Some(request);
    }

    fn ask_usage_again(&mut self, ctx: &mut FeatureContext<'_>) {
        if let Some(request) = self.usage_request.clone() {
            let tag = self.tag(Pending::Usage {
                generation: self.usage_read.generation(),
            });
            ctx.query(
                tag,
                Query::Diagnostics(DiagnosticsQuery::GetUsageSummary(request)),
            );
        }
    }

    fn forget_context(&mut self) {
        self.context_read.forget();
        self.context_request = None;
        self.context_missing = false;
        self.overview = None;
        self.messages.clear();
        self.next_cursor = None;
    }

    fn begin_context(&mut self, ctx: &mut FeatureContext<'_>) {
        self.forget_context();
        let Some(run) = self.run.clone() else {
            return;
        };
        let operation_id: OperationId = self.context_read.begin("diagnostics-context");
        let request = GetRunContext {
            operation_id,
            run,
            page: PageRequest::default(),
        };
        let tag = self.tag(Pending::Context {
            generation: self.context_read.generation(),
            append: false,
        });
        ctx.query(
            tag,
            Query::Diagnostics(DiagnosticsQuery::GetRunContext(request.clone())),
        );
        self.context_request = Some(request);
    }

    fn ask_context(&mut self, ctx: &mut FeatureContext<'_>, cursor: Option<PageCursor>) {
        if let Some(mut request) = self.context_request.clone() {
            let append = cursor.is_some();
            request.page = PageRequest {
                cursor,
                ..PageRequest::default()
            };
            let tag = self.tag(Pending::Context {
                generation: self.context_read.generation(),
                append,
            });
            ctx.query(
                tag,
                Query::Diagnostics(DiagnosticsQuery::GetRunContext(request)),
            );
        }
    }

    fn active_phase(&self) -> &Phase {
        match self.view {
            View::Context => self.context_read.phase(),
            View::Live => self.live_read.phase(),
            _ => self.usage_read.phase(),
        }
    }

    /// Returns whether a refresh was asked for. Only the Context view has one: a read of
    /// one moment, paged, that a replacement would pull from under the reader. The other
    /// views read again by themselves, a failed read as well.
    fn toolbar(&mut self, ui: &mut egui::Ui) -> bool {
        let waiting = *self.active_phase() == Phase::Waiting;
        ui.horizontal_wrapped(|ui| {
            ui.label(h3("Usage"));
            if matches!(self.view, View::Overview | View::Breakdown) {
                let before = self.range;
                egui::ComboBox::from_id_salt("usage-range")
                    .selected_text(range_label(self.range))
                    .show_ui(ui, |ui| {
                        for (range, label) in RANGES {
                            ui.selectable_value(&mut self.range, range, label);
                        }
                    });
                if self.range != before {
                    self.usage_due = true;
                    self.usage = None;
                }
                if let Some(data) = &self.usage {
                    ui.label(muted(format!(
                        "{} to {}",
                        data.summary.from_day, data.summary.to_day
                    )));
                }
            }
            let refresh = self.view == View::Context && ui.button("Refresh").clicked();
            if waiting {
                crate::theme::widgets::spinner(ui);
            }
            refresh
        })
        .inner
    }

    fn usage_body(&mut self, ui: &mut egui::Ui) {
        if let Phase::Failed(text) = self.usage_read.phase() {
            crate::theme::widgets::reason_label(ui, crate::theme::tokens::palette().ERROR_FG, text);
        }
        let Some(data) = &self.usage else {
            if *self.usage_read.phase() == Phase::Waiting {
                ui.label(muted("Loading usage…"));
            }
            return;
        };
        if data.summary.totals.requests == 0 {
            ui.label(muted("No usage recorded for this period."));
            return;
        }
        ui.separator();
        if self.view == View::Overview {
            usage::render_overview(ui, data, &mut self.distribution);
        } else {
            usage::render_breakdown(
                ui,
                data,
                BreakdownControls {
                    group: &mut self.group,
                    show_models: &mut self.show_models,
                    sort: &mut self.sort,
                },
            );
        }
    }

    fn live_body(&mut self, ui: &mut egui::Ui) {
        if let Phase::Failed(text) = self.live_read.phase() {
            crate::theme::widgets::reason_label(ui, crate::theme::tokens::palette().ERROR_FG, text);
        }
        let Some(live) = &self.live else {
            if *self.live_read.phase() == Phase::Waiting {
                ui.label(muted("Loading usage…"));
            }
            return;
        };
        ui.separator();
        egui::ScrollArea::vertical()
            .id_salt("usage-live")
            .auto_shrink([false, true])
            .show(ui, |ui| {
                live::render(ui, live, &mut self.live_window, self.quotas.profiles());
            });
    }

    fn context_view(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>, refresh: bool) {
        let Some(thread) = ctx.selection().thread.clone() else {
            ui.label(muted(context::NO_THREAD));
            return;
        };
        let choices = context::run_choices(ctx.cache(), &thread);
        if choices.is_empty() {
            ui.label(context::NO_RUNS);
            return;
        }
        if self.run.is_none() {
            self.run = Some(choices[0].run.clone());
            self.context_due = true;
        }
        if let Some(run) = context::run_picker(ui, &choices, self.run.as_ref()) {
            self.run = Some(run);
            self.context_due = true;
        }
        ui.separator();
        if std::mem::take(&mut self.context_due) || refresh {
            self.begin_context(ctx);
        } else if self.context_read.take_ask_again() {
            self.ask_context(ctx, None);
        }
        if std::mem::take(&mut self.more_requested) && !self.paging() {
            let cursor = self.next_cursor.clone();
            self.ask_context(ctx, cursor);
        }
        self.context_body(ui);
    }

    fn context_body(&mut self, ui: &mut egui::Ui) {
        if self.context_missing {
            ui.label(context::NO_CHECKPOINT);
            return;
        }
        match self.context_read.phase() {
            Phase::Failed(text) => {
                crate::theme::widgets::reason_label(
                    ui,
                    crate::theme::tokens::palette().ERROR_FG,
                    text,
                );
                return;
            }
            Phase::Waiting if self.overview.is_none() => {
                crate::theme::widgets::spinner(ui);
                return;
            }
            _ => {}
        }
        let Some(overview) = &self.overview else {
            return;
        };
        context::overview_body(ui, overview, &self.messages);
        let messages = &self.messages;
        let more = paged_list(
            ui,
            "diagnostics-messages",
            (self.next_cursor.is_some() && !self.paging()).then_some(context::SHOW_MORE),
            |ui| {
                for message in messages.iter().filter(|message| message.listed) {
                    context::message_row(ui, message);
                }
            },
        );
        if more {
            self.more_requested = true;
        }
        ui.add_space(6.0);
        context::tools(ui, overview);
    }
}

impl Feature for DiagnosticsFeature {
    fn id(&self) -> FeatureId {
        FeatureId::Diagnostics
    }

    fn title(&self) -> &'static str {
        "Usage"
    }

    fn availability(&self) -> Availability {
        Availability::Implemented {
            required: &[Capability::Queries],
        }
    }

    fn as_diagnostics(&self) -> Option<&DiagnosticsFeature> {
        Some(self)
    }

    fn as_diagnostics_mut(&mut self) -> Option<&mut DiagnosticsFeature> {
        Some(self)
    }

    fn maintain(&mut self, ctx: &mut FeatureContext<'_>, visibility: Visibility) {
        let now = Instant::now();
        self.keep_today(ctx, visibility, now);
        self.keep_thread_usage(ctx, visibility, now);
        self.quotas
            .maintain(ctx, visibility, &mut self.next_tag, &mut self.pending);
    }

    fn selection_changed(&mut self, ctx: &mut FeatureContext<'_>) {
        let thread = ctx.selection().thread.as_ref();
        let kept = self.run.as_ref().is_some_and(|run| {
            ctx.cache()
                .runs
                .get(run.as_str())
                .is_some_and(|summary| Some(&summary.thread) == thread)
        });
        if !kept {
            self.run = None;
            self.context_due = false;
            self.forget_context();
        }
    }

    fn reset(&mut self) {
        self.quotas = quota::Quotas::default();
        self.pending.clear();
        self.today_read.forget();
        self.today = None;
        self.today_next = None;
        self.usage_read.forget();
        self.usage = None;
        self.usage_due = true;
        self.usage_next = None;
        self.thread_usage.forget();
        self.live_read.forget();
        self.live = None;
        self.live_due = true;
        self.live_asked = None;
        self.live_next = None;
        self.forget_context();
        self.context_due = self.run.is_some();
    }

    fn event(&mut self, event: &UiEvent) {
        self.quotas.event(event);
        let UiEvent::Diagnostics(event) = event else {
            return;
        };
        match event {
            DiagnosticsEvent::DiagnosticsOperationChanged { view } => {
                self.usage_read.event(view);
                self.context_read.event(view);
                self.today_read.event(view);
                self.thread_usage.read().event(view);
                self.live_read.event(view);
            }
            DiagnosticsEvent::UsageChanged { change } => {
                // The live usage changes with every request; the selected thread's only
                // when the notice names it, which needs the cache and is decided when asking.
                self.live_due = true;
                self.thread_usage.notice(change);
            }
            DiagnosticsEvent::ProviderQuotasChanged {} => self.live_due = true,
        }
    }

    fn reply(&mut self, tag: u64, reply: FeatureReply) {
        let Some(pending) = self.pending.remove(&tag) else {
            return;
        };
        if matches!(pending, Pending::Quota) {
            self.quotas.reply(reply);
            return;
        }
        let FeatureReply::Query(result) = reply else {
            return;
        };
        let reply = match result {
            Ok(QueryReply::Diagnostics(reply)) => reply,
            Ok(_) => return,
            Err(error) => {
                match pending {
                    Pending::Quota => {}
                    Pending::Usage { generation } => {
                        self.usage_read.fail(generation, error.message);
                    }
                    Pending::Today { generation } => {
                        self.today_read.fail(generation, error.message);
                    }
                    Pending::Thread { generation } => {
                        self.thread_usage.read().fail(generation, error.message);
                    }
                    Pending::Live { generation } => {
                        self.live_read.fail(generation, error.message);
                    }
                    Pending::Context { generation, .. } => {
                        self.context_read.fail(generation, error.message);
                    }
                }
                return;
            }
        };
        match (pending, reply) {
            (Pending::Usage { generation }, DiagnosticsReply::Usage(outcome)) => {
                if let Some(summary) =
                    self.usage_read
                        .accept(generation, &outcome.operation, outcome.data)
                {
                    self.usage = Some(UsageData::new(summary));
                }
            }
            (Pending::Today { generation }, DiagnosticsReply::Usage(outcome)) => {
                if let Some(summary) =
                    self.today_read
                        .accept(generation, &outcome.operation, outcome.data)
                {
                    self.today = Some(summary.totals);
                }
            }
            (Pending::Thread { generation }, DiagnosticsReply::ThreadUsage(outcome)) => {
                if let Some(usage) =
                    self.thread_usage
                        .read()
                        .accept(generation, &outcome.operation, outcome.data)
                {
                    self.thread_usage.accept(usage);
                }
            }
            (Pending::Live { generation }, DiagnosticsReply::LiveUsage(outcome)) => {
                if let Some(live) =
                    self.live_read
                        .accept(generation, &outcome.operation, outcome.data)
                {
                    self.live = Some(live);
                }
            }
            (Pending::Context { generation, append }, DiagnosticsReply::Context(outcome)) => {
                let not_found = matches!(
                    outcome.operation.state,
                    OperationState::Failed {
                        code: ErrorCode::NotFound
                    }
                );
                if let Some(context) =
                    self.context_read
                        .accept(generation, &outcome.operation, outcome.data)
                {
                    if append {
                        self.messages.extend(context.messages.items);
                    } else {
                        self.overview = Some(context.overview);
                        self.messages = context.messages.items;
                    }
                    self.next_cursor = context.messages.next_cursor;
                } else if not_found
                    && generation == self.context_read.generation()
                    && matches!(self.context_read.phase(), Phase::Failed(_))
                {
                    self.context_missing = true;
                } else if append
                    && outcome.operation.state == OperationState::Pending
                    && generation == self.context_read.generation()
                    && *self.context_read.phase() == Phase::Ready
                {
                    // The core evicted the settled operation the pages came from and began
                    // it anew, so its settling is never awaited here: read the run afresh.
                    self.context_due = true;
                }
            }
            _ => {}
        }
    }

    fn render(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        let refresh = self.toolbar(ui);
        ui.horizontal(|ui| {
            for view in View::ALL {
                if segment(ui, self.view == view, view.label()) {
                    self.view = view;
                }
            }
        });
        if self.view == View::Context {
            self.context_view(ui, ctx, refresh);
            return;
        }
        if self.view == View::Live {
            let now = Instant::now();
            self.keep_live(ctx, now);
            if let Some(wait) = self.live_refresh_in(now) {
                ui.ctx().request_repaint_after(wait);
            }
            self.live_body(ui);
            return;
        }
        let now = Instant::now();
        self.keep_usage(ctx, now);
        if let Some(wait) = self.usage_refresh_in(now) {
            ui.ctx().request_repaint_after(wait);
        }
        self.usage_body(ui);
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod live_tests;
