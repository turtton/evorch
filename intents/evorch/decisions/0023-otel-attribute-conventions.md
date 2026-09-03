# ADR 0023: OTel attribute 規約 — gen-ai semconv 部分採用 + `evorch.*` 拡張 + mapping 層集中

## Status

Accepted（2026-08-30、grill session `metrics-attribute-semconv` の7決定から確定。interview 記録: `intents/evorch/interviews/metrics-attribute-semconv.json`）

## Context

ADR 0012 で計測の収集・保存方針（OTel Metrics API 語彙、ring buffer + downsampled SQLite、optional OTLP export）は確定したが、**attribute 命名規約は未決定**で、OTLP exporter 本体も「後続 slice」として未着手だった。subagent / 委譲まわりの計測を重視したいという要求（自己改善機能の分析基盤としても利用）があり、[gen-ai semconv registry](https://opentelemetry.io/docs/specs/semconv/registry/attributes/gen-ai/) の採用可否が争点となった。

事前実測（2026-08-30、open-telemetry/semantic-conventions-genai リポジトリ main 確認）:

- 標準に存在: `gen_ai.agent.{id,name,description,version}`、operations（`create_agent` / `invoke_agent` / `plan` / `execute_tool` 等）、`gen_ai.conversation.id`、tool 属性一式、agent invocation 単位 metrics（`gen_ai.invoke_agent.duration` / `inference_calls` / `tool_calls`）。multi-agent の metric 集計境界も明示（各 call は自身の invocation に一度だけ計上）
- 標準の思想: 階層は trace 親子（top-level `invoke_agent` → child `chat` → `execute_tool`）、集計は attribute
- 標準に存在しない: task、安定 run ID、agent role、delegation depth、branch 構造、明示的 delegation 関係属性
- 全 gen_ai 領域は Development stability
- **訂正（2026-09-03、slice ① 実装時の pin 精査）**: 上記の `gen_ai.agent.version` および agent invocation 系 metrics（`gen_ai.invoke_agent.duration` / `inference_calls` / `tool_calls`）は genai 拡張リポジトリ main の内容で、本設計の pin 先 semconv **v1.37.0**（`open-telemetry/semantic-conventions` v1.37.0 tag）には存在しない。v1.37.0 の gen_ai metrics は 5 種（`gen_ai.client.token.usage` / `gen_ai.client.operation.duration` / `gen_ai.server.request.duration` / `gen_ai.server.time_per_output_token` / `gen_ai.server.time_to_first_token`）に限定される。また `gen_ai.system` は v1.37.0 で deprecated（後継 `gen_ai.provider.name`）

## Decision

1. **部分採用**: 標準 attribute（`gen_ai.*`）は registry 準拠。registry に無い観測は `evorch.*` namespace で拡張
2. **信号範囲**: metrics + span。委譲親子 = span 親子。log 相当（SSE raw / message 本文）は対象外（ADR 0012 の raw 非永続化ポリシーと整合）。span 高頻度 export はサンプリング + ハード上限ポリシーで制御
3. **mapping 層集中**: 内部 event schema はドメイン語彙を保持し、`gen_ai.*` / `evorch.*` への変換・span 区間化・親子紐付けは専任変換層に集約。producer は opentelemetry crate を直接叩かない。**実現形（slice ①、2026-09-03）**: ADR 0016 の crate 不増方針（10+1）に従い event-bus 内 module `event_bus::otel`（写像層は常時 compile、SDK 接続層は `otel-exporter` feature gate）として実現。crate 分割は後続で再評価
4. **`evorch.*` の骨格（metrics whitelist は slice ① で最終確定）**: 集計軸（session / task / agent_run / profile）+ 最小構造軸（低カーディナリティ列挙型の `evorch.delegation.depth` / `evorch.delegation.role` 程度。branch 等は必要時追加）。ID 系属性は span 限定、metrics には入れない（高カーディナリティ規律）

   確定した OTLP metrics attribute whitelist（8 key、これが閉集合上限）:

   | attribute | local aggregation（ring/SQLite） | OTLP metrics | span（slice ②） |
   |---|---|---|---|
   | `gen_ai.operation.name` | - | ✅ `{"chat"}`（現状の閉 domain） | ✅ |
   | `gen_ai.provider.name` | - | ✅ `{"anthropic","openai","openai-compatible"}` 既知値＋未知値は固定 `"other"` へ正規化（pass-through 禁止） | ✅ |
    | `gen_ai.token.type` | - | ✅ `{"input","output"}` ＋固定拡張値 `{"cache_read","cache_write"}`（v1.37.0 well-known 外の evorch 拡張） | ✅ |
    | `gen_ai.request.model` | - | ✅（model は**必須次元のため全写像で常に付与**。写像層は形状ポリシー（非空・≤128 文字・printable ASCII・空白なし）で不適合値を固定値 `"other"` に畳み込み、emitter の初期化時 bounded registry（`OtelMetricsEmitter::new(provider, known_profiles, known_models)`、`MAX_MODEL_NAMES = 64` 上限・初期化後不変）非メンバーも exporter 層で `"other"` へ正規化する。**cardinality 保証の責務者は provider profile config の宣言 model 集合**であり、registry への注入は ADR 0014 配線 slice で行う。mapping 層 / validator は正規化防壁に限定） | ✅ |
    | `error.type` | - | ✅ `{rate_limited,http,timeout,invalid_response,transport,server,quota,auth,other}`（`Http{status}` は status を捨てる） | ✅ |
    | `evorch.profile.name` | ✅ | ✅（profile = 初期化時 bounded registry メンバーのみ emit: `OtelMetricsEmitter::new(provider, known_profiles, known_models)` で registry（`MAX_PROFILE_NAMES = 64` 上限、初期化後不変）を固定し、写像層の形状ポリシー（非空・≤64 文字・小字 ASCII alnum と `-_.`・先頭 alnum）適合 ∧ registry メンバーの profile のみ属性に残す。非メンバーは同属性のみ省略（measurement 保持）。registry への注入は config profile 集合から（ADR 0014 配線 slice） | ✅ |
   | `evorch.delegation.depth` | ✅ | whitelist 定義のみ確定。**slice ① では供給 event 不在のため未 emit**。値 domain は `0`..=`99` の decimal 文字列（leading zero なし）に固定 | ✅ |
   | `evorch.delegation.role` | ✅ | 同上。値 domain は閉集合 `{orchestrator, explorer, worker, reviewer}` に固定 | ✅ |
   | session / task / agent_run ID | ✅ | ❌ 不許可（ID 系、高カーディナリティ） | ✅（span 相関軸） |

    意図的省略（metrics label として採用しない）: `gen_ai.system`（deprecated）、`server.address` / `server.port`（provider event に非存在）、`finish_reason`（semconv metric 属性に非存在）。**`gen_ai.request.model` は省略しない**: model 名はメトリクスの必須次元という lead 判断により、上表の bounded registry 方式で採用する（Usage の `model` フィールドも同一ポリシーで emit）
5. **実装 slice 二段分割**: ① mapping 層 + OTLP metrics exporter → ② mapping 層の span 化拡張 + span exporter（② は ① に依存）
6. **追従方針 pin + 意図的 bump**: mapping 表は対象 semconv リリースを明記（現基準 v1.37.0、2025-08）。自動追従しない。bump = release 差分精読 → mapping 表差分 → 検証。タイミングは slice ② 着手時と technology re-evaluation 連動
7. **検証**: 二段検証。主軸 = mapping 層の golden/snapshot test（pin 固定 fixture）+ cardinality guard（CI で metrics attribute whitelist 強制、ID 混入を静的ブロック）。副軸 = 各 slice DoD に debug exporter 経由の最小 OTLP E2E 1本。**slice ① の E2E 実現形**: `opentelemetry_sdk::metrics::InMemoryMetricExporter`（debug 観察側）+ loopback OTLP HTTP receiver（wire 疎通側：POST `/v1/metrics` / protobuf content-type / 非空 body を assert）の二重 reader 構成。`opentelemetry-stdout` 0.32 は writer 注入不可・出力が人間向けテキストで assert 不能なため、この組合せを debug exporter 経路の実現形とする

**span 実現形（slice ②、2026-09-03、issue #57）**: 写像層に stateful span mapper `event_bus::otel::span`（決定 3 の crate 不増方針に従う。feature 非依存、opentelemetry crate 非参照）を追加し、決定 2 の「委譲親子 = span 親子」を以下の閉じた tree で実現した。

| key（mapper 内の論理参照） | name | kind | parent |
|---|---|---|---|
| `session:{id}` | `evorch.session` | Internal | root 固定（session↔run のリンクは存在しないため偽装しない） |
| `run:{run_id}` | `evorch.run {agent_name}` | Internal | `agent:{parent_run_id}`、親なしは root |
| `agent:{run_id}` | `invoke_agent {agent_name}` | Client | `run:{run_id}` |
| `request:{request_id}` | `chat {model}` | Client | `agent:{run_id}` |
| `tool:{call_id}` | `execute_tool {tool_name}` | Internal | `agent:{run_id}` |

mapper API は `SpanMapper::ingest(&Event) -> Vec<SpanAction>`（`Start` / `End`、属性は生成順固定で `HashMap` 不使用、`End.final_attributes` は開始属性＋in-flight 追加＋終端固有の**完全集合**）。開始・終了時刻は元 event の `EventMeta.wall_clock` のみから取る（決定性）。開始 event 側の追加は additive に実施: `LifecycleEvent::AgentRunStarted{run_id, parent_run_id, agent_name, role}` を追加（schema_version=1 維持、run lifetime は登録時に開始）し、ProviderEvent 4 attempt variant と `ToolStarted` / `ToolCompleted` に `#[serde(default)] run_id: Option<String>` を付与した。run_id の相関 threading は boundary context 契約として実現: runtime の `AgentInvocationContext`（`AgentModel::complete` 第1引数）、providers の `ObservationContext`（`ChatRequest.observation` は `#[serde(default, skip_serializing)]` で wire 非搭載、wire 生成は `to_wire_request` 明示変換を通すダブル防御）、tools の `ToolExecutionContext`（`ToolExecutor::execute` 必須引数）。

属性規律（決定 4 の span 面）は三層: `SPAN_ATTRIBUTE_WHITELIST`（ソート済み 18 key の閉集合、二分探索で検査）+ `validate_span_attributes` による key 別閉 domain 検査（`gen_ai.operation.name` 3 値 / role 4 値 / `gen_ai.provider.name` 5 値 / `error.type` 13 値 / `evorch.delegation.depth` 0..=99）+ raw-content denylist（prompt / completion / message / content / body / credential / token 等、正規例外 `gen_ai.usage.*`）。ID 系属性（session / task / agent_run / request）は span 層限定で、metrics whitelist との非交差を cardinality guard が静的に保証する。`error.type` は metrics 層と同一の `map_failure` snake_case 分類に安定 4 値（`agent_run_error` / `tool_error` / `session_failed` / `span_budget_evicted`）を加え、理由文字列等の自由文字列は属性に含めない。`evorch.delegation.depth` は mapper が parent graph から checked 算出（overflow 時 cap 99）。`evorch.task.id` は `BackgroundTaskStarted` が既知 run と一致した際に run span の `final_attributes` 末尾へ追加される。

sampling / hard limits（決定 2、ADR 0012）の実効経路は admission 一箇所: run sampling（決定的 FNV-1a、ratio 既定 1.0、子 run は親判定を相続）→ per-run in-flight 128（request / tool の operational span のみ計数）→ global in-flight 4096 → admission window 10_000/60s → 属性 caps（32 件 / span 合計 16KiB / 値 1KiB。超過属性は drop、String truncate はしない）→ lifetime 30min 超過は `End(Error, error.type=span_budget_evicted)` を発行して eviction → tombstone 4096（key → 拒否 kind を保持する bounded map。reject 済み key の後続 End は無 warn no-op、後着 subtree 開始は記録済み kind を no-warn で replay し、cap evict 後は既存規約（`UnknownParent` 等）へ落ちる）。写像不能事象は typed drop 10 種（`MissingRunId` / `UnknownParent` / `UnknownSpanEnd` / `DuplicateSpan` / `SampledOut` / `BudgetInFlightPerRun` / `BudgetInFlightGlobal` / `BudgetWindow` / `BudgetAttributes` / `BudgetEvicted`）として記録し、warn は同一 kind 60 秒に 1 回へ rate limit する。run + agent の 2 span 開始は 1 回の admission として扱い、片方でも許容できない場合は両方拒否して 1 件の typed drop（budget 系 or `DuplicateSpan`、key は run span）のみを記録して partial tree を作らない。委譲 run は親 `agent:{parent_run_id}` が open の場合のみ受理し、open でない親（未見 / 終了済み / 未 admission）への委譲は子 subtree ごと `UnknownParent` として drop する — `UnknownParent` を再利用するため **typed drop の総数は 10 種から不変**。run 終端（`AgentRunStateChanged` to=Done/Error）ではその run の per-run 相関エントリ（sampling 判定 / delegation depth）を mapper 帳簿から解放し、完了 run 数に比例したメモリ残留をなくす（active な子 subtree は開始時点で自エントリを保持するため壊れない）。

SDK 接続層（feature `otel-exporter`）: `OtelSpanEmitter`（open `HashMap<SpanKey,(Span,SpanContext)>`、親接続は `Context::with_remote_span_context` で trace_id 継承と parent_span_id 連鎖は SDK に委ねる。未知 parent は warn+root 開始、未知 End / 二重 End は no-op+warn の防御層）+ `build_otlp_tracer_provider`（OTel 0.32 の `with_endpoint` は `/v1/traces` を自動付加しないため手動 normalize、`SimpleSpanProcessor` + `Sampler::AlwaysOn` — sampling は mapper 側 admission 済みのため SDK 側は重ねない）+ `build_in_memory_tracer_provider`。依存追加なし（3 つの opentelemetry crate への `trace` feature 追加のみ）。semconv pin は GenAI spans v1.37.0（`span/mod.rs` module doc に canonical URL を記載、決定 6 の pin ポリシー）。

検証（決定 7 の span 拡張）: 手書き golden fixture 10 本（`tests/otel_span_golden/`、runner `tests/otel_span_golden.rs`、循環検査なし）+ cardinality guard へ span 5 本追加（metrics whitelist との非交差 / ソート・一意 / denylist 非含有 / `evorch.*` exact 集合 / validator 直接拒否）+ span E2E（`tests/otel_span_e2e.rs`、plain `#[test]` + std `TcpListener` の loopback receiver で POST `/v1/traces`・`application/x-protobuf`・非空 body を assert、InMemory で parent 連鎖・単一 trace_id を assert）+ runtime integration（実 provider wire 経由の run_id 相関、`runtime/tests/provider_correlation.rs` / `correlation_threading.rs`）。

O/R トレードオフとして残る既知注記: (a) `LifecycleEvent::Delegated` の topology mismatch 検証は現行 schema（`session_id` / `target` のみ）では実装不能のため no-op とし、逸脱候補として `span/mod.rs` doc に記録した。(b) routing crate に production `AgentModel` 実装は存在しない（依存方向 routing→runtime なし）ため、correlation は境界契約 + integration test で実証する形に留めた。(c) subscription wiring（EventBus→mapper→emitter の常駐配線）は ADR 0014 の config 有効化経路として後続 slice に deferred し、本 slice は mapper / emitter を test 駆動で提供する。(d) `storage/writer.rs` の `clippy::large_enum_variant` allow は `AgentRunStarted` による `EventKind` 大型化由来で、Box 化回避の意図的措置。

## Consequences

- v0.2（観測）に execution unit 2 件（metrics exporter / span exporter、依存付き）を queue seed する（operator 承認後）
- 親子関係の表現は trace に任せ、span attribute での再表現はしない。`evorch.*` の構造軸は metrics 面の集計（role 別 token 消費、depth 別 latency）専用
- 自己改善機能（inspect_session / inspect_provider 等、diagnostics-self-improvement）のローカル downsampled 集計にも委譲軸が供給される
- 内部 event schema（v0.1.1 landed の provider 観測含む）は OTel 名に侵食されず、標準改正の衝撃は mapping 層一箇所に閉じる
- **slice ① landed（2026-09-03、issue #55）**: histogram bucket boundaries に v1.37.0 advisory ExplicitBucketBoundaries を採用。client 側 TTFT は v1.37.0 に無いため `evorch.client.time_to_first_token`（f64 histogram、単位 `s`）として evorch.* 拡張で表現。runtime への subscribe 配線（ADR 0014 の config 有効化経路）は slice ① の scope 外として後続に送る。初版では `gen_ai.request.model` を「意図的省略」としたが、lead review で「model はメトリクスの必須次元」と覆され、emitter 初期化時 `known_models` bounded registry（上限 64、非メンバーは `"other"` 正規化）方式で whitelist 復活させた（profile.name と同型の責務境界）

## 参考

- 属性 registry: https://github.com/open-telemetry/semantic-conventions-genai/blob/main/docs/registry/attributes/gen-ai.md
- agent spans: https://github.com/open-telemetry/semantic-conventions-genai/blob/main/docs/gen-ai/gen-ai-agent-spans.md
- metrics: https://github.com/open-telemetry/semantic-conventions-genai/blob/main/docs/gen-ai/gen-ai-metrics.md
- 関連 ADR: [0012 計測アーキテクチャ](0012-metrics-architecture.md) / [0014 config](0014-config-architecture.md) / [0015 verification two-layer](0015-verification-two-layer.md)
