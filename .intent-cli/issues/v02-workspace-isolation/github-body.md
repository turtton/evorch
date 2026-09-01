## Goal

`RunConfig.workspace_mode = shared | isolated` を実装し、isolated run では runtime が project allowed directories 配下に run 専用 git worktree を作成して cwd として割り当てる。branch mode を既定、branch 名を `evorch/task/<run-id>` に固定する。sandbox / ToolExecutor を run workspace ごとに構成し、approval 済み Worker tool call から worktree と必要な `.git` metadata を writable にして直接 commit / push 可能にする。runtime 代理 git と bundle workaround は採用しない。

## Why This Slice Exists Now

v0.2 の自律実装ループは複数 Worker が同一 repo を並列編集し、branch + PR で成果を返す必要がある。現行 `RunConfig` は `interactive` / `name` のみで、`AgentRuntime::production` は単一 `workspace_root` から 1 つの Sandbox / ToolExecutor を composition-time に作って全 run で共有する。このままでは checkout 競合を防げず、`.git` read-only の現行 herdr 運用では bundle 受け渡しが必要になる。agent-runtime-kernel / orchestration overview は shared / isolated、runtime 所有 worktree、branch 既定を確定済み。grill `grill-v02-loop-foundation` Q1 は `.git` writable、approval 済み tool call にのみ sandbox を適用する境界、runtime 代理 git / bundle 自動化の不採用を確定した。

## Current Observed State

- `RunConfig`（`crates/runtime/src/run.rs`）の field は `interactive: bool` / `name: Option<String>` のみ。workspace mode / cwd / branch / worktree identity はない
- `RunTask` は run_id / role / prompt / config を保持し、run ごとの workspace resource を持たない
- `AgentRuntime::production`（`crates/runtime/src/runtime.rs`）は `ExecutionPolicy` と単一 `workspace_root` から `build_sandbox` を一度呼び、`ToolExecutor::with_standard_tools` を全 run で共有する
- `build_sandbox`（`crates/runtime/src/network.rs`）は `BwrapConfig::new(workspace_root)` + allow_network を構成し、doc に「1 AgentRuntime は 1 policy / 1 sandbox。run ごとの切替は executor API 再設計が必要」と明記されている
- `ExecutionPolicy` は RoleCapabilities による authorize / tool spec filter を実装し、Worker は read / edit / grep / shell / git_diff を許可する
- sandbox `ApprovalPolicy` は tool capabilities（fs_read / fs_write / process_spawn）を AutoAllow / Ask / Deny に分類できる
- NetworkAccess は `SandboxNetworkMode::{Unshared, ParentNetns}` へ fail-closed 写像済み。この契約は workspace 再設計後も維持対象
- runtime-owned git worktree manager、allowed directories validation、branch / worktree inspection、cleanup lifecycle は未実装

## Accepted Baseline You May Assume

- agent-runtime-kernel overview: `workspace_mode=shared` は親 cwd、`isolated` は runtime が git worktree を作り cwd に绑定。worktree rw、branch mode / patch mode、branch 既定 `evorch/task/<run-id>`
- orchestration overview: isolated workspace は現行 worktree 配置・`.git` read-only・bundle・`/tmp` 非共有の痛みを解消する正式運用
- grill `grill-v02-loop-foundation` Q1: worktree `.git` は writable。sandbox は approval 済み tool call のみの境界へ再設計。Worker shell から直接 commit / push。runtime proxy git と bundle 自動化は不採用
- ADR 0008: project trust / allowed directory と credential 分離を fail-closed に扱う
- ADR 0021: Linux-first bwrap、sandbox 構築失敗時は unsandboxed fallback をしない
- branch naming convention: `evorch/task/<run-id>`

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/runtime/`, `crates/sandbox/`

Target part: RunConfig.workspace_mode、runtime-owned git worktree / branch、project allowed dirs、approval 済み tool call の writable workspace / `.git` sandbox

## In Scope

- `WorkspaceMode::{Shared, Isolated}` と `RunConfig` field。default Shared で既存挙動維持
- project 基準 repo/path + allowed directories を runtime へ渡す seam、canonical path による worktree 配置先検証
- isolated mode の runtime-owned worktree manager。一意 path / cwd / `evorch/task/<run-id>` branch、collision / non-git / invalid path の型付き error
- shared mode は親 workspace / cwd を再利用し worktree / branch 非作成
- Sandbox / ToolExecutor を run workspace / policy ごとに安全に選択できる composition 再設計。既存 NetworkAccess mapping 維持
- approval / RoleCapabilities 通過後だけ process を起動する境界。deny / ask 未承認は filesystem / git 非実行
- isolated worktree と git operation に必要な `.git` gitfile / common metadata の最小 rw mount。許可集合外 / 他 worktree は ro または非公開
- Worker shell から status / add / commit / local test remote push が成立する real git integration
- workspace_mode / worktree path / branch identity の inspection / Event Bus 観測
- runtime-created resource の failure-safe cleanup。user-owned worktree / branch は非変更。PR / review に必要な branch lifecycle を保持
- branch mode 既定。patch mode を併設する場合は `.patch` artifact を返す

## Out Of Scope

- runtime proxy git service（agent 代理の add / commit / push / PR 作成）
- git bundle 生成 / import / relay 自動化。bundle workaround は廃止
- merge conflict 自動解決、auto-merge、`gh pr merge`、human approval flow — `v02-orchestrator-loop`
- GUI project / thread / worktree indicator / Diff tab — `v02-gui-workbench-restructure`
- remote filesystem、container / VM、macOS / Windows sandbox backend
- project allowed directories 外の自動 trust
- selective network egress / NetworkGuard 本体
- messaging / transcript persistence — `v02-agent-messaging`

## Standalone Child Issue Contract

`turtton/evorch` の `crates/runtime/` と `crates/sandbox/` に `RunConfig.workspace_mode = shared | isolated` を実装する。default は shared とし、shared は親 workspace / cwd を再利用して worktree を作らない。isolated は runtime-owned worktree manager が project allowed directories 配下に run 専用 git worktree を作り、cwd と branch `evorch/task/<run-id>` を割り当てる。canonical path 検証で許可外配置、collision、non-git repo を fail-closed にする。現行 composition-time 単一 Sandbox / ToolExecutor を run workspace / policy ごとに構成できる境界へ変更し、RoleCapabilities と approval を通過した tool call だけを実行する。isolated worktree と git operation に必要な `.git` gitfile / common metadata の最小範囲を rw とし、Worker shell から status / add / commit / local test remote push が成功する integration test を追加する。deny / ask 未承認では process を起動せず、許可集合外 write を拒否する。workspace_mode / worktree path / branch を inspection / event で観測可能にし、作成失敗・run failure・cancel・完了で runtime-created resource のみ安全に cleanup する。runtime proxy git、bundle、auto-merge、GUI project management は実装しない。PR は `main` をターゲットにする。

## Acceptance Criteria

- `WorkspaceMode::{Shared, Isolated}` が型付きで追加され、`RunConfig::default()` は Shared。既存 callers の回帰 test がある
- shared mode は親 cwd を利用し worktree / branch を作らない integration test がある
- isolated mode は allowed dir 配下に一意 worktree / cwd / `evorch/task/<run-id>` branch を作り、許可外 path を拒否する test がある
- 並列 run が異なる worktree / branch / cwd を持ち、同一 file の変更が相互に漏れない integration test がある
- approval 済み Worker shell から git add / commit / local remote push が成功し、必要な `.git` metadata が writable である real git test がある
- capability deny / approval deny / ask 未承認では git / shell process を起動せず、writable `.git` が bypass にならない test がある
- worktree 内 write は成功、allowed dirs 外 write は bwrap で拒否される integration test がある
- worktree 作成途中失敗 / run failure / cancel / Done の cleanup が runtime-created resource のみに作用し user-owned worktree / branch を保持する test がある
- branch mode が既定で、workspace mode / path / branch identity が inspection / event から観測可能
- NetworkAccess fail-closed matrix と standard tools の回帰 test が pass する
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check` が pass する

## Verification

- WorkspaceMode serde / default unit test
- shared / isolated tempfile git repo integration test
- parallel worktree isolation test
- approval 済み shell の git status / add / commit / local push real integration test
- capability / approval deny-before-exec test
- bwrap mount boundary test（worktree + `.git` rw、allowed dirs 外 write deny、network mapping regression）
- failure injection cleanup test（runtime-owned のみ）
- inspection / Event Bus identity test
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Related Links

- intents/evorch/features/agent-runtime-kernel/overview.md（RunConfig.workspace_mode / git worktree backend）
- intents/evorch/features/orchestration/overview.md（isolated workspace 正式運用）
- intents/evorch/decisions/0008-threat-model-phased-adoption.md
- intents/evorch/decisions/0021-bwrap-linux-sandbox.md
- intents/evorch/interviews/grill-v02-loop-foundation.json（Q1）
- 後続 slice: `v02-gui-workbench-restructure`、`v02-orchestrator-loop`

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs
writeback is expected for this slice. Answer or explicitly decline:

- Intent placement: `features/agent-runtime-kernel/overview.md` primary。supporting: orchestration overview、ADR 0008 / 0021、grill record。新規 intent は不要
- ADR candidate: none（project trust / bwrap は既存 ADR、`.git` writable / bundle 廃止は grill Q1 で確定済み）
- Diagram candidate: none
- Docs update: none（GUI project surface は後続）
- Closeout writeback expected: yes。workspace_mode schema、worktree manager / cleanup、allowed dirs、branch naming、rw mount / approval boundary を overview に記録する

## Guide Reachability (G645)

While the author still knows the answer, name the guide surface and role that route to every
role-facing surface this slice adds, or explicitly say that no role-facing surface is added. A
blank answer is not treated as no-surface. The closeout record is a debt check, not a merge gate.

この slice は Worker が利用する role-facing execution surface（isolated workspace / branch）を追加する。route: orchestration overview の isolated workspace 運用 → Worker → `RunConfig.workspace_mode=isolated` / `evorch/task/<run-id>` worktree。`no_role_facing_surface: false` を宣言する。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
