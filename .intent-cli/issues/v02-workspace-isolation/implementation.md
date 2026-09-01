# v02-workspace-isolation Implementation Packet

## Goal

`RunConfig` に `workspace_mode = shared | isolated` を追加し、isolated run では runtime が project の許可ディレクトリ配下に run 専用 git worktree を作成して cwd として割り当てる。branch mode を既定とし branch 名を `evorch/task/<run-id>` に固定する。sandbox / tool execution の構成を run ごとに選べる境界へ再設計し、承認済み tool call のみが isolated worktree と git operation に必要な `.git` metadata を writable で利用できるようにする。Worker は shell tool から直接 add / commit / push でき、現行 herdr の bundle 受け渡し回避策は廃止する。

## Why

v0.2 の自律実装ループでは複数 Worker が同一 repo を並列に編集し、branch + PR で成果を返す必要がある。現行 `AgentRuntime::production` は 1 つの `workspace_root` から composition-time に単一 `ToolExecutor` / `Sandbox` を作り、全 run で共有する。`RunConfig` は `interactive` / `name` しかなく、run ごとの cwd / worktree / branch identity を表現できない。このままでは並列 Worker の競合を防げず、現行 herdr 運用と同じく `.git` read-only のため bundle 受け渡しに依存する。agent-runtime-kernel / orchestration overview は shared / isolated、runtime 所有 worktree、branch 既定 `evorch/task/<run-id>` を既に確定し、grill `grill-v02-loop-foundation` Q1 は `.git` writable、approval 済み tool call にのみ sandbox を適用する境界、runtime 代理 git / bundle 自動化の不採用を確定した。本 packet はこの external worktree 運用を harness 内へ移す基盤 slice である。

## Scope

- `RunConfig` に型付き `WorkspaceMode::{Shared, Isolated}` を追加し、default は `Shared` として既存呼出しを維持する。必要な merge mode は branch 既定とし、patch を併設する場合も型付き enum とする
- project を「基準 repo/path + access allowed directories」として受け取る runtime seam を定義し、isolated worktree の配置先が許可集合配下であることを canonical path ベースで検証する。worktree は自動許可対象だが許可集合外へは作らない
- isolated mode で runtime-owned worktree manager が `git worktree add` 相当を実行し、run ごとに一意な path / cwd / branch `evorch/task/<run-id>` を作る。既存 branch / worktree との collision は型付き error で fail-closed
- shared mode は親 workspace / cwd を再利用し、worktree / branch を作らない。既存単一 workspace の挙動を保つ
- 現行 composition-time 単一 sandbox を、run workspace / policy を入力に tool execution 境界で選択できる構成へ再設計する。NetworkAccess → SandboxNetworkMode の既存 fail-closed 写像は維持する
- sandbox は「approval / RoleCapabilities を通過した tool call の実行境界」とし、writable mount が承認判定を置き換えない構造にする。deny / ask 未承認では process を起動しない
- isolated worktree root と git operation に必要な `.git` gitfile / common git dir の metadata を最小範囲で rw mount する。許可集合外・他 worktree・親 repo の不要領域は policy に従い ro / 非公開とする
- Worker の承認済み shell tool から `git status` / `git add` / `git commit` が成立する real git test を追加する。push は test remote を用いた local integration test で、credential を sandbox へ漏らさない既存境界を維持する
- worktree path / branch / workspace mode を run inspection または Event Bus detail から観測可能にし、Orchestrator / GUI が成果 branch を識別できるようにする
- 作成途中失敗、run 起動失敗、cancel、Done / Error の cleanup ownership を runtime に置く。runtime が作った worktree のみ削除し、user-owned worktree / branch は触らない。branch の削除時期は PR / closeout 消費を壊さない契約にする
- branch mode の成果は Worker が直接 commit / push し親が merge / cherry-pick / PR review に使う。patch mode を実装する場合は `.patch` artifact を返すが branch が既定

## Out of scope

- runtime が agent の代わりに git add / commit / push / PR 作成を行う git proxy service
- bundle の生成・受け渡し・import 自動化。v0.2 では bundle workaround を廃止する
- merge conflict の自動解決、auto-merge、`gh pr merge`。人間 approval と orchestrator loop は後続 `v02-orchestrator-loop`
- GUI project / thread 管理、worktree indicator、Diff tab — `v02-gui-workbench-restructure`
- remote filesystem / container / VM / macOS / Windows sandbox backend。Linux-first bwrap（ADR 0021）の範囲
- arbitrary user directory の自動 trust。project allowed directories に明示された範囲だけを使用する
- network selective egress / NetworkGuard 本体。既存 NetworkAccess fail-closed mapping を維持し、web tools は別 packet
- agent messaging / transcript persistence — `v02-agent-messaging`

## Verification

- unit test: `WorkspaceMode` serde / default（Shared）と既存 `RunConfig::default` 回帰
- integration test: shared mode は worktree / branch を作らず親 cwd を利用する
- tempfile git repo integration test: isolated mode が許可 dir 配下に一意 worktree、cwd、`evorch/task/<run-id>` branch を作る。許可外 path / collision / non-git repo は型付き error
- parallel integration test: 2 Worker run が同一 repo の同一 file を独立編集しても相互 checkout に漏れず、別 branch として観測できる
- real git integration test: approval 済み Worker shell で status / add / commit、local bare remote への push が成功する。`.git` metadata の必要範囲だけ writable
- authorization test: capability deny / approval deny / ask 未承認では shell / git process を起動せず、writable mount が bypass にならない
- bwrap integration test: worktree 内 write は成功、許可集合外 write は拒否、既存 NetworkAccess matrix は回帰なし
- failure injection: worktree add 後の run 起動失敗、cancel、Done / Error cleanup。runtime-owned のみ削除し user-owned worktree / branch を保持
- inspection / event test: workspace_mode / worktree path / branch identity が観測可能
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: `features/agent-runtime-kernel/overview.md` primary（RunConfig.workspace_mode / git worktree backend）。`features/orchestration/overview.md` の isolated workspace 運用を supporting とする。新規 intent は不要
- ADR candidate: decline — project trust / credential 境界は ADR 0008、Linux bwrap は ADR 0021 で既決。`.git` writable と bundle 廃止は grill Q1 で feature scope として確定済み
- Diagram candidate: decline — worktree ownership / branch flow は feature overview の記述で十分。runtime lifecycle に新 state が必要になった場合のみ follow-up candidate
- Docs update: decline — GUI project 設定は後続 packet。本 slice の Worker-facing route は Guide Reachability で宣言する
- Closeout learning: workspace_mode schema、worktree manager / cleanup ownership、allowed dirs validation、branch naming、rw mount 最小範囲、approval 強制位置、bundle 廃止を overview に記録する。`write_back_required: true`

- Guide reachability (G645): Worker が isolated workspace を利用する role-facing execution surface を追加するため `no_role_facing_surface: false`。route は orchestration overview の isolated workspace 運用 → Worker → `RunConfig.workspace_mode=isolated` / `evorch/task/<run-id>` worktree

`improve` (G456 / G460) is the later safety net; packet-time maintenance is the normal path.
