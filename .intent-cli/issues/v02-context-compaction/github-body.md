## Goal

AgentRun の model-visible context を provider-neutral に圧縮する。model context limit の 75% で自動発動し、手動 command と agent-facing compact tool（DCP 型）でも同じ engine を使う。古い visible messages を summary checkpoint + recent tail に置換して provider へ見せる窓だけを狭め、SQLite の raw messages/events は非破壊で保持する。発動理由・範囲・token before/after・summary identity を event で監査可能にする。

## Why This Slice Exists Now

v0.2 の goal→実装→PR→review loop は長時間 transcript を持つため、context 上限で finish gate 前に停止しない lifecycle が必要。現行 `compact` は `COMPACT_STUB` で、storage には raw transcript/event を順序付きで保持する基盤があるが model-facing window の切替がない。grill `grill-v02-loop-foundation` Q5 は 75% 自動 + 手動 + DCP 型、cache-aware system prompt、raw history 非破壊を確定した。依存する v02-agent-messaging の send/reply/steering 文脈も summary に残し、自律 loop continuation と audit を両立する。

## Current Observed State

- `crates/runtime/src/meta.rs` の `compact` dispatch は `COMPACT_STUB = "context-engine (v0.2) で提供予定"` を返すだけ
- `crates/runtime/src/agent_loop.rs` の `LoopState` は run 専有の `AgentContext` を持つが、summary checkpoint / visible window lifecycle はない
- `crates/runtime/src/run.rs` の `AgentInspection` は `message_count` を公開するが token ratio / compaction state はない
- `crates/storage/src/writer.rs` の `StorageHandle::append_event` は session_id 付き Event を SQLite single-writer へ追記する
- `Database::messages_by_session` / `events_by_session` は transcript/event を順序付きで読み戻せる
- `SessionSnapshot` は message/reasoning 差分と open_tool_calls を event log から復元する。raw history を保持する event-sourced 基盤は既存
- 統一 Tool trait / ToolExecutor は agent-facing compact tool の schema validation・event・result normalization に利用可能

## Accepted Baseline You May Assume

- grill Q5: default auto threshold 約 75%、manual trigger、agent tool としての compaction（DCP 型）
- cache hit rate 配慮は system prompt injection で threshold/behavior を調整し、乱用を防ぐ
- compaction は model に見せる window を狭める操作。raw history は storage に残し audit を維持
- reference behavior: opencode / senpi / oh-my-pi の auto/manual compaction、omo `compress` の範囲指定 summary
- ADR 0018: SQLite event sourcing を durable source of truth とする
- v02-agent-messaging: AgentMessage（send/reply/steering）は Event Bus/transcript に永続化される。strict parked revive は v0.3

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/runtime/`, `crates/storage/`, `crates/event-bus/`, `crates/tools/`, `crates/config/`

Target part: 75% automatic/manual/agent-trigger compaction engine、summary checkpoint、cache-aware policy、event observability、SQLite raw audit preservation

## In Scope

- model context limit + visible messages/system/tool schema/pending injection/response headroom の token estimate
- 75% default auto threshold、turn/tool boundary 判定、strict config validation
- meta `compact` manual trigger と統一 ToolExecutor 経由 compact tool。automatic/manual/agent reason
- complete turn/tool exchange 単位の compact range と open tool pair/in-flight/latest user request 保護
- goal/contract、unfinished task、decision、files/tests、unresolved、recent context、AgentMessage を保持する summary schema
- provider-neutral summary generation、core prefix + summary + recent tail への AgentContext swap
- raw message/event rows 非破壊、compaction event/checkpoint 追記、ordered audit/restore
- event metadata（session/run/reason/threshold/before-after/range/summary id）と後方互換
- failure atomicity、in-flight lock、hysteresis/cooldown、無限再発動防止
- v02-prompt-assembly の model-specific section への cache-aware compaction policy 注入

## Out Of Scope

- OpenAI official compaction API / Responses API provider-specific compaction — v0.3
- raw SQLite transcript/event の削除・上書き・retention cleanup
- parked run の完全 snapshot revive — v0.3
- semantic/vector memory、cross-session long-term memory
- tool output の storage からの lossy deletion
- summary model 用の新 routing subsystem
- GUI 専用 compaction editor

## Standalone Child Issue Contract

`turtton/evorch` に provider-neutral context compaction engine を実装する。model context limit に対する model-visible token estimate が既定 75% に達した turn boundary で一度だけ自動発動し、`runtime/meta.rs` の manual `compact` と統一 ToolExecutor 経由の agent compact tool も同じ engine を使う。complete turn/tool exchange を単位に、goal/contract、unfinished tasks、decisions、files/tests、unresolved issues、recent messages、AgentMessage を summary checkpoint に保持し、open tool pair/in-flight/latest user request を分断しない。成功後の provider request は core prefix + summary + recent tail のみとし、SQLite raw messages/events は削除・更新せず compaction event/checkpoint を追記する。reason、threshold、before/after estimate、range、summary identity を event で観測可能にし、failure atomicity、in-flight lock、hysteresis/cooldown、cache-aware system prompt policy を実装する。OpenAI official compaction API、raw retention cleanup、full revive、vector memory は実装しない。PR は `main` を target とする。

## Acceptance Criteria

- 74.99% では非発動、75% 到達で turn boundary に一度だけ auto compaction する boundary test がある
- manual command と agent compact tool が同一 engine を使い reason を区別できる
- summary fixture が goal/contract、unfinished task、decision、files/tests、unresolved、recent context、AgentMessage を保持し tool pair を分断しない
- compaction 後 provider request は summary + recent tail に狭まり compact 済み raw messages を再送しない
- SQLite messages/events の row/content は不変で、compaction event/checkpoint を含め ordered audit/restore できる
- summary/persistence/context swap failure で旧 context を保つ atomicity test がある
- auto/manual/agent 競合と連続発動を lock/hysteresis で抑止し無限 loop しない
- cache-aware policy が system prompt に注入され stable prefix の不必要な変更を避ける
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check` が pass する

## Verification

- threshold/unknown-limit/config validation unit test
- trigger parity + reason event test
- summary fidelity / tool-boundary fixture suite
- provider request snapshot before/after
- SQLite raw row/content preservation + compaction checkpoint restore integration test
- provider/storage/context-swap failure atomicity test
- concurrent trigger / hysteresis / no-loop test
- cache-aware prompt snapshot
- long-session continuation + AgentMessage relay integration fixture
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Related Links

- intents/evorch/features/agent-runtime-kernel/overview.md
- intents/evorch/features/orchestration/overview.md
- intents/evorch/technology/architecture.md
- intents/evorch/decisions/0005-headless-kernel-and-gui-separation.md
- intents/evorch/decisions/0018-sqlite-event-sourcing.md
- intents/evorch/interviews/grill-v02-loop-foundation.json（Q2 / Q5）
- dependency: `v02-agent-messaging`

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs writeback is expected for this slice. Answer or explicitly decline:

- Intent placement: `features/agent-runtime-kernel/overview.md` primary、`features/orchestration/overview.md` supporting
- ADR candidate: none（grill Q5 + ADR 0018 で確定済み）
- Diagram candidate: none
- Docs update: none
- Closeout writeback expected: yes。estimator/threshold/summary/event/atomicity/cache policy/raw audit preservation を記録する

## Guide Reachability (G645)

この slice は全 runtime role に automatic compaction を適用し、manual compact command と agent-facing compact tool を追加する。route は agent-runtime-kernel overview の context lifecycle surface → runtime meta/tool catalog。`no_role_facing_surface: false`。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly. Worker branch convention is `evorch/task/<run-id>`.
