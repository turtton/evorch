## Goal

agentskills 仕様（github.com/agentskills/agentskills）準拠の skill loader を evorch runtime に実装し、AgentRun の skills 軸を実装に接続する。skill は `skill-name/SKILL.md`（YAML frontmatter + Markdown 本文）+ 任意の bundled resources（scripts / references / assets）で構成。発見は repo / user の 2 スコープ（repo > user 優先）。注入は progressive disclosure 3 段: 起動時は name + description の metadata 一覧のみ → agent が活性化を判断した時点で SKILL.md 本文をロード（skill-load surface）→ resources は必要時のみ。委譲時は load_skills 相当で子 run の prompt assembly に skill 本文を注入する。frontmatter 検証（skills-ref validate 相当）を組み込みで行い、違反は Fault event で観測可能にする。

## Why This Slice Exists Now

v0.2 成功基準は「evorch orchestrator が goal 投入から merge 承認まで self-contained に完走（OpenCode / omo / herdr 非依存）」であり、現行の skill 機能は外部 harness（omo）側にあるため evorch 単体では動かない。AgentRun 構造要件（agent-runtime-kernel overview: id / role / category / skills / route / context / policy）と Agent の 5 軸分解（orchestration overview: Role + Category + Skills + Execution Policy + Route Policy）の skills 軸が構想止まりで未接続である。grill session `grill-v02-loop-foundation`（11/11 accepted、`intents/evorch/interviews/grill-v02-loop-foundation.json`）の Q4 で実装形態が全確定した（agentskills 仕様準拠 / progressive disclosure / 2 スコープ / 組み込み検証）。Q4 の精密化: SKILL.md 形式の権威を omo 実装ではなく agentskills 公開仕様に置き、omo（buildAvailableSkills / load_skills）は発見・注入の参照実装として使う。

## Current Observed State

- 現行 runtime は system prompt / prompt assembly を持たない: `run_agent`（`crates/runtime/src/agent_loop.rs:49-72`）は task prompt を `push_user` で user message として積むのみで、skills metadata を露出する接続点が存在しない（v02-prompt-assembly が assembly 層を新設する）
- `RunConfig`（`crates/runtime/src/run.rs:35-40`）は `{ interactive, name }` のみで、skills / category を保持しない
- `RoleCapabilities`（`crates/agents/src/capability.rs:27-34`）の allowed_tools に skill-load 相当の surface は存在しない。`ToolExecutor`（`crates/tools/src/executor.rs:92-108`）の標準 tool は read / edit / grep / shell / git_diff の 5 本
- `Config`（`crates/config/src/types/mod.rs:30`、deny_unknown_fields）は providers / routing / panel / diagnostics / permissions / metrics のみで skill 設定なし。一方 `crates/config/src/load.rs` は evorch.toml / config.d/*.toml と user config dir（$XDG_CONFIG_HOME/evorch または ~/.config/evorch）の解決を既に実装しており、2 スコープ発見の土台になる
- YAML frontmatter 解析依存（serde_yaml 等）は workspace dependencies 未導入（workspace Cargo.toml）— 追加 feasibility 確認を実装タスクに含める
- skill 発見・検証の失敗を流す先として `EventKind::Fault`（`crates/event-bus/src/event.rs:63-76`）は既存
- skill は構想（agent-harness-concept.md の AgentRun.skills / Stable Prefix の skill snapshot）と roadmap（mvp-roadmap.md v0.2 節）にのみ存在し、コード実装はゼロ

## Accepted Baseline You May Assume

- grill grill-v02-loop-foundation Q4 確定（`intents/evorch/interviews/grill-v02-loop-foundation.json`）: 形式は agentskills 仕様（github.com/agentskills/agentskills）準拠、注入は progressive disclosure（omo 遅延ロード同型）、配置スコープは repo / user の 2 スコープ（優先順位 repo > user）、検証は skills-ref validate 相当を組み込み側で行う
- agentskills 仕様の制約値: name 必須（1-64 文字、小文字英数 + hyphen、連続 hyphen 不可、親ディレクトリ名と一致）/ description 必須（≤1024 文字）/ 任意: license、compatibility（≤500）、metadata（string map）、allowed-tools（実験的）/ SKILL.md 500 行以下推奨・本文 <5000 tokens 推奨 / metadata ~100 tokens / bundled resources の file 参照は 1 段
- v02-prompt-assembly が prompt assembly 層（role / category → 論理モデル結線、preset / override 2 層）を新設する（本 slice は依存として消費する側）
- ADR 0010: skill discovery は built-in と同等の typed 扱い、load / transform 失敗は静かにしない（DiagnosticBus / event 観測）
- ADR 0002: role gate は `RoleCapabilities` で runtime レベル強制（role.rs の v0.2 拡張レシピ手順）
- ADR 0003: AGENTS.md / skills は task 開始時に snapshot 化して固定（本 slice の発見は task 開始時に確定）

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/agents/`, `crates/runtime/`, `crates/config/`

Target part: agentskills 仕様準拠の skill loader。2 スコープ発見（repo > user）、progressive disclosure（起動時 metadata 露出 / 活性化時本文ロード / resources 必要時）、skill-load surface（RoleCapabilities role gate 付き）、委譲時 load_skills 相当注入、組み込みバリデーション

## In Scope

- skill loader モジュール（`crates/agents/`）: 発見 / frontmatter 検証 / metadata 一覧生成 / 本文ロード
- agentskills 仕様準拠の frontmatter 検証と bundled resources（scripts / references / assets）解決、1 段相対参照の制限
- repo / user 2 スコープ発見（repo: project root 基準の既定ディレクトリ、user: $XDG_CONFIG_HOME/evorch/skills 既定 ~/.config/evorch/skills）。同名 skill は repo 優先、shadowing は diagnostic / event で観測可能
- 起動時（run 開始時）の name + description metadata 一覧露出（prompt assembly 層経由。skill が無い場合は prompt に影響しない）
- skill-load surface（tool または meta op — 実装 slice で確定）。RoleCapabilities で gate、初期付与は Orchestrator / Explorer / Worker。未発見 skill 名はエラー
- 委譲時 load_skills 相当: 子 run の prompt assembly への skill 本文注入（親 run の context には混入しない）
- skills-ref validate 相当の組み込みバリデーション。不正 skill の除外 / invalid 明示と Fault event 観測（ADR 0010「失敗は静かにしない」）
- 発見は task 開始時に確定（ADR 0003 snapshot 方針）

## Out Of Scope

- システム同梱 skill セットの内容作成 — loader は形式を受け付けるのみ
- bundled `scripts/` の自動実行・sandbox への自動登録（agent の shell tool 明示実行のみ）
- 動的 plugin / WASM からの skill 提供、namespace 付き plugin skill（ADR 0010 v0.3+）
- skill の hot reload（run 中の発見更新の即時反映）
- GUI での skill 管理・一覧 UI
- prompt assembly 層自体の実装 — v02-prompt-assembly の scope

## Standalone Child Issue Contract

`turtton/evorch` で、agentskills 仕様（github.com/agentskills/agentskills）準拠の skill loader を `crates/agents/` に実装する。SKILL.md frontmatter 検証（name: 1-64 文字・小文字英数 + hyphen・連続 hyphen 不可・親ディレクトリ名一致、description ≤1024 文字、任意 field 受付）と bundled resources（scripts / references / assets、1 段相対参照）を組み込みで行う（skills-ref validate 相当）。発見は repo / user 2 スコープ（既定: repo は project root 基準、user は $XDG_CONFIG_HOME/evorch/skills）で repo > user 優先、shadowing は観測可能。run 開始時は name + description の metadata 一覧のみ prompt assembly に露出し、agent が skill-load surface（tool または meta op、RoleCapabilities で gate、初期付与 Orchestrator / Explorer / Worker）で skill 名を指定した時点で SKILL.md 本文を返却する（resources は必要時）。委譲時は load_skills 相当で子 run の prompt assembly に skill 本文を注入する。検証失敗・発見失敗は Fault event で観測可能にする。システム同梱 skill の作成、scripts の自動実行、動的 plugin、hot reload、GUI 管理 UI は実装しない。PR は `main` をターゲットにする。

## Acceptance Criteria

- SKILL.md frontmatter 検証が agentskills 仕様に準拠する（name / description 必須制約、命名規則、任意 field）。違反はエラーとして観測可能であることを unit test で検証
- bundled resources（scripts / references / assets）を解決し、本文からの file 参照は 1 段の相対参照に制限されることを unit test で検証
- repo / user 2 スコープ発見が動作し、同名 skill は repo スコープが優先すること、shadowing が diagnostic / event で観測可能であることを unit test で検証
- run 開始時は metadata 一覧のみが露出し、SKILL.md 本文はロードされないことを unit test で検証（progressive disclosure 第 1 段）
- skill-load surface で skill 名を指定すると本文が返却され、未発見 skill 名はエラーになることを unit test で検証（第 2 / 第 3 段を含む）
- 委譲時 load_skills 相当が子 run の prompt assembly に skill 本文を注入することを unit test で検証（親 context 非混入を含む）
- skills-ref validate 相当の組み込みバリデーションで不正 skill が metadata 一覧に現れない（または invalid 明示）ことを unit test で検証
- skill-load surface の role gate が RoleCapabilities で機能し、未許可 role が拒否されることを unit test で検証
- 既存 4 role（Orchestrator / Explorer / Worker / Reviewer）の実行に回帰がないこと
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check` が pass する

## Verification

- frontmatter 検証 unit test（name の各違反パターン、description / compatibility 上限、任意 field 受付）
- bundled resources 解決と 1 段参照制限 unit test
- 2 スコープ発見・repo 優先・shadowing 観測 unit test
- progressive disclosure unit test（起動時 metadata のみ / skill-load で本文 / resources 必要時、未発見 skill エラー）
- 委譲時 load_skills 注入 unit test（親 context 非混入）
- 不正 skill の除外 / invalid 明示 + Fault event 観測 unit test
- role gate unit test（未許可 role 拒否）
- 既存 4 role 実行・ToolExecutor 契約の回帰確認
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Related Links

- intents/evorch/features/agent-runtime-kernel/overview.md（AgentRun 構造要件 / v0.2 loop 基盤 packet 索引）
- intents/evorch/features/orchestration/overview.md（Agent の 5 軸分解）
- intents/evorch/decisions/0010-extension-architecture-v2-style.md
- intents/evorch/decisions/0002-role-capability-boundaries.md
- intents/evorch/decisions/0003-cache-first-context-engine.md（skills snapshot 方針）
- intents/evorch/interviews/grill-v02-loop-foundation.json（Q4）
- agentskills 仕様: github.com/agentskills/agentskills
- 兄弟 slice: `v02-prompt-assembly`（prompt assembly 層 — 依存）、`v02-project-rules`（AGENTS.md / rules 注入）、`v02-orchestrator-loop`（loop 消費者）

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs
writeback is expected for this slice. Answer or explicitly decline:

- Intent placement: `features/agent-runtime-kernel/overview.md` primary（skills 軸 / v0.2 loop 基盤 packet 索引へ反映）。supporting: orchestration overview、ADR 0010 / 0002、interviews/grill-v02-loop-foundation.json
- ADR candidate: none（形式権威・スコープ優先・progressive disclosure は grill Q4 で確定済み。ADR 0010 の延長）
- Diagram candidate: none
- Docs update: none（skill 形式は agentskills 公開仕様参照。evorch 固有差分は発見ディレクトリ配置のみ）
- Closeout writeback expected: yes。発見ディレクトリ既定配置、優先順位と shadowing 観測、progressive disclosure の注入点、skill-load surface の最終形式と role 付与、検証ルールとエラー観測経路、YAML 解析依存の選定を agent-runtime-kernel overview に記録する

## Guide Reachability (G645)

While the author still knows the answer, name the guide surface and role that route to every
role-facing surface this slice adds, or explicitly say that no role-facing surface is added. A
blank answer is not treated as no-surface. The closeout record is a debt check, not a merge gate.

この slice は agent が呼び出す role-facing surface（skill-load）を追加するため `no_role_facing_surface: false`。route: agent-runtime-kernel overview で宣言される skill 利用 surface（skill-load）→ (1) run 開始時の skills metadata 露出と skill-load 呼び出し（meta op / tool 設定、role: Orchestrator）、(2) 実行中 agent からの skill-load 呼び出し（RoleCapabilities 許可対象、role: Worker）。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
