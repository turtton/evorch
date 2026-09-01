# v02-prompt-assembly Implementation Packet

## Goal

role / category から logical model と prompt preset を config で解決し、既存 Router の `(profile, model_id)` route と接続する deterministic system prompt assembly を実装する。assembly は system bundled preset + user override の2層、model id から選ぶ model-family base optimization、category overlay を持つ。既存 orchestration Intent Gate（Direct / Coordinated）へ omo 型の `<intent>` 分類 block と available agents / skills 由来 dynamic `keyTriggers` を統合し、Role::Orchestrator の system prompt にのみ注入する。prompt 本文は config に埋め込まず preset file に分離し、provider fallback と model fallback の identity を区別したまま既存 Router の候補 list を利用する。

## Why

v0.2 の Orchestrator が goal を正しく分類し、適切な Worker / Reviewer / model を選ぶには、Role・Category・Route Policy と system prompt を一貫して組み立てる層が必要である。現行 Config v2 は providers / routing 等のみで role / category / preset binding を持たず、`agent_loop` は initial prompt を User message として追加して `AgentModel::complete(role, messages, tools)` へそのまま渡すため system assembly が存在しない。一方、Router は logical route → 宣言順の `RouteCandidateConfig { profile, model }` を既に実装し、同一 model の別 provider を別 candidate として扱える。この既存長所を保ったまま、grill `grill-v02-loop-foundation` Q3 の config binding・model-family optimization・preset/override 2層・fallback 軸区別と、Q8 の Orchestrator 限定 Intent Gate / dynamic keyTriggers を実装する必要がある。これは後続 skill loader、project rules、compaction、orchestrator loop が prompt に安全に合成される基盤でもある。

## Scope

- Config schema に role / category binding を追加する。各 binding は logical model reference、preset reference、必要な generation overrides（temperature / top_p / max tokens 等、既存 provider 能力に合わせた型）を保持できる。prompt 本文そのものを config field として受け付けない
- role binding と category binding の precedence / merge rule を型付きで定義する。Role は capability boundary のまま、Category は認知モード / workload hint として logical model / prompt overlay を選ぶ。role 名や model 名の hardcoded 一対一表を runtime に持たせない
- preset file resolver を実装する。system bundled presets は binary/package version と共に更新可能、user override は別 root に保持して同名 preset を置換 / append できる2層とする。config は preset reference name のみを持つ
- preset name の validation、path traversal 防止、UTF-8 / size limit、missing / duplicate / invalid file の型付き error を実装する。user override 本文や assembled prompt を通常 diagnostics に出さない
- resolved model id から model family を分類する層を実装する。最低限 Anthropic Claude、OpenAI reasoning / GPT-5、Gemini、Kimi、unknown generic を table-driven に扱い、family ごとの reasoning / tool discipline / formatting optimization section を選ぶ。完全別 template ではなく base section の調整とする
- deterministic assembly order を確定する: role baseline → model-family optimization → category overlay → Orchestrator Intent Gate → preset/user append。重複 section / nondeterministic map iteration を避け、単一 provider-neutral `providers::Message { role: System, ... }` を作る
- orchestration overview の Intent Gate を Orchestrator prompt に統合する。current user message のみを分類対象とし、task type / required capabilities / mutation allowed / scope / uncertainty / expected output / completion criteria / delegation need を抽出し、Execution Shape を Direct / Coordinated に分類する指示を含める
- omo 型の key trigger table を available agents / skills metadata から assembly 時に動的生成する。安定 sort / dedup を行い、agent / skill がない場合も有効な block を生成する。skill 本文の遅延ロード自体は後続 `v02-skill-loader`
- Intent Gate は `Role::Orchestrator` のみに注入し、Worker / Reviewer / Explorer には注入しない。role-fixed agent に余分な再分類をさせない
- role/category → logical model 解決後は既存 Router / SessionAffinity / next_fallback を利用し、候補 identity を `(profile, model_id)` で保持する。同一 model id の別 profile を別候補として辿れることを回帰 test で固定する
- provider fallback（profile 変更）と model fallback（model id 変更）を診断上区別する。両方変わる候補も profile / model の before / after を保持し、omo の provider-list-inside-model candidate bug を再現しない
- `AgentModel` / runtime 呼出し境界へ assembled System message を渡し、既存 user / assistant / tool history の順序を壊さない

## Out of scope

- agentskills discovery / validation / progressive disclosure / SKILL.md 本文ロード — `v02-skill-loader`
- AGENTS.md / scoped rules の tool 後 synthetic injection — `v02-project-rules`
- context compaction / 75% threshold / DCP — `v02-context-compaction`
- goal persistence、finish gate、continuation dispatcher、PR / review loop — `v02-orchestrator-loop`
- provider client / OAuth / subscription credential 実装 — `v02-provider-codex-subscription` 等
- Role variant（Librarian / Oracle 等）そのものの追加や capability matrix 変更。本 slice は任意 Role / Category metadata を解決できる assembly 基盤
- GUI prompt editor / preset manager。user override は file/config discovery surface のみ
- prompt を provider ごとに完全分岐した巨大 template、runtime source への model name hardcode、config への prompt 本文埋込み
- model catalog の remote refresh / benchmarking / automatic quality scoring

## Verification

- Config unit test: role / category binding の parse / serde / deny_unknown_fields、config 内 prompt body field 拒否、schema migration / default 回帰
- tempfile test: bundled preset / user override precedence、override 不在時 fallback、bundled update 後も user file byte 不変、path traversal / missing / oversized / invalid UTF-8 fail-closed
- model-family table test: Claude / OpenAI reasoning・GPT-5 / Gemini / Kimi / unknown の分類と generic fallback
- golden test: deterministic assembly order、単一 System message、同一 input の byte-identical output、既存 user history 順序維持
- role matrix test: Intent Gate は Orchestrator のみ。Worker / Reviewer / Explorer には非注入
- Intent Gate golden test: current-message-only、Direct / Coordinated、mutation permission 非持越し、required fields と dynamic keyTriggers
- keyTriggers unit test: available agents / skills から stable sort / dedup、空集合、特殊文字の安全な rendering
- Router regression: 同一 model id + 異なる profile の候補を別 fallback として順に選択。provider-only / model-only / both-axis fallback diagnostics
- fail-closed test: missing preset / invalid config は provider call 前に error、prompt / credential / override 本文を logs に出さない
- 既存 Router / SessionAffinity / AgentModel / config v2 test suite の回帰
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: `features/orchestration/overview.md` primary（Agent 5軸 + Intent Gate）。`features/agent-runtime-kernel/overview.md` の AgentRun role/category/route seam を supporting とする。新規 intent は不要
- ADR candidate: decline — no-fixed-workflow / capability boundary は ADR 0001 / 0002 で既決。Q3/Q8 は orchestration feature の具体化であり新しい cross-cutting decision ではない
- Diagram candidate: decline — assembly pipeline は feature overview の順序記述で十分。preset roots が3層以上へ増える等の新 topology が出た場合のみ follow-up
- Docs update: decline — GUI / user-facing preset management は追加しない。internal assembly のため no role-facing surface
- Closeout learning: config schema、preset precedence、assembly order、family classifier、Intent Gate contract、keyTriggers source、fallback identity / diagnostics を overview に記録する。`write_back_required: true`

- Guide reachability (G645): role-facing command / tool / workflow surface は追加せず、既存 role の internal system prompt construction を変更するため `no_role_facing_surface: true`

`improve` (G456 / G460) is the later safety net; packet-time maintenance is the normal path.
