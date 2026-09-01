# v02-context-compaction Implementation Packet

## Goal

AgentRun の model-visible context window を非破壊に圧縮する provider-neutral compaction engine を実装する。model context limit の 75% を既定の自動閾値とし、手動 command と agent-facing compact tool（DCP 型）でも同じ engine を発動できる。圧縮は古い visible messages を summary checkpoint へ置換して provider request の窓を狭める操作であり、SQLite に永続化済みの raw messages/events は削除・上書きしない。発動理由、範囲、token before/after、summary identity を event で観測可能にし、system prompt には cache hit rate を壊す乱用を避ける policy を注入する。

## Why

v0.2 の goal→実装→PR→review loop は長時間・多 agent の transcript を持つため、context 上限に達すると完了 gate 前に停止する。現行 `crates/runtime/src/meta.rs` の `compact` は `COMPACT_STUB` を返すだけで、AgentContext の window 管理は未実装。一方、storage は `messages_by_session` / `events_by_session` と event sourcing により raw history を監査可能に保持できる。grill Q5 は 75% 自動 + 手動 + DCP 型 agent tool、cache-aware system prompt、raw history 非破壊を確定した。本 slice は v02-agent-messaging の durable AgentMessage 文脈を summary に含め、長い自律 loop を止めずに audit を保つ runtime context lifecycle を完成させる。

## Scope

- provider/model catalog から context limit を取得し、現在の model-visible messages、system prompt、tool schema、pending injection、response headroom を含む token usage estimate を算出する。未知 limit/estimator failure は自動 compaction を fail-closed に無効化し diagnostics を返す
- 既定 auto threshold は context limit の 75%。turn/tool boundary で判定し、未満では発動しない。config override を許す場合も strict range validation と safe default を持つ
- `crates/runtime/src/meta.rs` の `compact` stub を実 engine に接続し、manual trigger とする。agent-facing compact tool も統一 `Tool` / `ToolExecutor` 経由で同じ engine を呼ぶ。automatic/manual/agent の reason を型で区別する
- compaction 対象は complete turn/tool exchange 単位。system/core policy、active goal/contract、未完了 task、重要決定、変更 file、test結果、未解決事項、直近 relevant messages、v02-agent-messaging の send/reply/steering 文脈を summary schema に保持する
- open tool call と対応 result、未完了 tool execution、最新 user request を compact range の境界で分断しない。対象にできない場合は安全な前方 boundary へ縮める
- summary generation は既存 provider abstraction を使う provider-neutral operation とし、summary text と structured checkpoint metadata を生成する。OpenAI official compaction endpoint は使用しない
- successful compaction 後、AgentContext の provider-facing view を system/core prefix + summary checkpoint + recent tail に切替える。compact 済み raw messages を次 request へ再送しない
- SQLite message/event rows は削除・更新しない。compaction event/checkpoint を追加追記し、`Database::messages_by_session` / `events_by_session` / restore から raw transcript と visible-window 切替点の双方を追跡可能にする
- event detail は session/run identity、trigger reason、threshold、estimated tokens before/after、compacted range identity、summary/checkpoint identity、success/failure diagnostics を持つ。event schema_version と既存 consumer の後方互換を保つ
- summary generation / persistence / context swap は failure atomic にする。生成・event append・context swap のどこかが失敗したら古い AgentContext を維持し、raw storage に破壊的変更を残さない
- in-flight lock と hysteresis/cooldown を設け、auto/manual/agent の競合・連打で同じ range を重複圧縮しない。圧縮後も閾値超過なら一回の turn 内で無限再発動せず diagnostics と次 action を返す
- v02-prompt-assembly の model-specific system prompt section に cache-aware policy を注入する: stable prefix を不必要に壊さない、agent tool は意味のある task boundary で使う、閾値前の乱用を避ける、必要な durable fact を summary に残す

## Out of scope

- OpenAI official compaction API / Responses API の provider-specific compaction — roadmap どおり v0.3
- raw SQLite transcript/event の削除、上書き、retention cleanup、vacuum policy
- parked run の完全 snapshot revive。v0.2 crash recovery は親が新 run を起動し transcript から再構成（grill Q2）
- semantic memory / vector database / cross-session long-term memory
- lossy deletion だけを行う pruning、tool output を storage から消すこと
- summary model の新 routing subsystem。既存 provider/routing と v02-prompt-assembly を消費する
- GUI 専用 compaction editor。manual surface は runtime command/tool を headless に検証可能とする

## Verification

- threshold boundary unit test: 74.99% 非発動、75% 到達で一度だけ発動、override range validation、unknown limit fail-closed
- trigger parity test: automatic/manual/meta compact/agent tool が同じ engine を呼び reason のみ異なる
- summary fidelity fixture: goal/contract、unfinished task、decision、files/tests、unresolved、recent tail、AgentMessage を保持
- boundary test: open tool call/result pair、latest user request、in-flight tool を分断しない
- provider request snapshot: compaction 後は core prefix + summary + recent tail、raw compacted messages 非再送
- storage audit integration: messages/events row count と raw content 不変、compaction event/checkpoint 追記、ordered read/restore 可能
- failure atomicity: provider failure / event append failure / context swap failure で旧 visible context 維持、partial checkpoint 不可
- concurrency/hysteresis: auto + manual + agent 競合で単一 summary、連続 auto loop なし
- cache-aware prompt snapshot: model/context 特性に応じた policy 注入、stable prefix の順序不変
- long-session integration fixture: opencode / senpi / oh-my-pi / omo compress の共通 behavior を参照し、compaction 後も次 turn と delegated message relay が継続
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: `features/agent-runtime-kernel/overview.md` primary。Context lifecycle の kernel 責務。`features/orchestration/overview.md` は summary に goal/contract/unfinished work を保持する consumer
- ADR candidate: decline — visible window narrowing + raw history preservation は grill Q5 と ADR 0018 の組合せで確定済み。provider-specific API を採用する新決定ではない
- Diagram candidate: decline — raw transcript / summary checkpoint / visible window の三層は overview 記述で十分。実装で複雑化した場合のみ follow-up
- Docs update: decline — GUI editor は追加しない。manual/tool surface は Guide Reachability で扱う
- Closeout learning: estimator、threshold/hysteresis、summary schema、event schema、failure atomicity、raw audit preservation、cache policy の実装確定を write back。`write_back_required: true`

- Guide reachability (G645): 全 runtime role が automatic compaction の対象で、manual compact と agent-facing compact tool を利用する。agent-runtime-kernel overview から route する。`no_role_facing_surface: false`

`improve` (G456 / G460) is the later safety net; packet-time maintenance is the normal path.
