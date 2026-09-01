# v02-context-compaction Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

## Slice-specific review focus

- `crates/runtime/src/meta.rs` の compact stub が実 engine に接続され、automatic/manual/agent tool が別実装へ分岐せず同じ semantics を使うか
- threshold が model context limit に対する model-visible estimate の 75% か。raw SQLite bytes/message count を代用していないか。74.99/75 boundary、unknown limit、response headroom がテストされているか
- compaction が complete turn/tool exchange を境界にし、open tool call/result、in-flight tool、latest user request を分断しないか
- summary が active goal/contract、unfinished tasks、重要 decision、files/tests、unresolved issues、recent context、v02-agent-messaging の AgentMessage を保持するか。単なる末尾切捨てになっていないか
- successful compaction 後の provider request が core prefix + summary checkpoint + recent tail に狭まり、compact 済み raw messages を再送していないか
- SQLite messages/events の raw rows を削除・更新していないか。`messages_by_session` / `events_by_session` / restore で完全 transcript を監査できるか（ADR 0018）
- compaction event が reason、threshold、before/after estimate、range、summary identity を持ち、既存 event consumer と後方互換か
- provider summary failure、storage append failure、context swap failure が atomic に処理され、旧 visible context を失わず partial state を残さないか
- in-flight lock / hysteresis / cooldown が auto/manual/agent 競合と連続発動を抑え、圧縮後も 75% 超過時に無限 loop しないか
- cache-aware policy が system prompt assembly に入り、stable prefix を毎 turn 変更したり閾値前の tool 乱用を促していないか
- OpenAI official compaction API、raw retention cleanup、parked full revive、vector memory、GUI editor へ scope を広げていないか

## Facet context

<!-- BEGIN GENERATED FACET CONTEXT (G530) -->
### vocabulary
- (none overlapping this packet's intent_references)
### invariant
- (none overlapping this packet's intent_references)
### decider
- (none overlapping this packet's intent_references)
### acceptance-property
- (none overlapping this packet's intent_references)
<!-- END GENERATED FACET CONTEXT (G530) -->

注: lexical facet-check は visible-window narrowing と raw-history preservation の分離、summary fidelity、failure atomicity を検証できない。provider request snapshot と SQLite audit test を主根拠とする。

## Knowledge Writeback Expectation (G461)

`closeout_learning.write_back_required` は `true`。closeout で以下が記録されているか確認する。

- `features/agent-runtime-kernel/overview.md`: estimator、75% boundary、manual/agent trigger、summary/recent-tail、hysteresis、event、failure atomicity、raw history 非破壊
- `features/orchestration/overview.md`: goal/contract/unfinished work/AgentMessage を compaction summary に保持して loop continuation を保証する契約

未記録なら、長時間 loop の continuation と audit invariant が実装から再構成不能になるため knowledge writeback 不足として所見に残す。
