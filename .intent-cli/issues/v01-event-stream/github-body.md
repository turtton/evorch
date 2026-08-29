## Goal

Agent Kernel のイベントバックボーン `crates/event-bus/` を実装する。型付き event stream（tokio broadcast、in-process）と serde の event schema を提供し、ADR 0012 の計測収集層の土台（bounded ring buffer + in-memory 集計の観測点）を整備する。

## Why This Slice Exists Now

v0.1 成功基準は「event stream が観測でき」ことを含む。agent-runtime-kernel は event-driven runtime と GUI 非密結合を要件とし、後続の session-storage（event 永続化）・provider-client（usage 計測）・gui-panes（購読）はこのバックボーンに依存する。architecture.md の Open questions「Event Bus の transport 実装」も解決する必要があるため、最初に schema と transport を固定する。

## Current Observed State

- `crates/event-bus/` は骨格のみ（`Cargo.toml` + `src/lib.rs`、実装なし）。
- event schema・transport・lag ポリシーに関する決定は未存在。
- architecture.md に「Event Bus の transport 実装」Open question が未解決で残る。

## Accepted Baseline You May Assume

- v01-scaffold 完了済み。workspace はビルド可能。
- Async runtime は Tokio。broadcast channel は tokio 提供のものを使用できる。
- ADR 0012 で「OTel Metrics API」「bounded ring buffer で raw 保持」「downsampled のみ永続化」「TTFT / throughput は event bus の観測で自動集計」が決定済み。
- agent-runtime-kernel/overview.md が列挙する runtime event（Started / MessageDelta / ReasoningDelta / ToolStarted / ToolCompleted / Delegated / BackgroundTaskStarted / BackgroundTaskCompleted / Usage / CacheStats / ProviderFallback / Completed / Failed）を event の出発点にする。

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/event-bus/`

Target part: Agent Kernel のイベントバックボーン

## In Scope

- `Event` enum（serde）: lifecycle / message / tool / usage / provider request-response / fault の各カテゴリ。
- `EventBus`（tokio broadcast ベース）: emit / subscribe（複数 subscriber）。
- subscriber lag 時の挙動: drop ポリシーの定義と slow-consumer 検知（`Lagged(n)` → `tracing::warn` + fault event）。
- token usage / TTFT 計測用の timestamp（monotonic + wall-clock）フィールド。
- `RingBuffer<T>`（ADR 0012 の bounded バッファ、最古 drop）。
- usage event から 1 分バケットの in-memory 集計（per provider/model）を作る最小観測モジュールと、storage の single-writer へ渡すインターフェースの土台。
- schema version フィールド（将来の event 拡張用）。
- ADR 0017（event transport）生成。

## Out Of Scope

- 分散 / 外部 transport の実装（in-process のみ。方針は ADR 0017 に記録）。
- SQLite への永続化・downsampled 書き込み（v01-session-storage）。
- GUI の購読実装（v01-gui-panes）。
- provider / runtime / tool 各レイヤーでの event 発行（各 slice の担当）。
- OTel exporter（optional feature は後続 slice）。

## Standalone Child Issue Contract

`turtton/evorch` の `crates/event-bus/` に型付き Event Bus を実装する。serde で直列化可能な `Event` enum（agent lifecycle / message / tool call / token usage / provider request-response / fault の各カテゴリ、timestamp は monotonic + wall-clock を両方持つ）を定義し、tokio broadcast ベースの in-process `EventBus`（複数 subscriber への配信、capacity 上限、lag 時の drop + slow-consumer 検知として `tracing::warn` と fault event の emit）を実装する。ADR 0012 に従い、bounded `RingBuffer<T>`（最古 drop）と、usage event から 1 分バケット per provider/model を in-memory 集計して外部へ渡す観測モジュールの土台を含める（SQLite 永続化は v01-session-storage）。全 event 型の serde round-trip テストと、複数 subscriber 受信・lag 検知のテストを入れる。さらに transport 決定を `intents/evorch/decisions/0017-event-bus-transport.md` として書き（in-process 固定 + schema versioning による将来対応）、`intents/evorch/technology/architecture.md` の Open questions「Event Bus の transport 実装」を解決済みに更新する。

## Acceptance Criteria

- emit した具象 event を複数 subscriber が受信できる。
- subscriber が追従できない場合の挙動（drop / slow-consumer 検知）が定義され、実装される（`Lagged(n)` 検知 → warning + fault event）。
- 全 event 型の serde round-trip テストが通る。
- event schema に token usage / TTFT 計測に必要な timestamp（monotonic + wall-clock）フィールドがある。
- ADR 0012 の bounded ring buffer（raw 保持、最古 drop）が `crates/event-bus/` に実装される。
- transport の in-process 化が ADR 0017 として host に記録される。

## Verification

- `cargo test -p event-bus`: 複数 subscriber 受信 / lag 検知 / serde round-trip / ring buffer bounded 動作 / timestamp 完全性。
- `cargo clippy -p event-bus -- -D warnings` / `cargo fmt --check` / `git diff --check`。

## Related Links

- [agent-runtime-kernel/overview.md](../../../intents/evorch/features/agent-runtime-kernel/overview.md) — event-driven runtime・event 一覧
- [architecture.md](../../../intents/evorch/technology/architecture.md) — Event Bus の位置づけ・transport Open question
- [0012-metrics-architecture.md](../../../intents/evorch/decisions/0012-metrics-architecture.md) — ring buffer / TTFT / downsampled 方針

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs
writeback is expected for this slice. Answer or explicitly decline:

- Intent placement: 既存の agent-runtime-kernel feature を具体化。新規 intent node 不要。
- ADR candidate: `intents/evorch/decisions/0017-event-bus-transport.md`「Event Bus の transport を in-process tokio broadcast に確定する」（必須）
- Diagram candidate: なし
- Docs update: `intents/evorch/technology/architecture.md` の transport Open question 更新
- Closeout writeback expected: yes（ADR 0017 生成 + architecture.md 更新）

## Guide Reachability (G645)

本 slice は内部 crate のみを追加し、guide 等の role-facing surface は追加しない（`no_role_facing_surface: true`）。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.