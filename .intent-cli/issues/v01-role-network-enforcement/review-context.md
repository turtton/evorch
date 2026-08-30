# v01-role-network-enforcement Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

## Slice-specific review focus

- `RoleCapabilities.network` が production の sandbox/tool construction まで実際に伝播し、unit test 用だけの mapping や dead API で終わっていないか
- Denied role が確実に `--unshare-net` を受け、Allowed role のみが明示的に親 netns を継承するか。default 値や constructor の取り違えで fail-open にならないか
- OptIn が明示 opt-in 不在時に deny となる fail-closed rule を持ち、全 `NetworkAccess` variant の mapping test があるか
- Denied/Allowed integration test が同一の親 TCP endpoint を使い、単に argv を確認するだけでなく実接続の失敗/成功を証明するか
- Provider API 呼び出しを bwrap 内へ移動していないか。main process + per-call auth injection の既存境界に変更がないか
- bwrap unavailable が通常 pass と区別されるか。stderr を出して return するだけ、あるいは CI で常に green に見える仕組みは不十分
- per-destination proxy、DNS filter、`NetworkPolicy::providers_only` の実強制を v0.1 に持ち込んでいないか。scope widening の目印は新 proxy/backend/daemon や sandbox redesign
- tool capability matrix、approval semantics、filesystem policy を不必要に変更していないか

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

注: `intent-cli intent facet-check` は lexical な補助に留まる。上記 Slice-specific review focus が role capability と OS enforcement の意味的接続を確認する主たる観点である。

## Knowledge Writeback Expectation (G461)

`closeout_learning.write_back_required` は `true`。closeout で以下が host 側に記録されているか確認する（この PR 内または追跡可能な follow-up packet）。

- `features/tools-sandbox/overview.md`: issue #6 AC4 の provider endpoint allowlist 表現を、ADR 0021 の v0.1 実装（deny=`--unshare-net`、allow=親 netns full-open、selective egress は v0.2）へ整合
- `features/orchestration/overview.md`: role-dependent network mapping の確定挙動と `OptIn` の fail-closed rule
- bwrap integration test が実行されたか explicit skip されたかを判別する観測方法

記録が未実施の場合は、security boundary の仕様 drift が残るため知識 writeback 不足として review 所見に残す。
