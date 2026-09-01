# v02-prompt-assembly Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

## Slice-specific review focus

- role / category binding が config-driven で logical model / preset reference を返し、runtime source の role/model hardcode や呼出し側だけの ad-hoc `RunConfig` 指定になっていないか
- prompt 本文を config に埋め込まず preset file に分離しているか。Config の `deny_unknown_fields` が prompt body field を拒否し、設定肥大化を再導入していないか
- bundled preset と user override が明確な2層で、package update が bundled のみ更新し user file を上書きしないか。preset name の path traversal / symlink escape / oversized input が fail-closed か
- assembly order が deterministic（role baseline → model-family → category → Orchestrator Intent Gate → preset/user append）で、BTreeMap / stable sort 等により同じ入力から byte-identical output を作るか
- model-family classifier が model id の table-driven / fail-safe な解決で、未知 model は generic base に落ちるか。model ごとの巨大完全 template や散在する string match を生やしていないか
- Intent Gate が Orchestrator のみに入り、Worker / Reviewer / Explorer に注入されないか。current user message のみ分類し、前 turn の mutation permission を持ち越さない契約が prompt / golden test にあるか
- Intent Gate が既存 evorch の task fields と Direct / Coordinated を保持し、omo の分類表 / dynamic keyTriggers を上乗せするか。固定 workflow を決める prompt へ変質していないか（ADR 0001）
- keyTriggers が available agents / skills metadata から stable sort / dedup で動的生成されるか。skill 本文ロードや AGENTS.md 注入を本 slice に持ち込んでいないか
-既存 Router の候補 identity `(profile, model_id)` を維持し、同一 model の異なる profile を別 fallback として辿れるか。omo の provider-list-inside-model candidate bug を再現していないか
- provider fallback / model fallback / both-axis fallback が diagnostics / event で before / after profile + model を保持するか。SessionAffinity pin が model override を誤って default_model に戻す既存挙動も必要に応じて回帰 test で固定 / 修正されているか
- missing preset / invalid config が provider call 前に型付き error となり、assembled prompt / user override / credential を logs に漏らさないか
- skill loader、project rules、compaction、goal continuation、provider auth、GUI editor まで scope を広げていないか

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

注: `intent-cli intent facet-check` は lexical な補助に留まる。上記 Slice-specific review focus が config/preset 分離、assembly determinism、Orchestrator-only gate、Router pair identity を確認する主たる観点である。

## Knowledge Writeback Expectation (G461)

`closeout_learning.write_back_required` は `true`。closeout で以下が host 側に記録されているか確認する（この PR 内または追跡可能な follow-up packet）。

- `features/orchestration/overview.md`: role/category binding schema、preset roots / precedence、assembly order、model family classifier、Intent Gate fields / Direct-Coordinated / dynamic keyTriggers、fallback axes diagnostics
- `features/agent-runtime-kernel/overview.md`: AgentRun role/category/route から resolved route と assembled System message へ至る runtime seam

記録が未実施の場合は、v0.2 prompt assembly 確定と実装の drift が残るため知識 writeback 不足として review 所見に残す。
