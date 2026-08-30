# ADR 0022: 親子限定ツリー addressing と can_delegate の Role capability 開放

## Status

Accepted（2026-08-30、grill セッション `subagent-internalization` の Q1/Q2 で確定。実装は v0.2 ターゲット）

## Context

[herdr-opencode-loop](../../../.opencode/skill/herdr-opencode-loop/SKILL.md) の運用で、lead ↔ worker の別 pane 委譲、`[herdr-relay]` による中継、worktree 配置制約、sandbox 下 `.git` read-only に伴う bundle 運用といった跨ハーネス連携の痛みが蓄積した。これらを harness 内部の background delegation / messaging 機能として内製化するにあたり、subagent 間通信の宛先規則（topology）とネスト委譲の可否を実装に先立って確定する必要があった。

先行実装調査として oh-my-pi（can1357/oh-my-pi、commit 51f0380）を読解した結果:

- メッセージングは **flat peer DM**（registry 上の running/idle peer への任意 DM）で、宛先認可は「自 send 禁止」と「advisor 除外」のみ。
- 親子関係は lifecycle 管理と tree 表示（agent-tree）にのみ使われ、DM 認可には使われていない。
- 一方で irc-bridge は「送信者が親かどうか」で busy 時の注入挙動を steer（親）/ aside（非親）に切り替えており、親子関係が通信语义上特別であることの実装的証左がある。

現行 evorch は Orchestrator のみが `can_delegate = true`（`crates/agents/src/capability.rs`）で、meta-op `send_message` は送信後に相手 run の完了を待つ同期寄りの形を取り、宛先検証ルールは未定式だった。

## Decision

### 宛先規則: 親子限定ツリー addressing（Q1: comm-topology）

subagent 間通信の合法なメッセージ経路を **委譲ツリーの辺（delegator → delegatee）のみ** とする。任意の AgentRun は自分を委譲した run（親）と自分が委譲した run（子）のみにメッセージを送信可能。sibling 間の通信が必要な場合は orchestrator（ツリーの根）が中継する。

oh-my-pi 型 flat peer DM（registry 上の running/idle peer への任意 DM）は不採用。oh-my-pi では親子関係が DM 認可に使われていないが、本ドメインではそれより厳格な規律を取る。

### ネスト委譲: can_delegate の Role capability 開放（Q2: nested-delegation）

`can_delegate` を Role capability として Orchestrator 以外の Role（Worker 等）にも開放し、worker がさらに subagent を呼ぶネスト委譲を可能にする。付帯条件:

- 最大委譲深度の cap（推奨 2–3。実装時に確定）
- 自己 spawn 禁止（agent が自分へ委譲できない）
- [構想書 §5.1](../../../agent-harness-concept.md) の delegation 乱用防止枠組みの適用（委譲の正当性・value の説明可能性）

## Consequences

- 宛先検証が delegation tree の親子関係の有無で完結するため、認可・監査・再送の管理が単純になる。
- tool visibility に messaging 用の capability 次元を新設しない。可視化は従来どおり capability 行列（`RoleCapabilities`）による tool filtering で制御する。
- ADR 0002 との関係: boundary 自体の設計は変更せず、`can_delegate` の適用範囲（どの Role が持ち得るか）を拡大する。capability 強制は引き続き runtime が `RoleCapabilities` のみを消費する構造で行う。
- Q1 の親子ルールにより、ネスト委譲で N 階層になっても sibling へのアクセス経路は増えず、topology 上の安全性は depth に依らず保たれる。相互 await の deadlock cycle はツリー規則上構造的に不可能になる（wait timeout は二重の安全網）。
- herdr-opencode-loop で「worker からの再委譲は未検証」とされていた運用上の未解決が、evorch 内では構造として解決する。
- 影響先: `crates/agents/src/capability.rs`（`RoleCapabilities.can_delegate` の意味拡張。現行は Orchestrator のみ true）、`policy.rs` の tool filtering（delegate 系 meta-op を capability を持つ Role に可視化）、`crates/runtime/src/meta.rs` の宛先検証（sender → target が親子関係であること）。
- メッセージング op 構成・配送语义・workspace 绑定の詳細設計（grill Q3/Q4）は本 ADR ではなく feature overview の v0.2 計画に記す。[agent-runtime-kernel](../features/agent-runtime-kernel/overview.md)、[orchestration](../features/orchestration/overview.md) を参照。

## Related

- [ADR 0001: 固定 workflow を採用しない](0001-no-fixed-workflow.md) — orchestrator はツリーの根であり、動的 topology は委譲辺で構成される
- [ADR 0002: Role は capability boundary とし、prompt discipline ではなく権限で分離する](0002-role-capability-boundaries.md) — can_delegate 開放による適用範囲拡大。boundary 設計自体は不変
- [ADR 0018: SQLite event sourcing と storage schema](0018-sqlite-storage-schema.md) — メッセージの durable 化・監査・再送の基盤
- grill セッション確定記録: [interviews/subagent-internalization.json](../interviews/subagent-internalization.json)
- [herdr-opencode-loop SKILL.md](../../../.opencode/skill/herdr-opencode-loop/SKILL.md) — 内製化の動機となった運用ガイド

## References

oh-my-pi（can1357/oh-my-pi）の参照は commit 51f0380 の調査に基づく。参照ファイル:

- `registry/agent-lifecycle.ts`（idle → parked → revive のライフサイクル）
- `registry/agent-tree.ts`（親子 tree。lifecycle / 表示用途であり DM 認可には未使用）
- `irc/bus.ts`（mailbox + waiter + delivery receipt）
- `session/irc-bridge.ts`（送信者が親かどうかで分岐する steer / aside の注入 policy）
- `task/engine.ts`（task agent 実行。`spawns: "*"` によるネスト委譲）
- `config/agents-config.ts`（最大再帰深度・自己 spawn 禁止の検査）
- `messaging.ts`（DM メッセージ型）
- `projections/pipeline.ts`（イベント投影）
