---
facets: [vocabulary]
---

# Mission

> Ask intent-cli for guidance before editing:
> `intent-cli guide intent-work setup --kind tree-layout --domain evorch --format markdown`

## Mission statement

evorch は、複数の専門 Agent が並行して活動し、その状態・判断・cache・provider・tool execution を人間が常時観測できる、自己改善可能な **AI-native Agent Harness / Agent Workbench** を提供する。workflow を固定せず、Agent の責任・認知モード・権限・実行ポリシーを固定し、Orchestrator が依頼に応じて動的に topology を構築する実行環境を目指す。

## Vision

一般的な Coding Agent（LLM + Tools + Chat UI）より一段上の **Agent Operating Environment** を完成形とする。

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

最終的に harness 自体が Agent の能力を引き出すための「実行環境」として振る舞う。モデル、Provider、Agent Role、UI はそれぞれ交換可能であり、特定ベンダーや特定 orchestration に依存しない。

## Values / principles

1. **workflow は固定しない**  
   Explore → Plan → Execute → Review → Fix のような固定状態機械を中核にしない。ユーザーの依頼（調査・実装・バグ解析・レビュー・設計相談・ドキュメント作成など）に応じて、Orchestrator が動的に topology を構築する。

2. **capability discipline over prompt discipline**  
   「あなたは調査だけしてください」という prompt だけでは弱い。Explorer / Librarian / Reviewer などは runtime レベルで read/search/write/edit/delegate の権限を分離し、role contamination を防ぐ。Role は personality ではなく capability boundary。

3. **cognitive isolation**  
   生成と独立レビューを別 context / 別 role に分離する（Planner → Reviewer、Worker → Reviewer）。1 つの LLM に調べる・設計する・実装する・レビューする・修正するを全部やらせると、自分が一度選んだ案を正当化する方向へ寄りやすい。

4. **cache-first context engine**  
   Prompt cache hit rate は billing metric ではなく runtime health metric として設計する。Stable Prefix は invariant として扱い、cache metrics で runtime health を観測する。

5. **vendor independence**  
   Provider Type / Provider Profile / Logical Model / API Protocol を分離し、同一 provider の複数 account、model-aware fallback、session affinity を実現する。特定ベンダーや特定 orchestration に依存しない。

6. **headless kernel first**  
   GUI は Headless Agent Kernel の上に乗るだけ。Agent Runtime と UI は分離し、UI Event Bus・Workspace Model・GUI Renderer の層で接続する。

7. **observability by design**  
   background agent を「裏で動いている何か」にしない。agent / cache / provider / runtime を常に可視化し、DiagnosticBus ですべての component から runtime fault を収集する。

8. **self-improvement through dogfooding**  
   Harness 自身を dogfooding することで、introspection API を通じて UI を含めた自己改善 loop を作る。

## Glossary

| 用語 | 定義 |
|---|---|
| **Agent Operating Environment** | `evorch` が目指す最終形。Zed/IDE 的 Native Workspace + omo 的 Dynamic Orchestration + pi 的 Cache Efficiency + Codex 的 Sandbox + Senpi 的 Cache-aware Runtime / Memory + Provider Router + Self Diagnostics / Self Improvement を統合したもの。 |
| **Intent Gate** | ユーザーの依頼を受け、task type / required capabilities / mutation allowed? / scope / uncertainty / expected output / completion criteria / likely need for delegation を抽出する。workflow を決めず、粗い `ExecutionShape`（Direct / Coordinated）だけ決める。 |
| **Execution Shape** | Intent Gate の出力。`Direct`（単純な質問や局所的修正）または `Coordinated`（複雑な調査・実装・並列探索が必要）のいずれか。 |
| **Agent Instance** | `Role + Category + Skills + Execution Policy + Route Policy` の5軸で構成される Agent 定義。 |
| **AgentRun** | 1 回の Agent 実行単位。`id`, `role`, `category`, `skills`, `route`, `context`, `policy` を持つ独立した Tokio task として動作する。 |
| **Role** | Agent の責任・認知モード・権限・output contract。例: orchestrator, explorer, librarian, oracle, planner, reviewer, worker, multimodal。 |
| **Category** | 仕事の性質。例: quick, deep, high-reasoning, visual, writing, research。誰がやるかではなく、どんな負荷・思考特性でやるか。 |
| **Skills** | Agent に追加される専門知識や tool / prompt。例: rust, kotlin, frontend, git, database, aws, linux。 |
| **Execution Policy** | Agent の実行制約。sandbox policy / filesystem permissions / network permissions / background allowed / concurrency / cost budget / timeout など。 |
| **Route Policy** | どの logical model / provider profile を利用するか。例: orchestrator は claude-class を優先、worker は gpt-class を優先。 |
| **Role Contamination** | 1 つの LLM に調べる・設計する・実装する・レビューする・修正するをすべてやらせること。自分が一度選んだ案を正当化する方向へ寄りやすい。 |
| **Cognitive Isolation** | 生成と独立レビューを別 context / 別 role に分離すること。Planner → Reviewer、Worker → Reviewer など。 |
| **Capability Discipline** | prompt discipline ではなく、runtime レベルで tool 権限を制限し、Orchestrator 自身が強い mutation tool を使えないようにすること。 |
| **Provider Type** | プロバイダーの種別。例: anthropic, anthropic-subscription, openai, openai-codex, github-copilot, openrouter, openai-compatible。 |
| **Provider Profile** | Provider Type 上に作られる credential instance。例: claude-personal, claude-business, copilot-personal, copilot-work。 |
| **Logical Model** | モデル選択の上位概念。例: gpt-main, claude-main。Route Policy で指定され、複数の provider profile へ解決される。 |
| **API Protocol** | Provider とは独立した API プロトコル層。例: anthropic-messages, openai-responses, openai-completions, openai-codex-responses, google-generative-ai, copilot-compatible。 |
| **Provider Capability** | Provider ごとの差異を明示する構造。prompt_cache / reasoning / tool_calling / compaction / streaming / transport。 |
| **Session Affinity** | Prompt cache のため、同一 task / session では provider affinity を強く持つ。429 / 5xx / timeout / quota / auth に応じて cooldown 管理する。 |
| **Stable Prefix** | 毎 turn 再生成しない固定前置部分。system prompt / role definition / tool schema / project instruction snapshot / skill snapshot / memory snapshot。対義語は Append-only Context。 |
| **Append-only Context** | user / assistant / tool / assistant ... の履歴部分。Stable Prefix の下に追加されていく。 |
| **Cache-aware Wait** | 長時間 command 実行中に prompt cache TTL が切れないよう、cache lease の有効期限を監視しつつ wait する runtime primitive。 |
| **Cache Regression** | 各 request で記録する expected cacheable tokens / actual cache read tokens / cache hit ratio が急落した場合に DiagnosticBus に流す診断。 |
| **Compaction** | Agent が `compact_context` として呼べる control-flow primitive。長時間 session や複数 task の連続実行後に context を圧縮し、checkpoint を更新する。 |
| **Provider-specific Compaction** | Provider ごとの compaction 実装。OpenAI / GPT 系では公式 Responses API の compaction を優先し、その他 provider は model-aware summarization を利用する。 |
| **Task Boundary** | Session より下の境界。Session 内に Task A / Task B / Task C を持ち、各 task 間で compact できる。 |
| **Persistent Memory** | Task / Session 終了時に Quick Agent が「将来も有用な知識」を抽出して保存するもの。次の Task boundary から Stable Prefix へ snapshot として含まれる。 |
| **Background Agent** | Main Agent が `delegate_background` / `send_message` / `wait` / `cancel` できる独立した AgentRun。GUI 上でも可視化する。 |
| **Semantic UI API** | Agent から GUI を pixel surface として扱わせず、semantic object graph として expose する API。`ui.inspect`, `ui.find`, `ui.open_panel`, `ui.set_layout` など。 |
| **Workspace Model** | GUI framework に依存しない data として保持される workspace 状態。Panel / LayoutNode の enum として表現される。 |
| **DiagnosticBus** | Harness 内部の全 component が診断を送信する bus。ProviderProtocolViolation, CacheRegression, ToolCrash, SandboxViolation, AgentDeadlock, UiError, CompactionFailure, SessionCorruption, UnexpectedModelSwitch などを扱う。 |
| **Crash Spool** | panic などで session-end hook が実行できない場合に、診断情報を `~/.harness/crash-spool/` 等へ durable に保存する仕組み。 |
| **DelegationValue** | delegation の具体的価値。Expertise / Parallelism / ContextIsolation / IndependentReview / DifferentInformationSource / Scale。 |
| **Self-improvement Loop** | 不便を検出 → 改善案作成 → workspace config 変更または source 変更 → test instance → 検証 → screenshot / interaction replay の loop。 |
