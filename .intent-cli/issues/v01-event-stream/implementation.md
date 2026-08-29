# v01-event-stream Implementation Packet

## Goal

Agent Kernel のイベントバックボーンとなる `crates/event-bus/` を実装する。全コンポーネントが emit / subscribe する型付き event stream を tokio broadcast ベースで提供し、event schema（agent lifecycle / message / tool call / token usage / provider request-response / fault）を serde で定義する。ADR 0012 の「メモリ ring buffer で raw 保持」する計測収集層の土台（bounded ring buffer と in-memory 集計の観測点）もここに置く。transport は in-process のみに確定し、その理由と将来の拡張方針を ADR 0017 として記録する。

## Why

agent-runtime-kernel feature は「Runtime 内部は event-driven」「GUI は Event Stream を購読し UI と runtime を密結合しない」ことを要件としている。v0.1 成功基準（mvp-roadmap）も「event stream が観測でき」「GUI で複数 pane が表示され」ことを含む。一方 architecture.md の Open questions に「Event Bus の transport 実装」が残っており、後続 slice（session-storage の event 永続化、provider-client の usage 計測、gui-panes の購読）は event-bus の上に実装される。この slice が最初に schema と transport を固定する必要がある。また ADR 0012 は TTFT / throughput を「event bus の観測で自動集計」すると定めており、観測層の土台（ring buffer + timestamp 付き usage event）をここで用意する。

## Scope

- `crates/event-bus/` に以下を実装:
  - `Event` enum（serde）: 
    - lifecycle: `AgentRunStarted` / `AgentRunCompleted` / `AgentRunFailed` / `Delegated` / `BackgroundTaskStarted` / `BackgroundTaskCompleted`
    - message: `MessageDelta` / `ReasoningDelta`
    - tool: `ToolStarted` / `ToolCompleted`
    - usage: `Usage`（token 数・cache stats、provider/model 識別子、TTFT / throughput 計測用の monotonic + wall-clock timestamp） / `CacheStats`
    - provider request-response: `ProviderRequestStarted` / `ProviderResponseCompleted`（TTFT の観測点）
    - fault: `ProviderFallback` / `BusLagDetected`（slow-consumer 検知）など
  - `EventBus`: tokio `broadcast` channel の wrapper。送信 / 購読（`subscribe()` で `Receiver<Event>`）、購読数をサポート。バッファ上限（capacity）と lag 時の drop ポリシーを定義。
  - slow-consumer 検知: receiver が `TryRecvError::Lagged(n)` を受けたときに bus 側で検知し、`tracing::warn` + `Event::BusLagDetected` を emit する。
  - `RingBuffer<T>`: ADR 0012 の bounded バッファ（raw 高頻度データを最古から drop で保持）。
  - 観測点（metrics observer の土台）: Usage event から 1 分バケットの in-memory 集計（per provider/model）を作る最小モジュールと、集計スナップショットを外部（storage の single-writer）へ渡すインターフェース。SQLite への永続化は v01-session-storage の担当。
- event type の将来拡張に備え、schema に version フィールド（または serde tag 運用）を持たせる。

## Out of scope

- 分散 / 外部 transport（gRPC / WebSocket / OTLP 等）の実装。in-process のみ。将来方針は ADR 0017 に記録する。
- SQLite への永続化・downsampled 書き込み（v01-session-storage の担当）。
- GUI の購読実装（v01-gui-panes の担当）。
- provider / runtime / tool 各レイヤーでの event 発行（各 slice の担当）。本 slice は schema と bus 本体。
- OTel exporter（optional feature は後続 slice で導入。ADR 0012 の外部委譲は任意 sink）。

## Verification

- unit test:
  - 単一 bus への emit を複数 subscriber が受信できる（N subscriber 全員が受信）。
  - 追従不能な subscriber が `Lagged(n)` を受け、drop 件数が報告され、fault event が emit される。
  - serde round-trip: 各 event variant を serialize → deserialize して等価性確認。
  - `RingBuffer` の bounded 動作（満杯時に最古 drop）。
  - usage event の timestamp が monotonic / wall-clock 両方揃っていることの検証。
- `cargo test -p event-bus` / `cargo clippy -p event-bus -- -D warnings` / `cargo fmt --check` が通ること。`git diff --check`。

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: `intents/evorch/features/agent-runtime-kernel/overview.md` を具体化する slice（event-driven runtime・GUI non-coupling）。新規 intent node は不要。
- ADR candidate: 必須採用。`intents/evorch/decisions/0017-event-bus-transport.md`「Event Bus の transport を in-process tokio broadcast に確定する」を生成し、architecture.md の transport Open question を解決済みにする。
- Diagram candidate: なし（emit → bus → subscriber（observer / GUI）の流れは architecture.md の層構造テキストで表現済み）。
- Docs update: `intents/evorch/technology/architecture.md` の Open questions「Event Bus の transport 実装」を更新。
- Closeout learning: transport 選定根拠（v0.1 は単一プロセス内の観測・GUI subscription が主用途で、分散化は broadcast の枠を越えた別設計として後段で再検討）と schema versioning 方針を記録。`write_back_required: true`。

- Guide reachability (G645): 本 slice は内部 crate のみで、ユーザー / オペレータ向け等の role-facing surface を追加しない。`no_role_facing_surface: true` を明示する。

`improve` (G456 / G460) is the later safety net; packet-time maintenance is the normal path.