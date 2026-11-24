//! Desktop feature of the `diagnostics` area (PR 13): the usage of the core over a
//! range, and what a run holds in its saved context. Read only.
//!
//! Both are read operations ([`read`]): the screen asks, the core answers at once
//! with the operation, and the data arrives when the area's event says it
//! settled. Nothing here sends a command.
//!
//! The footer's cost of today is a read of its own, asked whether or not the tab is
//! drawn and again every [`TODAY_REFRESH`], as the embedded workbench refreshes it.
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use super::layout::{bounded_list, paged_list};
use super::{Availability, Feature, FeatureContext, FeatureId, FeatureReply, Visibility};
use read::{Read, phase_line};
use workbench_protocol::{
    events::UiEvent,
    handshake::Capability,
    ids::{OperationId, PageCursor, RunId},
    namespaces::diagnostics::{
        ContextMessage, ContextMessageRole, DiagnosticsEvent, DiagnosticsQuery, DiagnosticsReply,
        GetRunContext, GetUsageSummary, RunContextOverview, UsageRange, UsageSummary, UsageTotals,
    },
    queries::{PageRequest, Query, QueryReply},
};

pub(crate) mod read;

pub const TODAY_REFRESH: Duration = Duration::from_secs(60);

const RANGES: [(UsageRange, &str); 6] = [
    (UsageRange::Today, "今日"),
    (UsageRange::Last7Days, "直近 7 日"),
    (UsageRange::Last30Days, "直近 30 日"),
    (UsageRange::ThisMonth, "今月"),
    (UsageRange::Last90Days, "直近 90 日"),
    (UsageRange::AllTime, "すべて"),
];

#[derive(Debug, Clone, Copy)]
enum Pending {
    Usage { generation: u64 },
    Today { generation: u64 },
    Context { generation: u64, append: bool },
}

pub struct DiagnosticsFeature {
    next_tag: u64,
    pending: BTreeMap<u64, Pending>,
    range: UsageRange,
    usage_read: Read,
    usage_request: Option<GetUsageSummary>,
    usage: Option<UsageSummary>,
    usage_due: bool,
    run_input: String,
    context_read: Read,
    context_request: Option<GetRunContext>,
    overview: Option<RunContextOverview>,
    messages: Vec<ContextMessage>,
    next_cursor: Option<PageCursor>,
    context_due: bool,
    more_requested: bool,
    today_read: Read,
    today_request: Option<GetUsageSummary>,
    today: Option<UsageTotals>,
    today_next: Option<Instant>,
}

impl Default for DiagnosticsFeature {
    fn default() -> Self {
        Self {
            next_tag: 0,
            pending: BTreeMap::new(),
            range: UsageRange::Last7Days,
            usage_read: Read::default(),
            usage_request: None,
            usage: None,
            usage_due: true,
            run_input: String::new(),
            context_read: Read::default(),
            context_request: None,
            overview: None,
            messages: Vec::new(),
            next_cursor: None,
            context_due: false,
            more_requested: false,
            today_read: Read::default(),
            today_request: None,
            today: None,
            today_next: None,
        }
    }
}

/// The run id typed in. Whether it is one is for the core to say: it holds the rule
/// for what a run id is, and a refusal comes back as text shown under the input.
fn run_id(input: &str) -> Option<RunId> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    RunId::new(input).ok()
}

fn range_label(range: UsageRange) -> &'static str {
    RANGES
        .iter()
        .find(|(candidate, _)| *candidate == range)
        .map_or("", |(_, label)| label)
}

fn role_label(role: ContextMessageRole) -> &'static str {
    match role {
        ContextMessageRole::System => "system",
        ContextMessageRole::User => "user",
        ContextMessageRole::Assistant => "assistant",
        ContextMessageRole::ToolResult => "tool result",
    }
}

/// `1234567` micro-USD as `$1.234567`.
fn dollars(micro_usd: u64) -> String {
    format!("${}.{:06}", micro_usd / 1_000_000, micro_usd % 1_000_000)
}

/// What a set of requests used, in one line. The cost is a lower bound when
/// some tokens had no price or some requests reported no usage.
fn totals_line(totals: &UsageTotals) -> String {
    let mut line = format!(
        "{} 件 (失敗 {}) · 入力 {} / 出力 {} トークン · キャッシュ読 {} / 書 {} · 推論 {} · 記録された費用 {}",
        totals.requests,
        totals.failed,
        totals.input_tokens,
        totals.output_tokens,
        totals.cache_read_tokens,
        totals.cache_write_tokens,
        totals.reasoning_tokens,
        dollars(totals.recorded_cost_micro_usd),
    );
    if totals.unpriced_tokens > 0 || totals.unreported_requests > 0 {
        line.push_str(&format!(
            " (下限: 価格のないトークン {} / 使用量未報告 {} 件)",
            totals.unpriced_tokens, totals.unreported_requests
        ));
    }
    line
}

impl DiagnosticsFeature {
    pub fn today_cost(&self) -> Option<String> {
        self.today.as_ref().map(|totals| {
            #[allow(clippy::cast_precision_loss)]
            let dollars = totals.recorded_cost_micro_usd as f64 / 1_000_000.0;
            crate::format::cost_label(
                dollars,
                totals.unpriced_tokens > 0 || totals.unreported_requests > 0,
            )
        })
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

    fn begin_context(&mut self, ctx: &mut FeatureContext<'_>) {
        let Some(run) = run_id(&self.run_input) else {
            self.context_read.forget();
            self.context_request = None;
            return;
        };
        self.overview = None;
        self.messages.clear();
        self.next_cursor = None;
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
}

impl Feature for DiagnosticsFeature {
    fn id(&self) -> FeatureId {
        FeatureId::Diagnostics
    }

    fn title(&self) -> &'static str {
        "Diagnostics"
    }

    fn availability(&self) -> Availability {
        Availability::Implemented {
            required: &[Capability::Queries],
        }
    }

    fn as_diagnostics(&self) -> Option<&DiagnosticsFeature> {
        Some(self)
    }

    fn maintain(&mut self, ctx: &mut FeatureContext<'_>, visibility: Visibility) {
        self.keep_today(ctx, visibility, Instant::now());
    }

    fn reset(&mut self) {
        self.pending.clear();
        self.today_read.forget();
        self.today = None;
        self.today_next = None;
        self.usage_read.forget();
        self.context_read.forget();
        self.usage = None;
        self.usage_due = true;
        self.overview = None;
        self.messages.clear();
        self.next_cursor = None;
        self.context_due = !self.run_input.trim().is_empty();
    }

    fn event(&mut self, event: &UiEvent) {
        let UiEvent::Diagnostics(DiagnosticsEvent::DiagnosticsOperationChanged { view }) = event
        else {
            return;
        };
        self.usage_read.event(view);
        self.context_read.event(view);
        self.today_read.event(view);
    }

    fn reply(&mut self, tag: u64, reply: FeatureReply) {
        let Some(pending) = self.pending.remove(&tag) else {
            return;
        };
        let FeatureReply::Query(result) = reply else {
            return;
        };
        let reply = match result {
            Ok(QueryReply::Diagnostics(reply)) => reply,
            Ok(_) => return,
            Err(error) => {
                match pending {
                    Pending::Usage { generation } => {
                        self.usage_read.fail(generation, error.message);
                    }
                    Pending::Today { generation } => {
                        self.today_read.fail(generation, error.message);
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
                    self.usage = Some(summary);
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
            (Pending::Context { generation, append }, DiagnosticsReply::Context(outcome)) => {
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
                }
            }
            _ => {}
        }
    }

    fn render(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        ui.strong("使用量");
        let before = self.range;
        egui::ComboBox::from_id_salt("diagnostics-range")
            .selected_text(range_label(self.range))
            .show_ui(ui, |ui| {
                for (range, label) in RANGES {
                    ui.selectable_value(&mut self.range, range, label);
                }
            });
        if self.range != before || ui.button("再読み込み").clicked() {
            self.usage_due = true;
        }
        if std::mem::take(&mut self.usage_due) {
            self.begin_usage(ctx);
        } else if self.usage_read.take_ask_again() {
            self.ask_usage_again(ctx);
        }
        if let Some(line) = phase_line(self.usage_read.phase()) {
            ui.label(line);
        }
        if let Some(summary) = &self.usage {
            ui.label(format!(
                "{} — {} から {}",
                range_label(summary.range),
                summary.from_day,
                summary.to_day
            ));
            ui.label(totals_line(&summary.totals));
            if summary.days_omitted > 0 {
                ui.label(format!(
                    "古い {} 日分は表示していません。",
                    summary.days_omitted
                ));
            }
            bounded_list(ui, "diagnostics-days", |ui| {
                for day in summary.days.iter().rev() {
                    ui.label(format!("{} · {}", day.day, totals_line(&day.totals)));
                }
            });
            ui.separator();
            ui.strong("モデル別");
            if summary.models_omitted > 0 {
                ui.label(format!(
                    "使用の少ない {} モデルは表示していません。",
                    summary.models_omitted
                ));
            }
            bounded_list(ui, "diagnostics-models", |ui| {
                for model in &summary.models {
                    ui.label(format!(
                        "{} / {} · {}",
                        model.provider,
                        model.model,
                        totals_line(&model.totals)
                    ));
                }
            });
        }

        ui.separator();
        ui.strong("run の保存済みコンテキスト");
        // Closed until asked for: every available feature is stacked in one column, so
        // an input drawn all the time would share the screen with every other one.
        egui::CollapsingHeader::new("run id を指定して開く")
            .default_open(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.run_input)
                            .hint_text("run id (例: run-…)"),
                    );
                    let submitted = response.lost_focus()
                        && ui.input(|input| input.key_pressed(egui::Key::Enter));
                    if ui.button("開く").clicked() || submitted {
                        self.context_due = true;
                    }
                });
            });
        if std::mem::take(&mut self.context_due) {
            self.begin_context(ctx);
        } else if self.context_read.take_ask_again() {
            self.ask_context(ctx, None);
        }
        if std::mem::take(&mut self.more_requested) {
            let cursor = self.next_cursor.clone();
            self.ask_context(ctx, cursor);
        }
        if let Some(line) = phase_line(self.context_read.phase()) {
            ui.label(line);
        }
        if let Some(overview) = &self.overview {
            ui.label(format!(
                "{}{} · {} · {} · {} メッセージ · 圧縮 {} 回",
                overview.role,
                overview
                    .category
                    .as_ref()
                    .map_or(String::new(), |category| format!(" ({category})")),
                overview.phase,
                overview.selected_model.as_deref().unwrap_or("モデル未記録"),
                overview.message_count,
                overview.compaction_checkpoints,
            ));
            let segments = overview
                .segments
                .iter()
                .map(|segment| format!("{} {}", segment.label, segment.tokens))
                .collect::<Vec<_>>()
                .join(" / ");
            if !segments.is_empty() {
                ui.label(format!("トークン内訳(推定): {segments}"));
            }
            if !overview.tools.is_empty() {
                ui.label(format!(
                    "ツール {} 個{}",
                    overview.tools.len() + overview.tools_omitted as usize,
                    if overview.tools_omitted > 0 {
                        " (一部のみ表示)"
                    } else {
                        ""
                    }
                ));
            }
            let messages = &self.messages;
            let more = paged_list(
                ui,
                "diagnostics-messages",
                self.next_cursor.is_some().then_some("さらに読み込む"),
                |ui| {
                    for message in messages.iter().filter(|message| message.listed) {
                        ui.label(format!(
                            "#{} {} ({} トークン) {}",
                            message.index,
                            role_label(message.role),
                            message.tokens,
                            message.summary
                        ));
                    }
                },
            );
            if more {
                self.more_requested = true;
            }
        }
    }
}

#[cfg(test)]
mod tests;
