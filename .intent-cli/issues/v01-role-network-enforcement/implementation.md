# v01-role-network-enforcement Implementation Packet

## Goal

`crates/agents/src/capability.rs:10-18` の `NetworkAccess` と `RoleCapabilities.network`（同:27-33）を、`crates/runtime/src/policy.rs` から sandboxed tool execution の構築・dispatch 経路へ渡し、role ごとの network capability を bwrap policy として OS レベルで強制する。v0.1 の実装は ADR 0021 に従う二値制御で、deny は `BwrapConfig` の既定値と `--unshare-net`（`crates/sandbox/src/bwrap.rs:23-35,95-97`）、allow は親 network namespace の継承とする。併せて、`crates/sandbox/tests/bwrap.rs:14-21,56-59,85-89` の bwrap 不在時の silent early-return を、pass と区別できる明示的な skip 観測へ改める。

## Why

v0.1 inspect で、issue #7 / `v01-agent-roles` が実装した `NetworkAccess` は宣言されている一方、`ExecutionPolicy` は tool の authorize/filter のみを実施し（`crates/runtime/src/policy.rs:28-77`）、sandbox network policy を生成・伝播していないことが判明した。これは ADR 0002 の「各 Role に対応する sandbox policy との整合が必要」（`intents/evorch/decisions/0002-role-capability-boundaries.md:23-27`）と、issue #7 AC1 の runtime-level network enforcement を満たさない CRITICAL gap である。issue #6 / `v01-sandbox-approval` で bwrap の default-deny 自体は実装済みだが、role-level dispatch と未接続のため、この修正 round で security boundary を閉じる。

## Scope

- `NetworkAccess::{Denied, OptIn, Allowed}` から sandbox network mode への pure な mapping を定義する。`OptIn` は明示 opt-in 情報がない限り fail-closed（deny）とし、variant 全件を unit test する
- role / run ごとの `ExecutionPolicy` から、shell / git_diff に注入する `Sandbox` または `BwrapConfig` を構築する production dispatch path へ network mode を伝播する
- Denied role は `--unshare-net` を保持し、Allowed role は `BwrapConfig::allow_network(true)` 相当で親 netns を継承する。bwrap は destination ごとの filter を持たないため、allow は full-open であることを型名・コメント・テスト名で誤魔化さない
- 親 namespace のローカル TCP listener を fixture とし、Denied role の sandbox 内接続失敗と Allowed role の接続成功を同じ条件で検証する
- provider client（OpenAI / Anthropic / OpenAI-compatible）は main process から呼ばれ、`ProviderAuth` を per-call 注入する既存経路のままにする。bwrap は tool execution のみを包み、provider endpoint を bwrap 内へ移さない
- `crates/sandbox/tests/bwrap.rs` の `eprintln!("skip: ...")` + `return` を、テストランナー出力で pass と区別できる skip reporting に変更し、CI/ローカル双方で bwrap integration の実行・skip 状態を判別可能にする

## Out of scope

- per-destination / hostname allowlist の OS enforcement、proxy 型 selective egress、DNS 制御、sandbox architecture の再設計 — v0.2
- ADR 0021 の決定変更。v0.1 は deny=`--unshare-net`、allow=親 netns 継承という full-deny/full-open を維持する
- provider client を bwrap 内で実行する変更、provider authentication / routing / retry semantics の変更
- filesystem capability、approval policy、role の tool allowlist、macOS / Windows sandbox の変更

## Verification

- runtime/sandbox unit tests: `NetworkAccess` 全 variant と explicit opt-in 有無が期待する bwrap network mode へ mapping される
- integration test: 同一の親 TCP listener に対し、Denied role の sandboxed tool は接続失敗、Allowed role は接続成功する
- existing capability tests: `ExecutionPolicy::authorize` / `filter_tool_specs` と role tool boundary が回帰しない
- provider regression tests: provider client path と per-call auth injection に変更がなく、既存 provider tests が通る
- bwrap 利用環境では deny/allow integration が実行されたこと、非利用環境では skip が pass と別に明示されることをテスト出力で確認する
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: `features/tools-sandbox/overview.md` を primary とし、role dispatch の確定内容を `features/orchestration/overview.md` にも反映する。新規 intent は不要
- ADR candidate: decline — bwrap の all-or-nothing network model と v0.1 scope は ADR 0021 で確定済み
- Diagram candidate: decline — role → execution policy → sandbox config の経路は feature overview の記述で十分
- Docs update: decline — internal policy wiring で role-facing surface を追加しない
- Closeout learning: issue #6 AC4 の allowlist 文言を ADR 0021 の full-open/full-deny 実態へ host 側で整合し、selective egress は v0.2 と記録する。bwrap integration skip の観測方法も記録する。`write_back_required: true`

- Guide reachability (G645): 内部 runtime / sandbox wiring のみで CLI / GUI / 対話 surface は追加しないため、`no_role_facing_surface: true` を宣言する

`improve` (G456 / G460) は later safety net。packet-time で writeback を宣言済み。
