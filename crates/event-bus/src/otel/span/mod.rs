//! stateful span mapper (issue #57)。
//!
//! イベントバス上の [`Event`] を GenAI spans semantic conventions v1.37.0
//! ([`super::SEMCONV_PIN`]) の span tree 構築 action 列
//! ([`SpanAction`]) へ写像する。metrics 層 ([`super::map_event`]) とは独立の
//! stateful 写像であり、run / agent / request / tool / session の span 階層を
//! mapper 内部 state で追跡する。
//!
//! Pin 先 (canonical URL): GenAI spans v1.37.0
//! <https://github.com/open-telemetry/semantic-conventions/blob/v1.37.0/docs/gen-ai/gen-ai-spans.md>
//! (raw: <https://raw.githubusercontent.com/open-telemetry/semantic-conventions/v1.37.0/docs/gen-ai/gen-ai-spans.md>)。
//!
//! # span tree
//!
//! | key | 開始 | 終了 | name | kind | parent |
//! |---|---|---|---|---|---|
//! | `session:{id}` | `LifecycleEvent::Started` | `Completed` / `Failed` | `evorch.session` | Internal | root (固定) |
//! | `run:{run_id}` | `AgentRunStarted` | run 終端 (`AgentRunStateChanged` to=Done/Error) | `evorch.run {agent_name}` | Internal | `agent:{parent_run_id}` or root |
//! | `agent:{run_id}` | `AgentRunStarted` | run 終端 | `invoke_agent {agent_name}` | Client | `run:{run_id}` |
//! | `request:{request_id}` | `RequestStarted` (run_id=Some) | `RequestCompleted` / `RequestFailed` | `chat {model}` | Client | `agent:{run_id}` |
//! | `tool:{call_id}` | `ToolStarted` (run_id=Some) | `ToolCompleted` | `execute_tool {tool_name}` | Internal | `agent:{run_id}` |
//!
//! session↔run のリンクは存在しないため、session span の parent を run に
//! 偽装しない (独立 root)。
//!
//! # 決定性
//!
//! 開始 / 終了時刻はすべて元イベントの `EventMeta.wall_clock` から取り、
//! mapper 内部で `SystemTime::now()` を呼ばない。warn の rate limit も
//! イベント時刻で判定する。
//!
//! # 属性順序
//!
//! 各 action の属性列は生成順を固定する (下記列挙順)。`HashMap` は
//! 属性列に使用しない。
//!
//! - session: `evorch.session.id`
//! - run: `evorch.agent_run.id`, (`evorch.parent_agent_run.id`: Some 時のみ),
//!   `evorch.agent.name`, `evorch.delegation.role`, `evorch.delegation.depth`
//! - agent: `gen_ai.operation.name`=`invoke_agent`,
//!   `gen_ai.provider.name`=`evorch`, `gen_ai.agent.name`,
//!   `evorch.agent_run.id`, `evorch.delegation.role`, `evorch.delegation.depth`
//! - request: `gen_ai.operation.name`=`chat`, `gen_ai.provider.name`
//!   (normalize 後), `gen_ai.request.model`, `evorch.agent_run.id`,
//!   `evorch.request.id`
//! - tool: `gen_ai.operation.name`=`execute_tool`,
//!   `gen_ai.provider.name`=`evorch`, `gen_ai.tool.name`,
//!   `gen_ai.tool.call.id`, `evorch.agent_run.id`
//! - 終端固有: `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`,
//!   `gen_ai.response.finish_reasons` (request 成功終端。semconv v1.37.0 の
//!   string array 型に従い要素 1 値の文字列配列として記録する),
//!   `error.type` (Error 終端), `evorch.task.id` (in-flight 記録分)
//!
//! `End.final_attributes` は閉鎖時点の属性**完全集合** (開始属性 +
//! in-flight 追加 + 終端固有) を生成順で返す。
//!
//! # `evorch.delegation.depth`
//!
//! mapper が parent graph (`agent:{parent_run_id}` の depth + 1) から
//! checked 算出し、cap 99 (checked 加算が overflow したら 99 固定)。
//! parent が未知の run は depth 0 として開始する (parent link 自体は
//! 設計どおり付与する)。
//!
//! # typed drop
//!
//! span action に写像できない事象は [`SpanDrop`] として記録し
//! ([`SpanMapper::drain_drops`] で取得)、`tracing::warn!` を
//! 同一 [`SpanDropKind`] につき 60 秒に 1 回へ rate limit する。
//!
//! | 種別 | 事象 |
//! |---|---|
//! | `MissingRunId` | `run_id: None` の `RequestStarted` / `ToolStarted` |
//! | `UnknownParent` | 親 `agent:{run_id}` が未知の request / tool 開始 |
//! | `UnknownSpanEnd` | 開始済み span が無い End 要求 |
//! | `DuplicateSpan` | 既に open な同一 key の再開始 |
//! | `SampledOut` | run sampling 判定により subtree Start を拒否 |
//! | `BudgetInFlightPerRun` | run 内 request / tool in-flight 上限超過 |
//! | `BudgetInFlightGlobal` | mapper 全体の open span 上限超過 |
//! | `BudgetWindow` | 60 秒 admission window の上限超過 |
//! | `BudgetAttributes` | whitelist または属性 hard limit により属性を破棄 |
//! | `BudgetEvicted` | lifetime 上限超過 span を Error 終了 |
//!
//! # エラー分類 (`error.type`)
//!
//! request 失敗は metrics 層と同一の [`super::map_failure`] 分類
//! (snake_case variant 名、`Http` の status は捨てる) を用いる。
//! run / agent / tool / session の失敗は安定分類値
//! `agent_run_error` / `tool_error` / `session_failed` を用いる。
//! 理由文字列や detail 等の自由文字列は属性に含めない。
//!
//! # 非写像 event
//!
//! `FirstTokenObserved` (metrics のみ対象) / `ProviderFallback` /
//! `FallbackTriggered` / `BackgroundTaskCompleted` /
//! `BackgroundTaskCancelled` / `ApprovalRequested` / `ApprovalResolved` /
//! `ExecutionDenied` / `Message` / `Usage` / `Fault` / `AgentMessage` は
//! 空 [`Vec`] を返す。`LifecycleEvent::Delegated` は topology 検証のみを
//! 意図するが、session↔run のリンクが存在しないため本 slice では
//! state 変更も action 生成も行わない (設計からの逸脱候補として報告)。
//!
//! # 非ゴール
//!
//! 本モジュールは telemetry SDK に依存しない (feature 非依存制約)。
//! OTLP exporter への接続は後続 slice で
//! `SpanAction` を消費する形で行う。

mod action;
mod lifecycle;
mod mapper;
mod provider;
mod span_attrs;
mod span_budget;
mod state;
mod tool;

#[cfg(test)]
#[path = "tests/mod.rs"]
mod tests;

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use crate::event::Event;

pub use action::{
    FiniteF64, SpanAction, SpanAttribute, SpanAttributeValue, SpanDrop, SpanDropKind, SpanKey,
    SpanKind, SpanStatus,
};
pub use span_attrs::{SPAN_ATTRIBUTE_WHITELIST, SpanAttributeViolation, validate_span_attributes};
pub use span_budget::SpanBudget;

/// warn の rate limit 間隔 (同一 [`SpanDropKind`] につき)。
const WARN_INTERVAL: Duration = Duration::from_secs(60);

/// 開始済み span の内部 state。
#[derive(Debug)]
pub(super) struct OpenSpan {
    attributes: Vec<SpanAttribute>,
    in_flight: Vec<SpanAttribute>,
    started_at: SystemTime,
    sequence: u64,
    run_id: Option<String>,
}

/// イベントを span tree 構築 action 列へ写像する stateful mapper。
#[derive(Debug)]
pub struct SpanMapper {
    pub(super) open: HashMap<SpanKey, OpenSpan>,
    pub(super) agent_depth: HashMap<String, u32>,
    pub(super) drops: Vec<SpanDrop>,
    pub(super) last_warned: HashMap<SpanDropKind, SystemTime>,
    pub(super) budget: SpanBudget,
    pub(super) sampling_ratio: f64,
    pub(super) sampling_decisions: HashMap<String, bool>,
    pub(super) window_started_at: Option<SystemTime>,
    pub(super) admitted_in_window: usize,
    pub(super) tombstones: HashMap<SpanKey, u64>,
    pub(super) tombstone_sequence: u64,
    pub(super) span_sequence: u64,
}

impl Default for SpanMapper {
    fn default() -> Self {
        Self {
            open: HashMap::new(),
            agent_depth: HashMap::new(),
            drops: Vec::new(),
            last_warned: HashMap::new(),
            budget: SpanBudget::default(),
            sampling_ratio: 1.0,
            sampling_decisions: HashMap::new(),
            window_started_at: None,
            admitted_in_window: 0,
            tombstones: HashMap::new(),
            tombstone_sequence: 0,
            span_sequence: 0,
        }
    }
}

impl SpanMapper {
    /// 空の state で mapper を生成する。
    pub fn new() -> Self {
        Self::default()
    }

    /// 指定 hard limits で mapper を生成する。
    pub fn with_budget(budget: SpanBudget) -> Self {
        Self {
            budget,
            ..Self::default()
        }
    }

    /// run 単位の決定的 sampling ratio で mapper を生成する。
    pub fn with_sampling_ratio(ratio: f64) -> Self {
        let sampling_ratio = if ratio.is_nan() {
            1.0
        } else {
            ratio.clamp(0.0, 1.0)
        };
        Self {
            sampling_ratio,
            ..Self::default()
        }
    }

    /// イベントを写像し、生成された span action 列を返す。
    ///
    /// 写像対象外の事象は空 [`Vec`] となり、破棄されたものは
    /// [`SpanMapper::drain_drops`] で取得できる。
    pub fn ingest(&mut self, event: &Event) -> Vec<SpanAction> {
        let mut actions = self.audit_lifetimes(event.meta.wall_clock);
        actions.extend(self.map_event(event));
        actions
    }

    /// 記録済みの typed drop を取り出す (呼び出し後は空になる)。
    pub fn drain_drops(&mut self) -> Vec<SpanDrop> {
        std::mem::take(&mut self.drops)
    }

    /// drop を 1 件記録し、rate limit に基づき warn する。
    pub(super) fn record_drop(&mut self, kind: SpanDropKind, key: SpanKey, at: SystemTime) {
        if self.should_warn(kind, at) {
            tracing::warn!(
                target: "evorch::otel::span",
                drop_kind = ?kind,
                span_key = ?key,
                "span mapper dropped an event without emitting a span action"
            );
        }
        self.drops.push(SpanDrop { kind, key });
    }

    /// 同一 [`SpanDropKind`] の warn が 60 秒以上空いているかを判定する。
    fn should_warn(&mut self, kind: SpanDropKind, at: SystemTime) -> bool {
        let due = match self.last_warned.get(&kind) {
            Some(last) => at
                .duration_since(*last)
                .is_ok_and(|elapsed| elapsed >= WARN_INTERVAL),
            None => true,
        };
        if due {
            self.last_warned.insert(kind, at);
        }
        due
    }
}
