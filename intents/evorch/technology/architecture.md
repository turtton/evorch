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

現在の workspace member（2026-09-20 時点、ルート `Cargo.toml` の `members = ["crates/*"]` が権威）:

| crate | 責務 |
|---|---|
| `runtime` | agent / session / task / event の実行基盤。orchestration を内部モジュールとして保持 |
| `event-bus` | in-process tokio broadcast の event bus（ADR 0017） |
| `storage` | SQLite / events 永続化 |
| `providers` | anthropic / openai / openai-codex / github-copilot / openrouter / openai-compatible |
| `tools` | filesystem / shell / pty / git / search / code-intel |
| `sandbox` | policy / macos / linux / windows の実行隔離 |
| `routing` | profile / fallback / affinity / health |
| `model` | registry / capabilities |
| `config` | 設定読み込み・管理 |
| `agents` | role / category / skills の定義 |
| `arena` | role-eval 実験基盤（comparison / selection / promotion） |
| `catalog` | model catalog（ADR 0013） |
| `mock-openai` | OpenAI-compatible テスト用 provider |
| `workspace-ui` | panel / layout / action / semantic-tree |
| `gui` | egui + egui_dock（ADR 0007） |
| `evorch` | エントリポイント。lib + バイナリ `evorch` |

構想上存在したが独立 crate としては未配置のもの:

- `orchestration`（intent / coordinator / delegation / policy）: 現状 runtime 内モジュール（`crates/runtime/src/orchestration/`）として実装。独立 crate 化は将来判断
- `context`（prompt / cache / compaction / memory）: 現状 runtime および関連 crate に分散実装。独立 crate 化は将来判断
- `diagnostics`（fault-bus / crash-spool / issue-reporter）: 現状 event-bus / runtime に分散実装。独立 crate 化は将来判断
- `gui-floem-proto`: floem docking 評価用 prototype。現行 workspace member としては存在しない（必須ではない）

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
