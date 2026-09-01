# v02-skill-loader Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

## Slice-specific review focus

- SKILL.md 形式の解釈が agentskills 公開仕様に一致するか。omo 実装固有の要素（`<skill-instruction>` wrapper、`plugin:skill` namespace）を v0.2 契約に混入させていないか（grill Q4 の精密化: 権威は agentskills 仕様）
- progressive disclosure が実装として守られているか。起動時に SKILL.md 本文や bundled resources を読み込む経路（token 膨張）を生やしていないか。metadata 一覧は name + description のみか
- 2 スコープの優先順位（repo > user）が正しく機能し、shadowing が diagnostic / event で観測可能か。黙って user スコープを採用する fail-open 経路がないか
- frontmatter 検証失敗・発見失敗が静かに捨てられていないか（ADR 0010「失敗は静かにしない」、Fault event 観測）
- skill-load surface が RoleCapabilities（check_tool）で gate され、role gate を迂回する経路がないか。未発見 skill 名への呼び出しがエラーになるか
- 委譲時 load_skills 相当の注入が子 run の prompt assembly に閉じているか。親 run の context に子の skill 本文が混入しないか
- bundled `scripts/` を loader が自動実行していないか（実行は agent の shell tool 明示呼び出しのみ。sandbox 承認契約の変更がないか）
- 既存 4 role（Orchestrator / Explorer / Worker / Reviewer）の実行・ToolExecutor 契約（jsonschema 検証 / 承認 / 制御マーカーエスケープ / ToolStarted / ToolCompleted）に回帰がないか
- skill 発見が task 開始時に確定するか（run 中の hot reload を持ち込んでいないか。ADR 0003 snapshot 方針との整合）

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

注: `intent-cli intent facet-check` は lexical な補助に留まる。上記 Slice-specific review focus が progressive disclosure の遵守・2 スコープ優先・role gate・失敗の観測可能性といった意味的接続を確認する主たる観点である。

## Knowledge Writeback Expectation (G461)

`closeout_learning.write_back_required` は `true`。closeout で以下が host 側に記録されているか確認する（この PR 内または追跡可能な follow-up packet）。

- `features/agent-runtime-kernel/overview.md`: skill loader の実装確定（agentskills 仕様準拠の検証ルール、2 スコープ発見と優先順位、progressive disclosure 3 段の注入点、skill-load surface の形式と role gate、委譲時 load_skills 相当注入、組み込みバリデーション）
- AgentRun 構造要件（id / role / category / skills / route / context / policy）の skills 軸が実装に接続された状態

記録が未実施の場合は、grill Q4 確定と実装の drift が残るため知識 writeback 不足として review 所見に残す。
