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

## Decision

1. **部分採用**: 標準 attribute（`gen_ai.*`）は registry 準拠。registry に無い観測は `evorch.*` namespace で拡張
2. **信号範囲**: metrics + span。委譲親子 = span 親子。log 相当（SSE raw / message 本文）は対象外（ADR 0012 の raw 非永続化ポリシーと整合）。span 高頻度 export はサンプリング + ハード上限ポリシーで制御
3. **mapping 層集中**: 内部 event schema はドメイン語彙を保持し、`gen_ai.*` / `evorch.*` への変換・span 区間化・親子紐付けは専任変換層（crate）に集約。producer は opentelemetry crate を直接叩かない
4. **`evorch.*` の骨格**: 集計軸（session / task / agent_run / profile）+ 最小構造軸（低カーディナリティ列挙型の `evorch.delegation.depth` / `evorch.delegation.role` 程度。branch 等は必要時追加）。ID 系属性は span 限定、metrics には入れない（高カーディナリティ規律）
5. **実装 slice 二段分割**: ① mapping 層 + OTLP metrics exporter → ② mapping 層の span 化拡張 + span exporter（② は ① に依存）
6. **追従方針 pin + 意図的 bump**: mapping 表は対象 semconv リリースを明記（現基準 v1.37.0、2025-08）。自動追従しない。bump = release 差分精読 → mapping 表差分 → 検証。タイミングは slice ② 着手時と technology re-evaluation 連動
7. **検証**: 二段検証。主軸 = mapping 層の golden/snapshot test（pin 固定 fixture）+ cardinality guard（CI で metrics attribute whitelist 強制、ID 混入を静的ブロック）。副軸 = 各 slice DoD に debug exporter 経由の最小 OTLP E2E 1本

## Consequences

- v0.2（観測）に execution unit 2 件（metrics exporter / span exporter、依存付き）を queue seed する（operator 承認後）
- 親子関係の表現は trace に任せ、span attribute での再表現はしない。`evorch.*` の構造軸は metrics 面の集計（role 別 token 消費、depth 別 latency）専用
- 自己改善機能（inspect_session / inspect_provider 等、diagnostics-self-improvement）のローカル downsampled 集計にも委譲軸が供給される
- 内部 event schema（v0.1.1 landed の provider 観測含む）は OTel 名に侵食されず、標準改正の衝撃は mapping 層一箇所に閉じる

## 参考

- 属性 registry: https://github.com/open-telemetry/semantic-conventions-genai/blob/main/docs/registry/attributes/gen-ai.md
- agent spans: https://github.com/open-telemetry/semantic-conventions-genai/blob/main/docs/gen-ai/gen-ai-agent-spans.md
- metrics: https://github.com/open-telemetry/semantic-conventions-genai/blob/main/docs/gen-ai/gen-ai-metrics.md
- 関連 ADR: [0012 計測アーキテクチャ](0012-metrics-architecture.md) / [0014 config](0014-config-architecture.md) / [0015 verification two-layer](0015-verification-two-layer.md)
