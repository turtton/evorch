## Goal

DirectSandbox の暗黙・無条件な構築を禁止し、production の標準 tool executor を bwrap で fail-closed に構築する composition root を導入する。orchestrator demo もその安全な入口を使い、bwrap detect failure 時は隔離なしで続行せず error にする。

## Why This Slice Exists Now

v0.1 inspect で、issue #6 / `v01-sandbox-approval` の fail-closed sandbox と issue #5 / `v01-tool-layer` の tool injection の間に production-safe construction がないことが見つかった。ADR 0021 は bwrap 不可時に危険ツールを実行しないと確定したが、公開 `DirectSandbox` は一行でその境界を迂回でき、現行 demo も実際に直接構築している。v0.1.1 の security fix round で composition root を閉じ、production wiring を fail-closed にする。

## Current Observed State

- `crates/sandbox/src/exec.rs:30-43` の `DirectSandbox` は public unit struct、`Copy` / `Default` で、constructor や policy を必要としない
- `crates/sandbox/src/lib.rs:17` が `DirectSandbox` を public re-export する
- `crates/tools/src/executor.rs:78-102` の `ToolExecutor::with_standard_tools` は任意の `Arc<dyn Sandbox>` を shell / git_diff にそのまま注入する低レベル API で、production-safe default を持たない
- `crates/runtime/examples/orchestrator_demo.rs:12,196-206` が `DirectSandbox` を直接 import / construct し、隔離なし tool executor を runtime に接続する
- `crates/sandbox/src/bwrap.rs:50-81` には detect + typed failure、`:111-123` には Sandbox 実装が既にあるため、execution semantics ではなく construction path が欠落している

## Accepted Baseline You May Assume

- ADR 0008: approval と OS sandbox は二層で、承認しても sandbox 外では実行しない
- ADR 0021: Linux v0.1 は bwrap、detect failure は fail-closed、DirectSandbox は明示 opt-out としてのみ存在すべき
- `ToolExecutor` は標準5 tool を登録し、shell / git_diff に同じ `Arc<dyn Sandbox>` を注入する
- orchestrator demo は外部 provider を使わない deterministic `ScriptedModel` であり、composition root 変更後も同じ event/runtime flow を維持する
- low-level sandbox injection は unit tests / recording sandbox / future custom integration で有用なため、production-safe entrypoint と明確に分離すれば残せる

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/sandbox/`, `crates/tools/`, `crates/runtime/examples/orchestrator_demo.rs`

Target part: DirectSandbox の明示的 opt-out 化と production tool executor の安全な構築入口

## In Scope

- DirectSandbox の private/sealed construction または explicit unsafe-policy argument/token
- workspace root / policy を必須とする production composition root
- bwrap detect failure の typed error と no-fallback invariant
- production-safe ToolExecutor constructor と低レベル injection API の区別
- orchestrator_demo の composition root 移行
- compile-fail/API visibility test、safe construction/failure tests、demo execution verification

## Out Of Scope

- sandbox exec semantics、bwrap argv、mount/network/env policy の変更
- approval policy / ToolExecutor execution flow の変更
- gVisor 等の新 backend、Landlock 再評価
- macOS / Windows 対応
- provider client、routing、runtime orchestration の機能変更

## Standalone Child Issue Contract

`turtton/evorch` に、標準 tool executor を production 向けに fail-closed 構築する composition root を追加する。呼び出し側は workspace root と必要 policy を渡し、composition root は `BwrapConfig` と `BwrapSandbox::detect` を用いて shell / git_diff へ sandbox を注入する。bwrap 不在・機能確認失敗時は typed error を返し、DirectSandbox へ fallback してはならない。`DirectSandbox` は policy なしの public unit construction を禁止し、test-only または明示的 opt-out intent を示す限定 API からのみ利用可能にする。`crates/runtime/examples/orchestrator_demo.rs` は新 composition root を使用し、既存 deterministic runtime flow を維持する。compile-fail または同等の API test で permissive sandbox を無意識に構築できないことを証明する。sandbox semantics、新 backend、他 OS は変更しない。PR は `main` をターゲットにする。

## Acceptance Criteria

- DirectSandbox を policy 明示なしの public value として構築できない
- production composition root は workspace root / policy を受けて BwrapSandbox を構築し、detect failure で fallback せず error を返す
- production standard ToolExecutor construction は composition root を経由し、shell / git_diff に fail-closed sandbox を注入する
- orchestrator_demo は `Arc::new(DirectSandbox)` を使用せず、新 composition root で構築される
- compile-fail または API visibility test が無意識な permissive construction を拒否する
- tests が bwrap available success、unavailable fail-closed、限定 explicit opt-out を検証する
- demo は既存 scripted flow を保って実行完了し、sandbox / approval execution semantics は変わらない
- 新 backend・他 OS・sandbox policy redesign を含めない

## Verification

- downstream-style compile-fail / privacy test for DirectSandbox construction
- composition root unit tests: safe success / bwrap unavailable failure / no fallback
- existing sandbox and tools tests（shell / git_diff / two-layer / approval）
- orchestrator demo を実 surface から実行して completion を確認
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Related Links

- intents/evorch/features/tools-sandbox/overview.md
- intents/evorch/decisions/0008-threat-model-phased-adoption.md
- intents/evorch/decisions/0021-bwrap-linux-sandbox.md
- Original v0.1 slices: issue #5 / v01-tool-layer、issue #6 / v01-sandbox-approval

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs
writeback is expected for this slice. Answer or explicitly decline:

- Intent placement: `features/tools-sandbox/overview.md`
- ADR candidate: none（ADR 0021 の fail-closed invariant を construction API に適用）
- Diagram candidate: none
- Docs update: none（role-facing surface なし）
- Closeout writeback expected: yes。production composition root の最終 API、DirectSandbox の限定 opt-out、bwrap unavailable error surface を記録する

## Guide Reachability (G645)

While the author still knows the answer, name the guide surface and role that route to every
role-facing surface this slice adds, or explicitly say that no role-facing surface is added. A
blank answer is not treated as no-surface. The closeout record is a debt check, not a merge gate.

この slice は内部の sandbox/tool construction API と example wiring のみを変更し、CLI / GUI / 対話 surface を追加しない。`no_role_facing_surface: true` を宣言する。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
