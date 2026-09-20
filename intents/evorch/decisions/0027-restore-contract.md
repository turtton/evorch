# ADR 0027: 復元契約と実行状態の分離（restore / isolation 語彙の責務分離）（2026-09-21）

## Status

Accepted（2026-09-21）

## Context

run-29 で観測された不具合: GUI の chat run は常に ownership permit を持つため
`write_terminal_snapshot` が終端スナップショットに
`restorable = false` / `non_restorable_reason = "復元対象外の実行状態: ownership"` を
記録していた。後続メッセージの継続入口である `continue_goal` はこの記録をそのまま
`UnsupportedConfig` として拒否し、`chat failed: 実行 run-29 の復元に失敗しました`
で失敗した。

根本原因は語彙の混同にある。ownership はスナップショットから復元すべき「隔離設定」
（isolation）ではなく、continue 時に GUI が現在の permit を再発行する「実行権限」
（renewable authority）である。`delegate_chat` はこの区別を入口内にインラインで
持っており ownership-only 記録を受け入れていたが、`continue_goal` には同じ carve-out が
なかった。入口ごとに語彙の解釈がずれ、片方だけが壊れた。

## Decision

復元可否を「実行状態の列挙」ではなく「設定フィールドごとの復元 class 分類」として
語彙化し、3 surface で解釈を統一する。

1. **マーカーの一元化。** ownership のみを理由とする復元不可記録は
   `restore.rs` の `OWNERSHIP_ONLY_UNRESTORABLE_REASON` 定数によってのみ識別し、
   判定は `RunRestoreDescriptor::renewable_ownership_only()` に集約する。
   `write_terminal_snapshot` の書き込み側と `continue_goal` / `delegate_chat` の
   両ゲートは必ずこの定数・helper を経由し、文字列の inline 複写を禁止する。
2. **3 class 分類。** `write_terminal_snapshot` が復元不可として記録する
   設定フィールドは、意味によって次のいずれかに必ず分類する。
   - **renewable（呼び出し側が新規権限を再発行）**: `ownership`。スナップショットからは
     一切復元せず、continue / delegate 時の現在権限で上書きする。ownership
     のみを理由とする記録は復入口では history 復元を許可してよい。
   - **blocking（構造的に継続不能）**: `topology`（DynamicTeam）・`team`・`team_task`・
     `team_store`・`finding_store`・`delegation_value`・`memory`・`learning_internal`・
     `workspace_branch`。協調・委譲・workspace・session 配線を含む実行構造は
     スナップショットから意味のある継続が定義できず、いかなる入口でも拒否する。
   - **security/privacy（自動 carve-out 禁止）**: `skip_secret_redaction` のような
     配慮系トグル、および **ownership との複合を含むあらゆる hard blocker 組み合わせ**。
     複合理由 `"復元対象外の実行状態: team_task, ownership"` は renewable 判定の
     完全一致に引っかからず拒否理由としてそのまま返る
     （テスト `goal_restore_rejects_ownership_combined_with_other_blockers` で固定）。
     ownership が同居しても安全側に緩めない。
3. **surface ごとの carve-out 適否。**

   | 復入口 | 権限の供給源 | renewable（ownership のみ）carve-out | blocking / security |
   |---|---|---|---|
   | `continue_goal` | 呼び出し側の `RunConfig`（GUI が現在 permit を再発行） | 許可 | 拒否 |
   | `delegate_chat` | 同上 | 許可 | 拒否 |
   | `restore_and_deliver` | ディスク上の descriptor のみ（disk-descriptor-authoritative） | **不可** | 拒否 |

   `restore_and_deliver` は呼び出し側権限を持たず config 全体をディスク記録から導出する
   責務のため、`restorable == false` なら理由を問わず `UnsupportedConfig` で fail closed
   する。carve-out を持たない。
4. **将来ルール。** `RunConfig` に新規フィールドが追加され
   `write_terminal_snapshot` がそれを復元不可として記録し始める場合、merge 前に
   本 ADR の 3 class のいずれかへ分類を明記し、該当 surface での挙動をテストで
   固定すること。class 未決のまま復元不可記録を増やしてはならない。

なお `snapshot_consumed` は消費済みスナップショットの二重复元防止を示す内部
bookkeeping であり、3 class の分類対象ではない。

## Evidence（実装検証）

- 修正は `crates/runtime/src/restore.rs`（`OWNERSHIP_ONLY_UNRESTORABLE_REASON` +
  `renewable_ownership_only()`）と `crates/runtime/src/runtime/chat_restore.rs`
  （`continue_goal` ゲート）に限定され、Rust 側の code 修正テストは green。
- acceptance テスト: `goal_restore_accepts_renewable_ownership_only_snapshot`
  （ownership-only 記録の継続成功・呼び出し側 permit 供給・snapshot 同期消費を固定）、
  `goal_restore_rejects_ownership_combined_with_other_blockers`
  （複合理由の拒否を文字列一致で固定）。
- 復入口の拒否理由は記録された `non_restorable_reason` をそのまま利用者に返すため、
  診断可能性を保ったまま語彙の解釈差異のみを解消している。

## Consequences

- 「実行状態の列挙」（run phase / durable task 状態機械）と「設定の復元可否」
  （`restorable` + `non_restorable_reason`）は別軸として記憶・運用される。
  詳細は `features/orchestration/durable-execution-states.md` の
  「復元可否 (restorable) は実行状態列挙とは別軸」を参照。
- GUI chat の ownership 持ち run は終端後も継続可能で、run-29 型の拒否は再現しない。
- blocking / security class の複合は常に fail closed で、renewable の緩和が
  他フィールドへ外挿される経路は存在しない。
- 新規 `RunConfig` フィールド追加時は ADR 本書の class 表更新が review 必須項目になる。
