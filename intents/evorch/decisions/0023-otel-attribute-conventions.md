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
