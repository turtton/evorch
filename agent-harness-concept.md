# AI-Native Agent Harness 構想

> **注記（2026-08-29）**: 本ドキュメントは構想段階の起源記録（why）である。
> 現行の正確な意思決定・技術選定は `intents/evorch/` の intent tree、
> `decisions/` の ADR（0001–0007）、`technology/re-evaluation-2026-08.md` に従う。
> 本書と tree / ADR が矛盾する場合は **tree / ADR が優先**。
> 例: GUI 第一候補は Floem ではなく egui + egui_dock（ADR 0007）、
> サブスクリプションプロバイダは実装可能として v0.3 維持（re-evaluation §1）。

## 1. 概要

本構想は、OpenCode + oh-my-opencode（omo）で得られる高品質なマルチエージェント体験をベースにしつつ、以下の問題を根本から解消するための **ネイティブ AI Agent Harness / Agent Workbench** を新規に設計するものである。

目指すものは単なる「OpenCode の代替」ではない。

最終的には、以下を一級機能として統合した **AI-native development environment** を目指す。

- omo のような高品質な動的 orchestration
- pi のような高い prompt cache hit rate
- 複数 provider / 複数 account / fallback / affinity
- background agent
- agent ごとの独立 context
- agent 判断による compaction
- provider 固有 compaction
- sandbox
- memory
- agent / runtime / cache / provider の高い可観測性
- GUI 上で複数 agent を同時に表示
- harness 自身の不具合検出・Issue 化・自己改善

中核となる考え方は次の通り。

> **workflow は固定しない。  
> Agent の責任・認知モード・権限・実行ポリシーを固定し、Orchestrator が依頼に応じて動的に topology を構築する。**

---

# 2. 設計上の基本方針

## 2.1 固定 workflow を採用しない

以下のような固定状態機械は中核にしない。

```text
Explore
  ↓
Plan
  ↓
Execute
  ↓
Review
  ↓
Fix
```

理由は、ユーザーの依頼が必ずしも実装ではないからである。

たとえば以下は必要な動きがすべて異なる。

- 調査
- 実装
- バグ解析
- コードレビュー
- 設計相談
- ドキュメント作成
- リポジトリ探索
- リファクタリング
- 比較検証
- 単純な質問

したがって、Harness が固定 workflow を強制するのではなく、

```text
User Request
    ↓
Intent Gate
    ↓
Execution Shape
    ↓
Orchestrator / Direct Agent
    ↓
Dynamic Agent Topology
```

という形を採用する。

---

## 2.2 Intent Gate は workflow を決めない

Intent Gate の責務は「これは実装だから Explore → Plan → Execute」と決めることではない。

Intent Gate が抽出するのは、たとえば以下。

```text
task type
required capabilities
mutation allowed?
scope
uncertainty
expected output
completion criteria
likely need for delegation
```

その結果として、

```rust
enum ExecutionShape {
    Direct,
    Coordinated,
}
```

程度の粗い実行形式だけ決める。

単純な質問や局所的修正なら Direct。

複雑な調査・実装・並列探索が必要なら Coordinated。

---

# 3. omo の Agent 設計から取り込むべき本質

omo の品質が高い理由は、単に「Agent の数が多い」ことではない。

重要なのは、Agent ごとに以下を分離していること。

- 責任
- 認知モード
- 情報源
- 権限
- delegation 可否
- 出力契約
- model family との相性

たとえば概念的には以下。

| Role | 主な責務 |
|---|---|
| Orchestrator | 全体調整と完遂 |
| Explorer | ローカルコード探索 |
| Librarian | 外部ドキュメント・OSS 調査 |
| Oracle | 独立した高 reasoning consultant |
| Planner | 計画作成 |
| Metis 的 Role | 計画前の欠落・曖昧性検出 |
| Momus 的 Role | 完成した計画・方針の独立レビュー |
| Worker | 実作業 |
| Reviewer | 実装・成果物の独立検証 |
| Multimodal | 画像・PDF 等の異なるモダリティ処理 |

---

## 3.1 Role contamination を避ける

1つの LLM に、

```text
調べる
設計する
実装する
レビューする
修正する
```

をすべてやらせると、自分が一度選んだ案を正当化する方向へ寄りやすい。

そのため、

```text
生成
  ↓
独立レビュー
```

を別 context / 別 role にする。

たとえば、

```text
Planner
   ↓
Reviewer
```

や、

```text
Worker
   ↓
Reviewer
```

を分離する。

これは単なる specialization ではなく、**cognitive isolation** である。

---

## 3.2 Role は personality ではなく capability boundary

「あなたは調査だけしてください」という prompt だけでは弱い。

たとえば Explorer / Librarian / Reviewer は runtime レベルで、

```text
read      allowed
search    allowed
network   role-dependent
write     denied
edit      denied
delegate  denied
```

にする。

Orchestrator も同様に、強い mutation tool を与えない。

これにより、「Orchestrator が何でも自分でやる」問題を prompt discipline ではなく capability discipline で抑制する。

---

# 4. Agent を多次元に分解する

Agent を1つの巨大な定義にしない。

以下の5軸に分離する。

```text
Agent Instance
  =
    Role
  + Category
  + Skills
  + Execution Policy
  + Route Policy
```

---

## 4.1 Role

責任・認知モード・権限・output contract。

例:

```text
orchestrator
explorer
librarian
oracle
planner
reviewer
worker
multimodal
```

---

## 4.2 Category

仕事の性質。

例:

```text
quick
deep
high-reasoning
visual
writing
research
```

Category は「誰がやるか」ではなく、「どんな負荷・思考特性でやるか」。

---

## 4.3 Skills

専門知識や追加 tool / prompt。

例:

```text
rust
kotlin
frontend
git
database
aws
linux
```

---

## 4.4 Execution Policy

実行制約。

例:

```text
sandbox policy
filesystem permissions
network permissions
background allowed
concurrency
cost budget
timeout
```

---

## 4.5 Route Policy

どの logical model / provider profile を利用するか。

例:

```text
orchestrator:
  preferred logical model = claude-class
  fallback = kimi-class

worker:
  preferred logical model = gpt-class
```

---

# 5. Orchestrator の責務

Orchestrator は「何でもできる main agent」ではなく、

> **何を、誰に、どの順序で、どの程度まで任せるべきかを判断する agent**

とする。

原則として以下を持つ。

```text
delegate
delegate_background
send_message
wait
cancel
list_agents
inspect_agent
read
grep
git_diff
compact
finish
```

原則として以下は持たせない。

```text
write
edit
apply_patch
arbitrary shell
git commit
```

実作業は Worker に任せる。

---

## 5.1 delegation の乱用を防ぐ

「複雑だから delegate」ではなく、delegation に具体的価値を要求する。

概念的には以下。

```rust
enum DelegationValue {
    Expertise,
    Parallelism,
    ContextIsolation,
    IndependentReview,
    DifferentInformationSource,
    Scale,
}
```

Orchestrator が subagent を起動する場合、

```text
なぜこの delegation が必要か
```

を内部的に説明可能であることを求める。

例:

```text
Explorer
  → local repository evidence

Librarian
  → official docs / upstream evidence
```

これは情報源が独立するため、並列化の価値がある。

一方、同じ context で同じことを2 agent に投げるだけなら原則不要。

---

# 6. Agent Runtime

Harness の中心は **Headless Agent Kernel** とする。

GUI はその上に乗るだけで、Agent Runtime と UI は独立させる。

```text
Agent Kernel
├ Runtime
├ Orchestration
├ Context
├ Provider Routing
├ Tools
├ Sandbox
├ Storage
├ Diagnostics
└ Event Bus
```

各 AgentRun は独立 context を持つ。

```rust
struct AgentRun {
    id: AgentId,
    role: Role,
    category: Category,
    skills: Vec<Skill>,
    route: RoutePolicy,
    context: AgentContext,
    policy: ExecutionPolicy,
}
```

Agent は Tokio task として動作させる。

---

# 7. Event-driven architecture

Runtime 内部は event-driven にする。

概念的には以下。

```rust
enum AgentEvent {
    Started,
    MessageDelta,
    ReasoningDelta,
    ToolStarted,
    ToolCompleted,
    Delegated,
    BackgroundTaskStarted,
    BackgroundTaskCompleted,
    Usage,
    CacheStats,
    ProviderFallback,
    Completed,
    Failed,
}
```

GUI はこの Event Stream を購読する。

これにより UI と runtime が密結合しない。

---

# 8. Provider architecture

Provider は Agent Runtime と完全に分離する。

Agent SDK を中心に据えるのではなく、**pi のように Claude / OpenAI / ChatGPT Codex / GitHub Copilot 等を普通の Model Provider として扱う**。

---

## 8.1 Provider Type と Provider Profile を分離

Provider type と credential instance を同一視しない。

```text
Provider Type
  anthropic
  anthropic-subscription
  openai
  openai-codex
  github-copilot
  openrouter
  openai-compatible
```

その上に複数の Profile を作れるようにする。

```toml
[providers.claude-personal]
type = "anthropic-subscription"

[providers.claude-business]
type = "anthropic-subscription"

[providers.copilot-personal]
type = "github-copilot"

[providers.copilot-work]
type = "github-copilot"

[providers.openai-direct]
type = "openai"

[providers.crof]
type = "openai-compatible"
base_url = "..."
```

---

## 8.2 Model と Provider を分離

Logical Model を上位概念にする。

```text
logical model
  ↓
route
  ↓
provider profile
  ↓
API protocol
```

例:

```text
gpt-main
  ↓
1. openai-direct
2. copilot-work
3. openrouter
```

```text
claude-main
  ↓
1. claude-business
2. claude-personal
3. openrouter
```

---

## 8.3 API Protocol も分離

pi と同様に、

```text
Provider
≠
API Protocol
```

とする。

例:

```text
anthropic-messages
openai-responses
openai-completions
openai-codex-responses
google-generative-ai
copilot-compatible
```

ProviderProfile が利用する protocol を選択する。

---

## 8.4 Provider Capability

Provider ごとの差異を capability で明示する。

```rust
struct ProviderCapabilities {
    prompt_cache: PromptCacheCapabilities,
    reasoning: ReasoningCapabilities,
    tool_calling: ToolCapabilities,
    compaction: CompactionCapabilities,
    streaming: StreamingCapabilities,
    transport: TransportCapabilities,
}
```

---

# 9. Provider fallback

複数 provider で同じ model を利用する場合、自動 fallback を行う。

単純 round-robin ではなく、

```text
current provider profile
   ↓ fail
same model / another profile
   ↓ fail
alternative logical model
```

の順。

---

## 9.1 Session affinity

Prompt cache のため、同一 task / session では provider affinity を強く持つ。

```text
anthropic-business で開始
      ↓
可能な限りその profile に留まる
```

429 / 5xx / timeout / quota / auth などに応じて cooldown 管理する。

Provider が Retry-After を返す場合は優先する。

---

# 10. Cache-first Context Engine

Prompt cache hit rate は後付け optimization ではなく、Runtime correctness の一部として設計する。

pi のような高い cache hit rate を狙う。

---

## 10.1 Stable Prefix

Prompt を大きく2つに分ける。

```text
Stable Prefix
────────────────
system prompt
role definition
tool schema
project instruction snapshot
skill snapshot
memory snapshot

Append-only Context
────────────────
user
assistant
tool
assistant
...
```

以下を毎 turn 勝手に再生成しない。

```text
AGENTS.md
skills
memory
environment
tool schema
```

Task 開始時に snapshot 化し、stable prefix に固定する。

必要なら明示的に、

```text
refresh_context
```

を呼び cache invalidation する。

---

## 10.2 Cache metrics

各 request で、

```text
expected cacheable tokens
actual cache read tokens
cache hit ratio
```

を記録する。

例:

```text
expected: 184,220
cache read: 183,104
hit rate: 99.4%
```

急落した場合、

```text
CacheRegression
```

として DiagnosticBus に流す。

Cache は billing metric ではなく runtime health metric として扱う。

---

# 11. Cache-aware wait

長時間 command が走っている間に prompt cache TTL が切れないようにする。

```text
start command
   ↓
JobHandle
   ↓
wait
   ↓
cache lease nearing expiry?
   ├ no → wait
   └ yes → agent turn に戻る
```

Tool call 自体を cache TTL より長く block させない。

Senpi の cache-aware wait の考え方を runtime primitive にする。

---

# 12. Compaction

Compaction は Agent が自分で判断して呼べる tool とする。

```text
compact_context
```

用途:

- 長時間 session
- 複数 task の連続実行
- 調査フェーズ終了後の tool result 圧縮
- 古い detail の整理

ただし通常 tool ではなく control-flow primitive とする。

```text
Agent
 ↓ compact_context
Runtime
 ↓ compaction
Context checkpoint 更新
 ↓
Agent resume
```

---

## 12.1 Provider-specific compaction

```rust
trait Compactor {
    async fn compact(&self, context: Context) -> Result<CompactedContext>;
}
```

OpenAI / GPT 系では、利用可能なら公式 Responses API の compaction を優先する。

その他 provider は model-aware summarization 等を利用する。

---

# 13. Session と Task

Session より下に Task という境界を持つ。

```text
Session
 ├ Task A
 ├ Task B
 └ Task C
```

例:

```text
Task A: 調査
  ↓ compact
Task B: 実装
  ↓ compact
Task C: テスト改善
```

1 conversation = 1 task に固定しない。

長寿命 workspace として使えるようにする。

---

# 14. Memory

Task / Session 終了時に Quick Agent を起動し、

```text
今回の作業から、将来も有用な知識は何か
```

を抽出する。

Persistent Memory へ保存する。

ただし cache を壊さないため、セッション途中で Stable Prefix に挿入しない。

```text
Persistent Memory
   ↓
Task Start
   ↓
Relevant Memory Retrieval
   ↓
Memory Snapshot
   ↓
Stable Prefix
```

新しい memory は次 Task boundary から利用する。

---

# 15. Background Agent

omo の backgroundtask 的機能を一級機能にする。

```text
Main Agent
├ Explorer #1       running
├ Librarian #2      running
├ Worker #3         running
└ Oracle #4         completed
```

各 AgentRun は独立した Tokio task。

Main Agent は必要に応じて、

```text
delegate_background
send_message
wait
cancel
```

できる。

---

# 16. Tool system

Tool は統一 interface にする。

```rust
trait Tool {
    fn name(&self) -> &str;
    fn schema(&self) -> JsonSchema;
    fn permissions(&self) -> ToolPermissions;

    async fn execute(
        &self,
        context: ToolContext,
        input: Value,
    ) -> Result<ToolOutput>;
}
```

初期実装候補:

```text
read
write
edit
grep
glob
bash
git
diagnostics
definition
references
compact_context
delegate
delegate_background
```

Role ごとに capability を制限する。

---

# 17. Shell / PTY

通常 command と interactive process を分離。

```text
ShellTool
├ exec
└ pty
```

通常 command:

```text
cargo test
git diff
rg foo
```

は `tokio::process::Command`。

interactive:

```text
ssh
REPL
interactive installer
```

は PTY。

Rust では `portable-pty` 等を利用候補とする。

---

# 18. Code Intelligence

LSP と Tree-sitter は独立機能として扱う。

```text
Code Intelligence
├ filesystem index
├ Tree-sitter
└ LSP
```

Tree-sitter:

```text
syntax-aware search
symbol extraction
AST navigation
```

LSP:

```text
diagnostics
definition
references
hover
rename
```

---

# 19. Sandbox

Codex のように、agent ごとの能力に応じた sandbox policy を持たせる。

例:

```text
Explorer
  workspace: read-only
  network: optional

Librarian
  workspace: read-only
  network: allowed

Worker
  workspace: read-write
  outside workspace: denied
  network: denied by default

Orchestrator
  mutation tools: unavailable
```

候補:

```text
macOS
  Seatbelt

Linux
  Landlock
  seccomp
  namespaces / bwrap 等

Windows
  restricted token / job object 等
```

---

# 20. GUI

TUI に限定しない。

目標は **IDE / Workbench 的な Native GUI**。

理想は Qt の Dock Widget のように、各機能を自由に配置できること。

```text
┌──────────────────────────────────────────────────┐
│ Tasks │ Main Agent               │ Explorer #1  │
│       │                          ├───────────────┤
│       │                          │ Librarian #2 │
│       │                          ├───────────────┤
│       │                          │ Worker #3    │
├───────┴──────────────────────────┴───────────────┤
│ Terminal │ Diff │ Diagnostics │ Cache │ Provider│
└──────────────────────────────────────────────────┘
```

Panel は、

```text
left
right
bottom
tabs
floating
separate OS window
```

へ自由に配置できる。

---

# 21. Subagent の可視化

Background Agent を「裏で動いている何か」にしない。

可能なら実行中の Agent をすべて表示。

画面サイズ的に難しい場合でも、デフォルトで3つ程度を常時表示する。

各 Agent Panel では、

```text
status
role
model
provider
reasoning
tool execution
transcript
cache
usage
```

等を確認できるようにする。

---

# 22. GUI framework

Pure Rust を優先する。

現時点の候補:

## 第一候補: Floem

理由:

- Pure Rust
- Lapce での実戦投入
- IDE / editor UI との親和性
- GPU rendering
- reactive architecture
- 大量テキストの扱い

## 第二候補: egui + egui_dock

理由:

- docking
- floating
- multi-window
- 開発速度
- agent によるコード変更のしやすさ
- debug / inspection UI に強い

初期段階で Floem の docking prototype を作り、

```text
mouse UX
dock / undock
multi-window
large transcripts
```

を評価する。

難しい場合、UI Model を保ったまま egui へ切り替えられるようにする。

---

# 23. GUI Framework と Workspace Model を分離

GUI framework を application architecture の中心にしない。

```text
Agent Kernel
    ↓
UI Event Bus
    ↓
Workspace Model
    ↓
GUI Renderer
```

Workspace Model の例:

```rust
enum Panel {
    Agent(AgentId),
    Terminal(TerminalId),
    Diagnostics,
    CacheInspector,
    ProviderInspector,
    Tasks,
    Memory,
    Diff(DiffId),
}
```

Layout:

```rust
enum LayoutNode {
    Split,
    Tabs,
    Panel,
    Floating,
    Window,
}
```

Framework-independent data として保持する。

---

# 24. Semantic UI API

Agent から GUI を pixel surface として扱わせない。

Semantic object graph として expose する。

例:

```text
ui.inspect
ui.find
ui.open_panel
ui.close_panel
ui.move_panel
ui.focus
ui.set_layout
ui.save_workspace
ui.screenshot
```

これにより GUI 自体も agent から理解・改善可能になる。

---

# 25. UI 自己改善

UI 改善を3段階に分ける。

## Level 1: runtime configuration

即時変更可能。

```text
pane placement
visible panels
filters
keybind
workspace layout
```

## Level 2: UI composition

既存 primitive を組み合わせて新 view を作る。

例:

```text
Cache Dashboard
Provider Health Panel
Agent Overview
Cost Inspector
```

## Level 3: framework implementation

Rust source の変更が必要。

例:

```text
dock algorithm
mouse interaction
new widget
rendering behavior
```

この場合は、

```text
worktree
  ↓
source modification
  ↓
build
  ↓
test harness instance
  ↓
semantic inspection
  ↓
screenshot / interaction replay
```

で自己検証する。

---

# 26. Diagnostics

Harness 内部の不具合を runtime が直接捕捉する。

```rust
enum Diagnostic {
    ProviderProtocolViolation,
    CacheRegression,
    ToolCrash,
    SandboxViolation,
    AgentDeadlock,
    UiError,
    CompactionFailure,
    SessionCorruption,
    UnexpectedModelSwitch,
}
```

全 component が `DiagnosticBus` に送信する。

---

# 27. Session 終了時の自動 Issue 化

Session / Task 終了時に Quick Diagnostic Agent を起動。

```text
Diagnostic bundle
   ↓
Quick diagnostic agent
   ↓
classification
   ├ project problem
   ├ transient provider issue
   └ probable harness bug
```

Harness bug と判断された場合、

```text
version
OS
provider
model
event timeline
stacktrace
cache transition
tool call
sanitized reproduction
```

等をまとめて GitHub Issue 化する。

panic 等で session-end hook が実行できない場合は、

```text
~/.harness/crash-spool/
```

等へ durable に保存し、次回起動時に処理する。

---

# 28. Self-improvement

Harness を dogfooding することで、自分自身を改善可能にする。

Agent が利用できる introspection API の例:

```text
harness.inspect_session
harness.inspect_agents
harness.inspect_cache
harness.inspect_provider
harness.inspect_ui
harness.spawn_test_instance
harness.capture_ui
harness.replay_interaction
harness.report_bug
```

これにより、

```text
不便を検出
  ↓
改善案作成
  ↓
workspace config変更
または
source変更
  ↓
test instance
  ↓
検証
```

という自己改善 loop を作れる。

---

# 29. Storage

SQLite を中心とした event-sourced runtime を想定。

主な entity:

```text
sessions
tasks
agent_runs
messages
tool_calls
events
usage
diagnostics
artifacts
memory
provider_health
```

Event Log を source of truth とし、

```text
event log
  ↓
state projection
  ↓
GUI
```

とする。

これにより、

```text
resume
branch
rewind
timeline
debugging
usage analysis
```

がしやすくなる。

---

# 30. Rust Workspace 構成案

```text
crates/
  runtime/
    agent
    session
    task
    event

  orchestration/
    intent
    coordinator
    delegation
    policy

  agents/
    role
    category
    skills

  context/
    prompt
    cache
    compaction
    memory

  model/
    registry
    capabilities

  providers/
    anthropic
    openai
    openai-codex
    github-copilot
    openrouter
    openai-compatible

  routing/
    profile
    fallback
    affinity
    health

  tools/
    filesystem
    shell
    pty
    git
    search
    code-intel

  sandbox/
    policy
    macos
    linux
    windows

  storage/
    sqlite
    events

  diagnostics/
    fault-bus
    crash-spool
    issue-reporter

  workspace-ui/
    panel
    layout
    action
    semantic-tree

  gui/
    floem
```

---

# 31. 想定技術スタック

```text
Language
  Rust

Async Runtime
  Tokio

GUI
  Floem
  (fallback: egui + egui_dock)

HTTP
  reqwest

Serialization
  serde
  serde_json

CLI
  clap

Storage
  SQLite

PTY
  portable-pty

Code Intelligence
  tree-sitter
  LSP

MCP
  rmcp

Logging
  tracing
```

---

# 32. MVP

最初から全機能を作らない。

## v0.1

```text
Rust
Tokio
SQLite
Floem prototype

Native Agent Runtime

Provider:
  OpenAI
  Anthropic
  OpenAI-compatible

Agents:
  Orchestrator
  Explorer
  Worker
  Reviewer

Tools:
  read
  edit
  grep
  shell
  git diff

Features:
  independent agent contexts
  background agent
  provider profile
  simple fallback
  event stream
  session persistence
  basic GUI panes
```

---

## v0.2

```text
Librarian
Oracle
Role / Category separation
Tree-sitter
LSP
sandbox
provider affinity
cache metrics
```

---

## v0.3

```text
OpenAI Codex subscription provider
GitHub Copilot provider
Claude subscription provider
cache-aware wait
agent-triggered compaction
OpenAI official compaction
```

---

## v0.4

```text
Memory
Task boundary
multi-task sessions
advanced routing
provider health / cooldown
```

---

## v0.5

```text
Diagnostics
automatic Issue creation
self-improvement agent
semantic UI introspection
test harness instance
```

---

# 33. このプロジェクトの本質

一般的な Coding Agent は、

```text
LLM
+
Tools
+
Chat UI
```

である。

本構想はそれより一段上の、

> **Agent Operating Environment**

を目指す。

主要な差別化ポイントは以下。

1. **Role-based multi-agent system**
   - omo の品質の根源
   - cognition / responsibility / permissions を分離

2. **Dynamic orchestration**
   - workflow を固定しない
   - intent に応じて topology を構築

3. **Cache-first context engine**
   - pi 級の cache hit rate を目標
   - Stable Prefix を invariant として扱う

4. **Provider routing layer**
   - provider type と profile を分離
   - 同一 provider の複数 account
   - model-aware fallback
   - session affinity

5. **Observable native workspace**
   - background agent を隠さない
   - agent / cache / provider / runtime を可視化

6. **Self-observing / self-improving harness**
   - runtime fault を自動収集
   - Issue 化
   - memory
   - UI を含めた自己改善

---

# 34. 最終イメージ

最終的な姿は「OpenCode + omo の再実装」というより、

```text
Zed / IDE 的 Native Workspace
          +
omo 的 Dynamic Orchestration
          +
pi 的 Cache Efficiency
          +
Codex 的 Sandbox
          +
Senpi 的 Cache-aware Runtime / Memory
          +
Provider Router
          +
Self Diagnostics / Self Improvement
```

を統合したものになる。

つまり、

> **複数の専門 Agent が並行して活動し、その状態・判断・cache・provider・tool execution を人間が常時観測できる、自己改善可能な Native Agent Workbench**

が完成形となる。

モデル、Provider、Agent Role、UI はそれぞれ交換可能であり、特定ベンダーや特定 orchestration に依存しない。

Harness 自体が Agent の能力を引き出すための「実行環境」として振る舞うことを最終目標とする。
