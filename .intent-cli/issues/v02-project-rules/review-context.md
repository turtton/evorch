# v02-project-rules Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

## Slice-specific review focus

- startup が root AGENTS.md（+ user scope）だけを注入し、repository 全体の nested rules を eager scan していないか
- post-tool seam が read/edit 等の明示 target path から resolver を起動し、tool 成功後にだけ synthetic message を作るか。shell output の文字列から path を推測していないか
- 対象 directory→project root の探索を root→deep に並べ直し、全 AGENTS.md を注入するか。deep 一件だけ、root 一件だけ、兄弟混入、project root より上への漏出がないか
- closest-wins が ordered injection（root first / deep last）で保たれ、複数 path の union/dedup が決定論的か。同じ source/content を同一 turn に重複注入して context を膨らませないか
- synthetic content が最後の user message 直前の system notification 層に入り、ToolResult、source file bytes、ToolStarted/ToolCompleted event を変更していないか
- `.omo/rules` / `.claude/rules` / `.cursor/rules` / `.github/instructions` の alwaysApply/glob が project-relative normalized path に対して評価されるか。non-match / invalid glob が fail-open で適用されないか
- project trust 未承認時に project AGENTS.md / rules を startup/post-tool の両方で読まないか。user scope と project scope が混同されていないか（ADR 0008）
- dynamic truncation が固定文字数切捨てではなく context state を考慮し、UTF-8、root→deep ordering、closest rule 優先を守るか。省略 source と再取得可能性が marker で観測できるか
- cache が bounded で、rules file edit 後に invalidation されるか。cache stale により古い指示を継続注入しないか
- AGENTS.md lint/semantic merge、skill loader、project trust GUI、intent gate へ scope を広げていないか

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

注: lexical facet-check は closest-wins ordering、trust-before-load、post-tool insertion point の意味的保証を検査できない。fixture と AgentContext integration test を主根拠とする。

## Knowledge Writeback Expectation (G461)

`closeout_learning.write_back_required` は `true`。closeout で以下が記録されているか確認する。

- `features/agent-runtime-kernel/overview.md`: startup root-only、path resolver、root→deep synthetic injection、scoped rules、dynamic truncation、dedup/cache
- `features/tools-sandbox/overview.md`: project trust 未承認時の rules 非ロードと read/edit tool seam で source bytes/ToolResult を変更しない境界

未記録なら、runtime prompt と repository policy の実装 drift として review 所見に残す。
