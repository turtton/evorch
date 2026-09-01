# v02-skill-loader Implementation Packet

## Goal

`crates/agents/` に agentskills 仕様（github.com/agentskills/agentskills）準拠の skill loader を実装する。skill は `skill-name/SKILL.md`（YAML frontmatter + Markdown 本文）+ 任意の bundled resources（`scripts/` / `references/` / `assets/`）で構成し、frontmatter 検証（name: 必須・1-64 文字・小文字英数 + hyphen・連続 hyphen 不可・親ディレクトリ名と一致 / description: 必須・≤1024 文字 / 任意で license・compatibility ≤500・metadata・allowed-tools）を組み込みで行う（skills-ref validate 相当）。発見は repo / user の 2 スコープで優先順位は repo > user。注入は progressive disclosure 3 段とする: 起動時は name + description の metadata 一覧のみ（~100 tokens/skill 目安）、agent が活性化を判断した時点で SKILL.md 本文をロード（<5000 tokens・500 行以下推奨）、resources は必要時のみ。本文ロードは agent が呼び出せる skill-load surface（ToolExecutor 登録 tool または meta op — 実装 slice で確定）で提供し、委譲時は load_skills 相当で子 run の prompt assembly に skill 本文を注入する（omo の buildAvailableSkills / load_skills 同型）。metadata 露出と本文注入の接続点は v02-prompt-assembly が確立する prompt assembly 層であり、本 slice はそこに skills 軸を供給する。

## Why

v0.2「実装ループ内製化」の対になる slice である。現行の skill 機能は外部 harness（omo）側にあり、evorch 単体では AgentRun の skills 軸（agent-runtime-kernel overview の AgentRun 構造要件、orchestration overview の 5 軸分解）が構想止まりで未接続。omo / herdr 非依存のループ完走（v0.2 成功基準）には、skill 発見・検証・遅延ロードを evorch runtime 内に持つ必要がある。grill session `grill-v02-loop-foundation`（11/11 accepted、`intents/evorch/interviews/grill-v02-loop-foundation.json`）の Q4 で実装形態が確定済みであり、本 packet はその実装落とし込みである。Q4 の精密化点（初回回答から変更）: SKILL.md 形式の権威を omo 実装ではなく agentskills 公開仕様に置き、omo は発見・注入の参照実装として使う。ADR 0010 の「失敗は静かにしない」に従い、frontmatter 違反・発見失敗は Fault event で観測可能にする。

## Scope

- skill loader モジュールを `crates/agents/` に実装する（発見 / frontmatter 検証 / metadata 一覧生成 / 本文ロード）。`architecture.md` の crate 分担（agents/ = role, category, skills）に従う
- agentskills 仕様準拠の frontmatter 検証: name / description の必須制約と命名規則、任意 field の受付、bundled resources（`scripts/` / `references/` / `assets/`）の解決。本文からの file 参照は 1 段の相対参照に制限する
- 発見は 2 スコープ: repo スコープ（project root 基準。既定ディレクトリは `.evorch/skills/`、実装 slice で最終確定）と user スコープ（`$XDG_CONFIG_HOME/evorch/skills`、既定 `~/.config/evorch/skills`）。同名 skill は repo スコープが優先され、優先（shadowing）の発生は diagnostic / event で観測可能にする
- 起動時（run 開始時）は name + description の metadata 一覧のみを prompt assembly に露出する（skill が 1 つも無い run では prompt に影響しない）
- skill-load surface: agent が skill 名を指定して SKILL.md 本文を取得する tool / meta op。実行は RoleCapabilities（`check_tool`）で gate し、初期付与は Orchestrator / Explorer / Worker とする（Reviewer への付与要否は実装 slice で判断）。発見済みでない skill 名への呼び出しはエラーを返す
- 委譲時の load_skills 相当: 委譲時に skill 名リストを渡すと、子 run の prompt assembly が該当 skill 本文を注入する。親 run の context には混入しない
- 組み込みバリデーション（skills-ref validate 相当）: 不正な frontmatter の skill は metadata 一覧から除外（または invalid として明示）し、違反内容を Fault event で観測可能にする
- skill 発見は task 開始時に確定する（ADR 0003 の snapshot 方針に整合。run 中の追加 skill の即時反映は行わない）

## Out of scope

- システム同梱 skill セットの内容作成 — loader は形式を受け付けるのみ。同梱 skill の作成は別 slice
- bundled `scripts/` の自動実行・sandbox への自動登録 — scripts は agent が shell tool で明示実行する補助スクリプトであり、loader は実行しない
- 動的 plugin / WASM からの skill 提供 — ADR 0010 の v0.3+ 項目。namespace 付き plugin skill（omo の `plugin:skill` 相当）も対象外
- skill の hot reload（run 中の発見ディレクトリ更新の即時反映）
- GUI での skill 管理・一覧 UI
- prompt assembly 層自体の実装 — v02-prompt-assembly の scope（本 slice は依存として消費する側）

## Verification

- unit test: frontmatter 検証（name 制約の各違反パターン — 64 文字超・大文字・連続 hyphen・親ディレクトリ名不一致、description 欠落・1024 文字超、compatibility 500 文字超）
- unit test: bundled resources 解決と 1 段参照制限
- unit test: repo / user 2 スコープ発見、同名 skill の repo 優先と shadowing 観測
- unit test: 起動時は metadata 一覧のみ（本文未ロード）、skill-load surface での本文取得、未発見 skill のエラー
- unit test: 委譲時 load_skills 相当の子 run 注入（親 context 非混入を含む）
- unit test: 不正 skill の metadata 一覧除外 / invalid 明示と Fault event 観測
- unit test: skill-load surface の role gate（未許可 role の拒否）
- 既存 4 role 実行の回帰確認（skill が存在しない場合に prompt への影響がないことを含む）
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: `features/agent-runtime-kernel/overview.md` を primary とし、skill loader の確定実装を AgentRun 構造の skills 軸 / v0.2 loop 基盤 packet 索引に反映する。supporting: orchestration overview（5 軸分解の Skills 軸）、ADR 0010 / 0002、interviews/grill-v02-loop-foundation.json。新規 intent は不要
- ADR candidate: decline — SKILL.md 形式の権威（agentskills 公開仕様）、2 スコープ優先順位、progressive disclosure は grill Q4 で確定済み。ADR 0010 の延長であって新決定ではない
- Diagram candidate: decline — 発見 → metadata 露出 → 本文ロード → resources の経路は overview の記述で十分
- Docs update: decline — skill 形式は agentskills 公開仕様を参照すれば十分であり、evorch 固有の差分は発見ディレクトリ配置のみ（overview 記載でカバーする）
- Closeout learning: skill loader の実装確定（発見ディレクトリ既定配置、優先順位と shadowing 観測、progressive disclosure の注入点、skill-load surface の最終形式と role 付与、検証ルールとエラー観測経路、YAML 解析依存の選定）を overview に記録する。`write_back_required: true`

- Guide reachability (G645): skill-load surface は agent（Orchestrator / Worker が主利用者）が呼び出す role-facing surface を追加するため `no_role_facing_surface: false`。route: agent-runtime-kernel overview で宣言される skill 利用 surface（skill-load）→ run 開始時の skills metadata 露出と skill-load 呼び出し（meta op / tool 設定）、および実行中 agent からの skill-load 呼び出し（RoleCapabilities 許可対象）

`improve` (G456 / G460) is the later safety net; packet-time maintenance is the normal path.
