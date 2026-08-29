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

**v0.1 実 crate（2026-08-29 確定、ADR 0016）**: `runtime` / `event-bus` / `storage` / `providers` / `tools` / `sandbox` / `routing` / `model` / `config` / `gui` + バイナリ `evorch`。外部依存ゼロの骨格で、依存は各 slice の実装に応じて `[workspace.dependencies]` へ集約する。

以下は v0.1 完了後の再編で目指す目標構成（未配置 crate を含む）:

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
  gui/            egui_dock        (第一候補。ADR 0007)
  gui-floem-proto/ floem           (docking 評価用 prototype。必須ではない)
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

## Event Bus transport（2026-08-29 解決）

in-process tokio broadcast 固定（ADR 0017）。将来の分散化は gateway subscriber で serde_json bridge し、イベント型と購読 API は不変。

## Open questions

（現在なし）
