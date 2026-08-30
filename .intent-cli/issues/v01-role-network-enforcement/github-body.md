## Goal

`RoleCapabilities.network` を sandboxed tool execution の bwrap network policy に接続し、role ごとの network boundary を OS レベルで強制する。Denied は `--unshare-net`、Allowed は親 network namespace 継承、OptIn は明示 opt-in がなければ deny とする。併せて bwrap 不在時の integration test skip を pass と区別して観測可能にする。

## Why This Slice Exists Now

v0.1 inspect で、issue #7 / `v01-agent-roles` の `NetworkAccess` 宣言が runtime の tool allowlist 判定に留まり、issue #6 / `v01-sandbox-approval` の bwrap network enforcement へ届いていない CRITICAL gap が見つかった。ADR 0002 は role を prompt discipline ではなく capability boundary とし、`intents/evorch/decisions/0002-role-capability-boundaries.md:17-20,27` で role-dependent network と sandbox policy の整合を要求する。v0.1.1 でこの未接続を閉じなければ、network denied role でも sandbox 構築次第で親 netns を継承できる。

## Current Observed State

- `crates/agents/src/capability.rs:10-18` に `NetworkAccess::{Denied, OptIn, Allowed}`、同 `:27-33` に `RoleCapabilities.network` がある
- `crates/runtime/src/policy.rs:28-77` の `ExecutionPolicy` は tool authorize と tool spec filtering のみで、network policy を生成・伝播しない
- `crates/sandbox/src/bwrap.rs:23-35,95-97` は `allow_network` の bool で `--unshare-net` を付け外せるが、role との wiring がない
- `crates/sandbox/src/network.rs:1-4,14-32` の `NetworkPolicy` は allowlist の表現に留まり、ADR 0021 が明記する通り bwrap 自体は all-or-nothing である
- `crates/sandbox/tests/bwrap.rs:14-21` は bwrap 不在時に stderr 出力だけで `None` を返し、各 test は `:56-59,85-89,114-117` で早期 return するため、skip が通常 pass と区別されない

## Accepted Baseline You May Assume

- ADR 0002: Explorer は network optional、Worker は network denied by default、Librarian は network allowed。role capability は runtime レベルで強制する
- ADR 0021: bwrap の network 制御は all-or-nothing。v0.1 default-deny は `--unshare-net`、allow は親 netns 継承。per-endpoint enforcement には proxy が必要
- Provider API calls は sandboxed tool process ではなく main process の provider client が実施し、auth は call ごとの `ProviderAuth` 注入で client に保持しない。この経路を変更しない
- `ToolExecutor` は shell / git_diff に `Arc<dyn Sandbox>` を注入する（`crates/tools/src/executor.rs:78-102`）。role policy から production sandbox を選ぶ composition point を明示して接続する
- Operator decision: v0.1 は full-open path を維持する。proxy selective egress と sandbox redesign は v0.2 に延期する

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/agents/`, `crates/runtime/`, `crates/tools/`, `crates/sandbox/`

Target part: RoleCapabilities.network から sandboxed tool execution の bwrap network namespace 方針への強制配線

## In Scope

- `NetworkAccess` 全 variant の fail-closed policy mapping
- runtime role policy から production sandbox/tool executor construction への network mode 伝播
- Denied=`--unshare-net`、Allowed=親 netns、OptIn=明示 opt-in がなければ deny
- Denied/Allowed を同一 TCP fixture で比較する実 bwrap integration test
- bwrap unavailable を pass と別に観測できる explicit skip reporting
- provider client path が bwrap 外のままであることの回帰確認

## Out Of Scope

- proxy / per-host / per-destination selective egress enforcement、DNS filtering — v0.2
- `NetworkPolicy::providers_only` を bwrap で実強制すること。bwrap 単体では不可能
- provider client の実行位置・認証・routing semantics の変更
- filesystem policy、approval policy、role tool matrix の再設計
- macOS / Windows sandbox backend

## Standalone Child Issue Contract

`turtton/evorch` で、issue #7 が定義した `RoleCapabilities.network` を issue #6 の bwrap sandbox dispatch に接続する。`NetworkAccess::Denied` は sandboxed shell / git_diff を `--unshare-net` 下で実行し、`Allowed` は ADR 0021 の v0.1 制約に従って親 network namespace を継承する。`OptIn` は明示 opt-in がない限り deny とする。policy mapping は全 variant を unit test し、実 bwrap integration では親 TCP endpoint に Denied role から接続できず Allowed role からは接続できることを証明する。Provider API calls は main process + per-call auth injection の既存経路を維持する。また `crates/sandbox/tests/bwrap.rs` の bwrap 不在時 early-return を、テスト出力上で pass と区別できる明示的な skip に変更する。per-destination proxy enforcement、sandbox redesign、provider routing 変更は実装しない。PR は `main` をターゲットにする。

## Acceptance Criteria

- `RoleCapabilities.network` が sandboxed tool execution の構築・dispatch 経路で消費され、宣言-only ではない
- Denied role の sandboxed tool は `--unshare-net` 下で実行され、親側で到達可能な TCP endpoint への接続が失敗する
- Allowed role は親 network namespace を継承し、同 endpoint への接続を維持する
- OptIn は明示 opt-in がない場合 deny となり、NetworkAccess 全 variant の mapping unit test がある
- role network mapping の追加後も既存 tool authorize/filter と role tool boundary が回帰しない
- OpenAI / Anthropic 等の provider client path と per-call auth injection に挙動変更がない
- bwrap 不在時の integration tests は pass ではなく explicit skip として観測できる
- v0.1 に proxy / per-destination filtering を持ち込まず、ADR 0021 の full-deny/full-open 二値を守る

## Verification

- role-to-network-mode unit tests（Denied / OptIn without opt-in / OptIn with opt-in / Allowed）
- real bwrap integration: 同一親 TCP listener への Denied failure / Allowed success
- existing runtime capability tests と tools executor tests
- provider regression tests（client は bwrap 外、auth は per-call injection）
- bwrap 有/無の両条件で integration test status が実行または explicit skip として判別可能
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Related Links

- intents/evorch/features/tools-sandbox/overview.md
- intents/evorch/features/orchestration/overview.md
- intents/evorch/decisions/0002-role-capability-boundaries.md
- intents/evorch/decisions/0021-bwrap-linux-sandbox.md
- Original v0.1 slices: issue #6 / v01-sandbox-approval、issue #7 / v01-agent-roles

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs
writeback is expected for this slice. Answer or explicitly decline:

- Intent placement: `features/tools-sandbox/overview.md` primary、`features/orchestration/overview.md` supporting
- ADR candidate: none（ADR 0021 の既存 decision を実装に接続する）
- Diagram candidate: none
- Docs update: none（role-facing surface なし）
- Closeout writeback expected: yes。issue #6 AC4 の provider allowlist 文言を ADR 0021 の v0.1 reality（full-open/full-deny、selective egress は v0.2）へ整合し、role mapping と explicit skip の確定結果を記録する

## Guide Reachability (G645)

While the author still knows the answer, name the guide surface and role that route to every
role-facing surface this slice adds, or explicitly say that no role-facing surface is added. A
blank answer is not treated as no-surface. The closeout record is a debt check, not a merge gate.

この slice は内部の role policy / tool composition / bwrap dispatch と test reporting のみを変更し、CLI / GUI / 対話 surface を追加しない。`no_role_facing_surface: true` を宣言する。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
