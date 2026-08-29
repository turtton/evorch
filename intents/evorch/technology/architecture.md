# Architecture（アーキテクチャ）

[product overview](../product/overview.md) / [mvp-roadmap](mvp-roadmap.md)

## 全体構造

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

Agent Kernel → UI Event Bus → Workspace Model → GUI Renderer の層構造。GUI framework は application architecture の中心にしない。

## Rust Workspace 構成案（crates/）

```text
crates/
  runtime/        agent, session, task, event
  orchestration/  intent, coordinator, delegation, policy
  agents/         role, category, skills
  context/        prompt, cache, compaction, memory
  model/          registry, capabilities
  providers/      anthropic, openai, openai-codex, github-copilot, openrouter, openai-compatible
  routing/        profile, fallback, affinity, health
  tools/          filesystem, shell, pty, git, search, code-intel
  sandbox/        policy, macos, linux, windows
  storage/        sqlite, events
  diagnostics/    fault-bus, crash-spool, issue-reporter
  workspace-ui/   panel, layout, action, semantic-tree
  gui/            floem
```

## 想定技術スタック

| 用途 | 選択 |
|---|---|
| Language | Rust |
| Async Runtime | Tokio |
| GUI | egui + egui_dock（ADR 0007。Floem は評価用 prototype に限定、GPUI + gpui-component は長期 watch） |
| HTTP | reqwest |
| Serialization | serde / serde_json |
| CLI | clap |
| Storage | SQLite |
| PTY | portable-pty |
| Code Intelligence | tree-sitter / LSP |
| MCP | rmcp |
| Logging | tracing |

## 主要データ構造（概念）

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

struct ProviderCapabilities {
    prompt_cache: PromptCacheCapabilities,
    reasoning: ReasoningCapabilities,
    tool_calling: ToolCapabilities,
    compaction: CompactionCapabilities,
    streaming: StreamingCapabilities,
    transport: TransportCapabilities,
}
```

## Open questions

- crate 分割の初期 granularity（v0.1 で全 crate 作るか、必要なものから作るか）
- Event Bus の transport 実装（in-process channel のみか、将来の分散を見越すか）
