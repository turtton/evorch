# v02-project-rules Implementation Packet

## Goal

trusted project の `AGENTS.md` と scoped rules を AgentRun context へ path-aware に注入する。起動時は project root（+ user scope）の rules のみを読み、read/edit 等の path-bearing tool 成功後に対象 file directory から root までの nested `AGENTS.md` を収集して root→deep 順に synthetic message として最後の user message 直前へ挿入する。`.omo/rules` / `.claude/rules` / `.cursor/rules` / `.github/instructions` は `alwaysApply` / `glob` で対象 path に絞り、残 context budget に応じた dynamic truncation を適用する。

## Why

v0.2 の agent が自律実装するには、repository 全体の規約だけでなく触れた subtree 固有の規約を、その file を扱う turn で確実に認識する必要がある。startup に全 nested rules を一括注入すると未使用 subtree の規約で context を浪費し、root のみでは closest-wins を満たせない。grill Q7 は omo の `findAgentsMdUp` / context-injector を参照し、startup root-only + tool 後合成注入、root→deep の複数注入、scoped rules、dynamic truncation を確定した。現行 ToolExecutor と AgentContext は存在するが、その間に tool target path から rules を解決して model-visible window に合成する seam はない。本 slice は v02-prompt-assembly に依存してその seam を成立させる。

## Scope

- project root と user scope を明示的な探索境界とする rule resolver を実装する。project trust 未承認時は project 側 `AGENTS.md` / rules directory を一切ロードしない
- startup assembly では root `AGENTS.md` と user scope の always-apply rules だけを注入する。未アクセス subtree の nested `AGENTS.md` と path-scoped glob rules を先読みしない
- read / edit を最低対象とし、grep 等の path-bearing tool も結果から対象 path を安全に抽出できる場合は同じ post-tool seam に載せる。shell の出力文字列から arbitrary path を推測して rules を注入しない
- tool 成功後、対象 path の directory から project root へ親方向探索し、見つけた `AGENTS.md` を root→deep 順で全件注入する。project root より上へ探索しない。兄弟 subtree の rules を混ぜない
- synthetic message は model-visible context の system notification 層として、最後の user message text の直前に挿入する。tool result 本文、disk bytes、event payload を書換えない
- closest-wins は root rules を先、deep rules を後に並べることで明示する。同じ source path/content は同一 turn で deduplicate し、複数 target path は正規化 path の安定順で chain を union する
- `.omo/rules/*.md`、`.claude/rules/*.md`、`.cursor/rules/*.md`、`.github/instructions/*.instructions.md` を project scope として発見する。frontmatter `alwaysApply: true` または glob metadata が target normalized relative path に一致した rules のみを適用する。user scope も同じ metadata semantics を使う
- glob は path separator と project-relative path を正規化し、invalid pattern は actionable error / diagnostic として fail-closed に無効化する。case sensitivity は platform/file-system policy とテストで固定する
- dynamic truncation は現在の model context window、既使用 token、応答 headroom から injection budget を算出する。UTF-8 boundary を守り、root→deep ordering と closest rule の保持を優先し、省略 source 一覧と再アクセスで再注入可能である旨を marker に残す
- normalized path + source mtime/content hash 等の bounded cache を許容するが、edit 後に stale rules を使わない invalidation test を持つ。cache は正しさの前提にしない

## Out of scope

- `AGENTS.md` 内容の lint、矛盾解消、自然言語 key 単位の merge。closest-wins は ordered injection で model に伝える
- source file や ToolResult の書換え、control marker sanitizer の変更
- shell output からの path 推測、未アクセス repository 全体の eager scan
- `.sisyphus/rules` 等、確定リスト外 directory の追加。互換 directory 拡張は後続で行う
- skill discovery / SKILL.md activation（`v02-skill-loader`）
- project trust の GUI / sandbox policy 本体。本 slice は ADR 0008 の trust 判定結果を消費する
- intent gate の `<intent>` 分類（`v02-prompt-assembly`）

## Verification

- startup snapshot: root AGENTS.md + user always-apply のみ、nested/path-scoped rules は未注入
- nested fixture integration: read/edit 後に root→deep 全件、closest/deep が後、兄弟非混入、project root 上を探索しない
- multi-path fixture: stable union + dedup、同一 source の同一 turn 二重注入なし
- scoped rules fixture: 4 directory、alwaysApply、matching/non-matching glob、invalid glob、normalized relative path
- trust test: untrusted project は startup/post-tool とも project rules 非ロード、user scope のみ
- truncation test: small/large budget、UTF-8、安全な marker、closest rule 優先、再アクセス時再注入
- cache invalidation test: AGENTS.md/rule edit 後は更新内容を注入
- regression: read/edit ToolResult・disk bytes・ToolStarted/ToolCompleted は不変、injection は AgentContext synthetic layer のみ
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: `features/agent-runtime-kernel/overview.md` primary。model-visible context の runtime assembly だから。`features/tools-sandbox/overview.md` は project trust と tool seam の supporting
- ADR candidate: decline — startup root-only / tool 後 closest-wins / scoped rules / truncation は grill Q7 で確定済み。ADR 0008 の trust-before-load を実装する slice
- Diagram candidate: decline — tool target path → resolver → ordered synthetic message の流れは overview の記述で十分
- Docs update: decline — user-facing editor integration は追加しない。role-facing injection surface は Guide Reachability で扱う
- Closeout learning: resolver boundary、insertion point、glob semantics、truncation、cache invalidation、trust の確定を両 overview に記録する。`write_back_required: true`

- Guide reachability (G645): 全 runtime role に startup root rules と post-tool path-scoped instructions が見える surface を agent-runtime-kernel overview から route する。`no_role_facing_surface: false`

`improve` (G456 / G460) is the later safety net; packet-time maintenance is the normal path.
