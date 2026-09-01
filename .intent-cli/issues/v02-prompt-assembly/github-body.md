## Goal

config-driven role/category → logical-model / preset binding と deterministic system prompt assembly を実装する。既存 Router の `(profile, model_id)` candidate / fallback を利用し、system bundled preset + user override の2層、model-family base optimization、category overlay を合成する。既存 evorch Intent Gate（Direct / Coordinated）へ omo 型分類 block と available agents / skills 由来 dynamic `keyTriggers` を統合し、Role::Orchestrator の System message にのみ注入する。

## Why This Slice Exists Now

v0.2 の Orchestrator が goal を分類し適切な role / category / model を選ぶには、Agent の5軸（Role + Category + Skills + Execution Policy + Route Policy）を system prompt と routing に接続する基盤が必要である。現行 Config v2 は providers / routing 等のみで role / category / preset を持たず、runtime agent loop は initial prompt を User message として `AgentModel::complete` に渡すだけで system assembly がない。一方 Router は logical route → `RouteCandidateConfig { profile, model }` の宣言順 list を実装済みで、同一 model の別 provider を別候補にできる。grill `grill-v02-loop-foundation` Q3/Q8 は config binding、model-family optimization、preset/override 2層、fallback 軸区別、Orchestrator 限定 Intent Gate / dynamic keyTriggers を確定した。本 slice は後続 skill / project rules / compaction / orchestrator loop の合成基盤でもある。

## Current Observed State

- Config schema version 2（`crates/config/src/types/mod.rs`）の root field は providers / routing / panel / diagnostics / permissions / metrics。role / category / preset 設定はない
- Config と RoutingConfig は `deny_unknown_fields`。`RoutingConfig.routes` は logical name → `Vec<RouteCandidateConfig>`
- `RouteCandidateConfig` は `profile: String` + `model: Option<String>`。同一 model id の異なる provider profile を別 candidate として宣言できる
- `Router::resolve` は `ResolvedRoute { profile, model_id }` を返し、SessionAffinity で profile を pin。`next_fallback` で宣言順の次候補へ進む
- `AgentModel` trait は `complete(role, messages, tools)` / `selected_model(role)`。runtime は routing / prompt assembly を境界実装へ委譲する
- `agent_loop::run_agent` は `AgentContext::new` 後に initial prompt を User message として push し、そのまま `complete` へ渡す。System preset、category、model-family、Intent Gate は未実装
- providers `Message` は `Role::{System, User, Assistant}` + content を持つため provider-neutral System message の既存表現はある
- Role は Orchestrator / Explorer / Worker / Reviewer。Intent Gate はコードにない

## Accepted Baseline You May Assume

- orchestration overview: Intent Gate は task type / capabilities / mutation / scope / uncertainty / output / completion / delegation need を抽出し、Execution Shape を Direct / Coordinated に分類する。workflow は固定しない
- Agent Instance = Role + Category + Skills + Execution Policy + Route Policy。Role は capability boundary（ADR 0002）のまま
- grill Q3: config で role/category → logical model、model-family base optimization、prompt body は preset file、bundled + user override 2層、provider/model fallback 軸を区別
- grill Q8: omo 型 `<intent>` block + dynamic keyTriggers は Orchestrator のみ。Worker / Reviewer / Explorer には不要
- omo 4.19.4 prior art: model family に応じ base section を調整し category overlay を上に載せる。完全別 template ではない。keyTriggers は available agents / skills から動的生成
- evorch Router の `(profile, model)` pair candidate は omo の既知 fallback bug を避ける既存基盤として維持する

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/config/`, `crates/routing/`, `crates/runtime/`

Target part: role/category binding、preset/override、model-family optimization、Orchestrator-only Intent Gate、logical route / fallback と System message assembly の統合

## In Scope

- Config の role / category binding（logical model ref / preset ref / typed generation overrides）。prompt body field は拒否
- role/category merge / precedence の型付き規則。Role capability と Category cognitive mode を分離
- system bundled preset + user override の2層 resolver。validation / path traversal / size / UTF-8 / missing error
- model id → family classifier（Claude / OpenAI reasoning・GPT-5 / Gemini / Kimi / unknown）と family optimization section
- deterministic assembly order: role baseline → model-family → category overlay → Orchestrator Intent Gate → preset/user append
- provider-neutral 単一 System message の生成と既存 conversation history への挿入
- Orchestrator-only Intent Gate: current message の task fields + Direct / Coordinated、mutation permission 非持越し
- available agents / skills metadata 由来 dynamic keyTriggers の stable sort / dedup / empty handling
- role/category → logical model → existing Router / SessionAffinity / fallback の接続
- `(profile, model_id)` identity の維持。同一 model / 別 profile fallback の回帰固定
- provider-only / model-only / both-axis fallback の before / after identity を diagnostics / event で区別
- provider call 前の fail-closed error と prompt / credential 非ログ出力

## Out Of Scope

- SKILL.md discovery / validation / progressive disclosure — `v02-skill-loader`
- AGENTS.md / rules tool 後 injection — `v02-project-rules`
- compaction / DCP — `v02-context-compaction`
- goal / finish gate / continuation / PR loop — `v02-orchestrator-loop`
- provider OAuth / subscription credentials — provider packet
- Role variant / capability matrix の追加
- GUI prompt / preset editor
- provider ごとの完全別巨大 template、source hardcode の role→model 表、config への prompt body
- remote model benchmark / auto quality scoring

## Standalone Child Issue Contract

`turtton/evorch` の `crates/config/`・`crates/routing/`・`crates/runtime/` に、config-driven role/category → logical-model / preset binding と deterministic System prompt assembly を実装する。Config は logical model reference、preset reference、typed generation overrides を保持し、prompt 本文 field は `deny_unknown_fields` で拒否する。preset は system bundled + user override の2層で、同名 override 優先、package update は user file を変更しない。model id を Claude / OpenAI reasoning・GPT-5 / Gemini / Kimi / unknown family に分類し、role baseline → model-family optimization → category overlay → Orchestrator Intent Gate → preset/user append の安定順で単一 System message を生成する。Intent Gate は Orchestrator のみに入り、current user message から task type / capabilities / mutation / scope / uncertainty / output / completion / delegation need と Direct / Coordinated を分類し、available agents / skills から stable `keyTriggers` を生成する。role/category が返す logical model は既存 Router / SessionAffinity で `(profile, model_id)` route に解決し、同一 model の別 profile を別 fallback として保持する。provider-only / model-only / both-axis fallback を before / after identity で観測可能にする。missing preset / invalid config は provider call 前に fail-closed とし、prompt / credential / override 本文を logs に出さない。skill loading、AGENTS.md、compaction、goal loop、provider auth、GUI editorは実装しない。PR は `main` をターゲットにする。

## Acceptance Criteria

- Config が role/category → logical model / preset / typed overrides を parse し、prompt body field を拒否する unit test がある
- bundled preset / user override の2層 precedence と user file 非変更を tempfile test で検証する
- assembly order が deterministic で同一 input から byte-identical な単一 System messageを生成する golden test がある
- Claude / OpenAI reasoning・GPT-5 / Gemini / Kimi / unknown family classifier の table test と generic fallback がある
- role/category binding → Router が `(profile, model_id)` route を返し、同一 model / 別 profile を別 fallback として順に選ぶ regression test がある
- provider fallback / model fallback / both-axis fallback が diagnostics で区別される test がある
- Intent Gate は Orchestrator のみに含まれ、Worker / Reviewer / Explorer に含まれない role matrix test がある
- Intent Gate が current-message-only / Direct-Coordinated / mutation permission 非持越し / required fields を含む golden test がある
- dynamic keyTriggers が agents / skills metadata から stable sort / dedup され、空集合でも valid な test がある
- invalid config / missing preset は provider call 前に typed error、prompt / credential / override 本文は log 非露出
- Config v2 / Router / SessionAffinity / AgentModel の回帰 test が pass する
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check` が pass する

## Verification

- Config schema / serde / migration / unknown field tests
- preset resolver tempfile tests（precedence / update / traversal / size / UTF-8 / missing）
- model-family table tests
- deterministic assembly golden tests
- Orchestrator-only Intent Gate role matrix / current-message-only tests
- dynamic keyTriggers stable sort / dedup / empty tests
- Router same-model-different-profile fallback regression と fallback-axis diagnostics tests
- fail-before-provider / sensitive-log redaction tests
- existing config / routing / runtime regression suite
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Related Links

- intents/evorch/features/orchestration/overview.md（Agent 5軸 / Intent Gate / Direct-Coordinated）
- intents/evorch/features/agent-runtime-kernel/overview.md（AgentRun role/category/route）
- intents/evorch/interviews/grill-v02-loop-foundation.json（Q3 / Q8）
- intents/evorch/decisions/0001-no-fixed-workflow.md
- intents/evorch/decisions/0002-role-capability-boundaries.md
- 後続 slice: `v02-skill-loader`、`v02-project-rules`、`v02-context-compaction`、`v02-orchestrator-loop`

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs
writeback is expected for this slice. Answer or explicitly decline:

- Intent placement: `features/orchestration/overview.md` primary。supporting: agent-runtime-kernel overview、ADR 0001 / 0002、grill record。新規 intent は不要
- ADR candidate: none（既存 ADR と Q3/Q8 の具体化）
- Diagram candidate: none
- Docs update: none（internal assembly のみ）
- Closeout writeback expected: yes。schema / preset precedence / assembly order / family classifier / Intent Gate / keyTriggers / fallback diagnostics を overview に記録する

## Guide Reachability (G645)

While the author still knows the answer, name the guide surface and role that route to every
role-facing surface this slice adds, or explicitly say that no role-facing surface is added. A
blank answer is not treated as no-surface. The closeout record is a debt check, not a merge gate.

この slice は role-facing command / tool / workflow surface を追加せず、既存 role の internal System prompt construction を変更する。`no_role_facing_surface: true` を宣言する。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
