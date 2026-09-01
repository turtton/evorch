## Goal

trusted project の nested `AGENTS.md` と scoped rules を path-aware に AgentRun context へ注入する。起動時は root（+ user scope）のみ、read/edit 等の tool 成功後は対象 file から root までを探索して root→deep 順に複数 rules を synthetic message として最後の user message 直前へ挿入する。`.omo/rules` / `.claude/rules` / `.cursor/rules` / `.github/instructions` は `alwaysApply` / `glob` で絞り、残 context に応じて dynamic truncation する。

## Why This Slice Exists Now

自律実装 loop の Worker/Reviewer は、repository root の共通規約だけでなく触れた subtree の局所規約をその turn で知る必要がある。全 rules の startup 一括注入は context 浪費、root-only は closest-wins 不成立になる。grill `grill-v02-loop-foundation` Q7 は omo の `findAgentsMdUp` / context-injector を参照し、startup root-only、tool 後 root→deep 複数注入、scoped rules、dynamic truncation を確定した。v02-prompt-assembly に続き、ToolExecutor の target path と各 AgentRun の model-visible context を安全に接続する slice である。

## Current Observed State

- `crates/tools/src/tool.rs` の統一 Tool trait と `crates/tools/src/executor.rs` の ToolExecutor は read / edit / grep / shell / git_diff を実行し、schema validation・event emit・結果正規化を行う
- `crates/runtime/src/agent_loop.rs` の各 `LoopState` は独立した `AgentContext` を持つが、tool target path から project rules を解決する seam はない
- `crates/config/src/types/mod.rs` は deny_unknown_fields の strict v2 schema で、rule directory / truncation 設定は未実装
- `tools-sandbox/overview.md` と ADR 0008 は project trust 未解決時に AGENTS.md / skills / MCP 設定をロードしない要件を持つ
- 現行 runtime に nested AGENTS.md discovery、closest-wins ordering、scoped rules metadata、dynamic truncation はない

## Accepted Baseline You May Assume

- grill Q7: startup は root + user scope のみ。read/edit 等 tool 後に対象 path の rules を synthetic system notification として最後の user message 直前へ挿入
- nested AGENTS.md は対象 directory から親へ探索し、root→deep 順に複数注入。deep/closest が後に来て優先される
- scoped dirs: `.omo/rules`, `.claude/rules`, `.cursor/rules`, `.github/instructions`; `alwaysApply` / `glob` metadata
- omo reference は read/write/edit/multiedit 後 context injection、dynamic truncator、bounded cache を採用。本 slice は Rust runtime/tool seam に適合させる
- ADR 0008: project trust 前に project rules を読まない
- v02-prompt-assembly が startup/runtime context assembly seam を提供する

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/runtime/`, `crates/tools/`, `crates/config/`

Target part: startup root rules、nested AGENTS.md closest-wins resolver、post-tool synthetic injection、scoped rules alwaysApply/glob、dynamic truncation

## In Scope

- project root/user scope を境界とする rule resolver と project trust gate
- startup root AGENTS.md + user always-apply のみの注入
- read/edit を最低対象とする path-bearing tool 成功後の resolver 起動。安全に path を抽出できる grep 等は同 seam に追加可能
- target directory→root 探索、root→deep 全件 ordered injection、兄弟非混入、root 上探索禁止
- 最後の user message 直前の synthetic system notification。ToolResult / disk bytes / event payload は不変
- multi-path stable union / dedup と同一 turn 重複抑止
- 4 scoped rules directory、alwaysApply、glob、invalid pattern diagnostics
- context window/used tokens/response headroom に基づく dynamic truncation、UTF-8 安全、closest rule 優先、省略 marker
- bounded cache と rules edit 後 invalidation

## Out Of Scope

- AGENTS.md の自然言語 lint、矛盾解消、key 単位 semantic merge
- source file / ToolResult / control-marker sanitizer の変更
- shell output から arbitrary path を推測すること、repository 全体 eager scan
- 確定リスト外 rules directory の追加
- SKILL.md loader（`v02-skill-loader`）
- project trust の GUI / sandbox policy 本体
- intent gate / prompt preset 本体（`v02-prompt-assembly`）

## Standalone Child Issue Contract

`turtton/evorch` に project rules context injector を実装する。trusted project の startup では root AGENTS.md と user scope rules のみを注入し、read/edit 等の path-bearing tool 成功後に対象 directory から project root まで AGENTS.md を探索、root→deep 順で全件を最後の user message 直前の synthetic system notification として一度だけ挿入する。複数 path は stable union/dedup、兄弟 rules と root 上は対象外。`.omo/rules` / `.claude/rules` / `.cursor/rules` / `.github/instructions` は alwaysApply または project-relative glob match 時のみ適用する。project trust 未承認時は project rules を一切ロードしない。dynamic truncation は context state、UTF-8、closest-rule 優先を守り、省略 source marker を残す。ToolResult、disk bytes、ToolStarted/ToolCompleted は変更しない。AGENTS.md semantic merge、skill loader、trust UI は実装しない。PR は `main` を target とする。

## Acceptance Criteria

- startup snapshot が root AGENTS.md + user always-apply のみを含み nested/path-scoped rules を含まない
- read/edit 後に対象 chain が root→deep 全件注入され、deep/closest 優先、兄弟非混入、root 上探索なしを fixture で証明する
- multi-path が stable union/dedup され同一 source を同一 turn に二重注入しない
- 4 scoped dirs の alwaysApply / matching glob / non-match / invalid glob が fixture test で証明される
- untrusted project は startup/post-tool とも project rules 非ロード、user scope のみ
- dynamic truncation が context budget、UTF-8、closest priority、省略 marker を満たす
- rules edit 後に cache stale が残らず、ToolResult/disk/event 契約が不変
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check` が pass する

## Verification

- startup root-only snapshot test
- nested root→deep / closest-wins / sibling isolation / project boundary integration test
- multi-path stable union / dedup test
- scoped rules metadata/glob fixture suite
- project trust fail-closed test
- dynamic truncation small/large budget + UTF-8 + omission marker test
- cache invalidation test
- read/edit ToolResult・disk bytes・ToolStarted/ToolCompleted regression test
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Related Links

- intents/evorch/features/agent-runtime-kernel/overview.md
- intents/evorch/features/tools-sandbox/overview.md
- intents/evorch/features/orchestration/overview.md
- intents/evorch/decisions/0005-headless-kernel-and-gui-separation.md
- intents/evorch/decisions/0008-threat-model-phased-adoption.md
- intents/evorch/interviews/grill-v02-loop-foundation.json（Q7）
- dependency: `v02-prompt-assembly`

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs writeback is expected for this slice. Answer or explicitly decline:

- Intent placement: `features/agent-runtime-kernel/overview.md` primary、`features/tools-sandbox/overview.md` supporting
- ADR candidate: none（grill Q7 と ADR 0008 で確定済み）
- Diagram candidate: none
- Docs update: none
- Closeout writeback expected: yes。resolver/insertion/glob/truncation/cache/trust boundary を両 overview に記録する

## Guide Reachability (G645)

この slice は全 runtime role の model-visible context に startup root rules と post-tool path-scoped rules を追加する。route は agent-runtime-kernel overview の project rules injection surface → runtime prompt assembly / AgentContext。`no_role_facing_surface: false`。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly. Worker branch convention is `evorch/task/<run-id>`.
