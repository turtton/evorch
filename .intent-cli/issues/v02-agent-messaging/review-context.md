# v02-agent-messaging Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

## Slice-specific review focus

- AgentMessage が provider 会話 `Message` や `BackgroundTaskCompleted` payload に混在せず、message lifecycle と run lifecycle を独立に購読・永続化できる型 / EventKind になっているか
- `send` が fire-and-forget で、現行 `meta::send_message` のように送信後 `wait_for_run` へ入り Done/Error を待つ挙動を残していないか。reply が必要な経路だけ `wait_reply` を使うか
- `wait_reply` が message id / `reply_to` で相関し、無関係 message を取りこぼさず、timeout / recipient termination を型付き error として扱うか。単一共有 waiter が reply を奪い合う race がないか
- `inbox` が未読 pull 契約を守り、既読再配送・順序逆転・steering と aside の二重注入を起こさないか
- ADR 0022 の親子限定 addressing が共通配送入口で強制されるか。send / reply / steering の一部だけが sibling・自己宛・無関係 run を許す bypass を持たないか
- Running recipient で親 sender は steering、非親 sender は aside、Waiting / idle recipient は wake になるか。aside が model completion と tool result の不安全な途中へ割り込まず step boundary で注入されるか
- AgentMessage が Event Bus emit 後に既存 storage ingress（secret guard / hard limits / schema version）を通り transcript 永続化されるか。in-memory mpsc のみを durable mailbox と誤認していないか
- transcript から sender / recipient / reply correlation / ordering を復元できるか。`MessageRecord` schema 拡張または event projection の選択が migration / round-trip test で裏付けられているか
- crash recovery が「親が別 RunId の新規 run を起動し transcript から文脈再構成」に留まるか。parked snapshot、in-flight tool/provider 状態、同一 run identity の厳密 revive を v0.2 に持ち込んでいないか
- AgentMessage 到着のみで `BackgroundTaskCompleted` / `Cancelled` を emit せず、既存 delegate / wait / cancel / GUI event consumer の契約を壊していないか

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

注: `intent-cli intent facet-check` は lexical な補助に留まる。上記 Slice-specific review focus が channel 分離、reply correlation、親子 addressing、durable transcript、v0.3 revive 境界の意味的接続を確認する主たる観点である。

## Knowledge Writeback Expectation (G461)

`closeout_learning.write_back_required` は `true`。closeout で以下が host 側に記録されているか確認する（この PR 内または追跡可能な follow-up packet）。

- `features/agent-runtime-kernel/overview.md`: AgentMessage envelope / EventKind、send / wait_reply / inbox、steering / aside / wake の実装確定、transcript persistence、crash 時の新規 run + context 再構成、strict revive v0.3 境界
- `features/orchestration/overview.md`: mid-run relay が lifecycle completion と独立した durable AgentMessage channel を使うこと、および Orchestrator の route surface

記録が未実施の場合は、v0.2 messaging 計画と実装の drift が残るため知識 writeback 不足として review 所見に残す。
