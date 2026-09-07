# Feature: GUI Workbench（ネイティブ GUI ワークベンチ）

[features 一覧](../) / [agent-runtime-kernel](../agent-runtime-kernel/overview.md) / [technology/architecture](../../technology/architecture.md)

## 概要

TUI に限定しない。目標は **IDE / Workbench 的な Native GUI** で、Qt の Dock Widget のように各機能を自由に配置できること。

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

Panel は left / right / bottom / tabs / floating / separate OS window に自由に配置できる。

## 要件

- **Subagent の可視化**: background agent を「裏で動いている何か」にしない。可能なら全 agent を表示し、難しくてもデフォルトで3つ程度を常時表示。各 Agent Panel で status / role / model / provider / reasoning / tool execution / transcript / cache / usage を確認できる
- **GUI framework と Workspace Model の分離**: GUI framework を application architecture の中心にしない。Agent Kernel → UI Event Bus → Workspace Model → GUI Renderer の層構造。Workspace Model / Layout（Split / Tabs / Panel / Floating / Window）は framework-independent data として保持し、Floem から egui への切り替えが可能にする
- **Semantic UI API**: Agent から GUI を pixel surface として扱わせず、semantic object graph として expose する（ui.inspect / ui.find / ui.open_panel / ui.close_panel / ui.move_panel / ui.focus / ui.set_layout / ui.save_workspace / ui.screenshot）。GUI 自体も agent から理解・改善可能にする
- **UI 自己改善（3段階）**: Level 1 runtime configuration（pane placement / filters / keybind 等の即時変更）、Level 2 UI composition（既存 primitive の組み合わせによる新 view: Cache Dashboard 等）、Level 3 framework implementation（Rust source 変更が必要なものは worktree → build → test instance → semantic inspection → screenshot / interaction replay で自己検証）
- **GUI framework 選定（ADR 0007、2026-08 再評価で確定）**: 第一候補は **egui + egui_dock**（`anhosh/egui_dock` 0.21.x。tab 移動/resize/undock/floating window、DockEvent による layout persistence を標準提供、2026 年も活発リリース）。Floem は汎用 dock API を提供せず安定版が v0.2.0（2024-11）のままであるため「docking 評価用 prototype」に限定。**GPUI + gpui-component**（Zed 実戦系、dock/nested split/floating/syntax highlighting 対応）は長期 watch
- **大容量 transcript の扱い**: 行単位 chunking + 差分更新 + 明示的 virtualization の自前 widget とし、framework 非依存設計にする（egui immediate mode の制約。Floem/GPUI への切り替え時も流用可能に）

## v0.2 GUI 再構成の確定（grill grill-v02-loop-foundation、2026-09-02）

t3code（pingdotgg/t3code、commit b883fc0 調査）を基準レイアウトとして採用する。egui_dock の自由配置機構は保持し、以下を**既定レイアウト**とする。

```text
┌──────────────┬──────────────────────────────┬────────────────────┐
│ 左サイドバー   │ 中央: 会話 (thread)          │ 右: tabbed surfaces │
│ project 管理  │                              │  Agents (主眼)      │
│ thread 管理   │                              │  Diff (最小)        │
└──────────────┴──────────────────────────────┴────────────────────┘
```

- **プロジェクト概念**: プロジェクトは「基準 repo/path + アクセス許可ディレクトリ集合」を持つ。subagent worktree の cwd はプロジェクトルートと一致しないため（cwd != プロジェクト）、プロジェクトごとにアクセス可能ディレクトリを設定し、sandbox 境界・project trust（ADR 0008 v0.2 項目）と一体化する。worktree（`evorch/task/<run-id>`）はプロジェクト許可ディレクトリ傘下として自動許可
- **thread 管理**: 複数 session の作成/切替/pin/状態表示を左サイドバーで提供（v0.2 スコープ）
- **Agents 可視化（主眼、t3code を超える水準）**: 右サイドバー Agents tab は一覧（identity/phase/model/provider/現在 tool/token・usage）のライブ更新 + 選択 agent の中央 pane drill-down + dock 機構による複数 agent transcript pane 同時ライブ。既定レイアウトは orchestrator + 直近 worker + reviewer の3分割程度。t3code の Agents tab は dashboard 止まりなので、ここは独自実装
- **diff tab（最小版のみ v0.2）**: working tree / branch の unified diff 表示（人間 merge 承認の判断材料）。file tree / turn 別 diff / whitespace 制御等の完全版は v0.3 以降
- **loop UI**: goal 投入（goal + packet/issue 参照 + 制約）と merge 承認 approval は本 feature が器を提供し、orchestrator-loop の機構が利用する

## v0.1.1 実 runtime wiring の実装確定（2026-08-30、PR #30 / issue #29）

製品 GUI entrypoint（`crates/gui/src/bin/evorch-gui.rs`）が `EmptyAgentSource` を廃止し、実 `AgentRuntime` と同一 `Arc<EventBus>` を EventPump と共有する構成で landed。

- **製品起動 lifecycle**: `evorch-gui` は起動時に `AgentRuntime::production(bus, policy, workspace_root, model)` を組み、失敗時（bwrap 未検出）は明確なエラーで exit 1（fail-closed）。`--demo` は外部 AI provider / credential 不要の決定的 scripted session（orchestrator が background worker を delegate）を起動し、各 row が Pending → Running → Done へ遷移する。手動確認手順は `--help` に同梱
- **AgentSummary identity 境界**: `AgentSummary { name（表示名）, role_name（role）, model（実行 model）, phase }`。`RunConfig.name` 未指定時は name は role 名へフォールバック。model は `AgentModel::selected_model(role)`（routing profile 層が報告、runtime は解決しない）をそのまま記録。`list_agents` serialization と GUI tasks pane はこの identity を直接写像する（TaskRow への固定 label・role→name 複製は廃止）
- **自動 / 手動検証の分担**: headless wiring test（`gui/tests/runtime_wiring.rs`: 実 runtime + EventPump + WorkbenchState で 2 rows 収束）は CI で常時実行。文字内容レイアウト（name/role/model の実表示）は headless screenshot 基盤が必要で別 unit。`--demo` の手順による目視は手動
- **network capability 伝播**: production runtime は `runtime::network::build_sandbox(policy)` 経由で role の network mode を bwrap へ伝播する（PR #20 seam。`AgentRuntime::production` 内では `ToolExecutor::with_standard_tools` に構築済み sandbox を注入。`with_production_sandbox` は BwrapConfig 直接受取りの sibling entry として残存）


## v0.2 GUI workbench 3領域再構成の実装確定（issue #65、PR #66、2026-09-05）

- workspace schema v2 + `migrate.rs` による v1 自動移行。PanelKind に Sidebar / Agents / AgentTranscript（target=run_id 必須）/ Diff / Goal / Merge を追加し、load は fail-closed（不正 panel 参照・duplicate・invalid trust path を拒否）
- project/thread/trust モデルは workspace-ui 所有（config 不変）。allowed-directory は mutation/load で同一 validator を通し、runtime 所有 worktree（`<root>/.evorch/worktrees/`）は membership 自動許可、任意外部 path は明示 trust なしに許可しない
- TranscriptRegistry が run-addressed event（tool は run_id、approval は call_id index、AgentMessage は sender/recipient）を決定配送。MessageDelta は run_id を持たないため thread + 単一 Running run のみ mirror（follow-up 候補: event-bus の MessageDelta/ReasoningDelta へ run_id 付与）
- Telemetry は ProviderEvent の run_id Some のみ集約、UsageEvent は推測せず無視
- Diff は working tree / main 固定 branch のみ（turn 別・split・whitespace・file tree・base 任意選択は v0.3 scope 据置）、256KiB cap、空/Error は明示状態
- goal-submission / merge-approval は WorkbenchCommand + FixtureLoopAdapter で型付き・決定的（orchestrator-loop 未接続でも fixture で操作可能）
- `--demo` は 3 役 delegate + AgentMessage + telemetry + diff/goal/merge を外部 provider なしで再現、`--state` で sidebar 永続化、手動確認手順は `--help` に同梱（NixOS は LD_LIBRARY_PATH+WGPU_BACKEND=gl+llvmpipe が必要）
- HeadlessWorkbench 統合テスト（chained scenario + v1 migration + error paths）を含む。GUI 95 / workspace-ui 38 test green、Reviewer Gate 3 round で APPROVED（AC5 非混線・AC10 migration・AC11 scope 外項を最終修正）
## v0.3 デザインシステム（t3code 準拠ダークテーマ）の実装確定（issue #81、PR #82、2026-09-06）

- 単一テーマモジュール crates/gui/src/theme/（tokens/style/text/widgets/dock）。t3code b883fc0 のトークンを egui/egui_dock に適合: canvas #0a0a0a / sidebar #000 / surface #111-#191 / accent #346bf1 / 4px grid / radius 6-10 / body 14px
- pane_root（Role::Pane）ランドマークでタブ/見出し重複を解消しつつ kittest の has_label 契約を維持。ラベル一意性ガードテスト追加
- タブアテンション: tab_style_override + U+2022 "•"（U+25CF は egui 同梱フォント欠如で tofu 化）
- 空状態+CTA: No projects yet / No project selected / No thread selected / No messages yet + Go to Projects / Start a thread / Go to Goal
- headless 検証基盤: --demo fixture / --activate <panel> / --pointer X Y（hover capture）。crates/gui/docs/screenshots/v03 に before/after 6 枚+再現手順
- 既知の制約: kittest click はノード rect 中心の模擬ポインタでクリップ外はクリック不能 → MIN_SIDEBAR_FRACTION=0.30 で最小幅保証。agents グリッドは horizontal scroll で列到達性確保（列幅自動フィットは follow-up 候補）

## v0.3 demo_loop flake 根治の実装確定（issue #83/#77、PR #84、2026-09-06）

- orchestration イベント発行は spawn 前（reserve+emit→spawn）が規約 — runtime に reserve_run_id/spawn_reserved API を追加し dispatch_continuation を emit-before-spawn 化（ContinuationDispatched が FinishAccepted に逆転する race を解消）
- worker RunAttached の発行元は delegate tool 経路（registry）に単一化（registry/supervisor 二重発行 race 解消）
- DemoScriptModel は構築時 subscribe + inbox replay 化（demo root 最終ターン待機の取り逃し解消）、create_goal は同期 insert
- テストの gate 解放は notify_one（permit 保持）を使う。末尾 sleep polling は collector mpsc done 通知の recv_timeout 待機へ置換
- 検証: taskset -c 0,1 負荷下 30 連続 PASS ×2 セット（変更前 11/30 FAIL）

## v0.3 GUI デザイン修正の実装確定（issue #87、PR #88、2026-09-06）

- 状態ドット規約: 直径 DOT_SIZE=10px token（半径は DOT_SIZE/2.0 で導出）、配置は行内テキスト左・垂直中央（compact_row の left_to_right(Align::Center) 固定高レイアウトに委譲）。4px grid の半値例外（body 14px の cap height 光学値）
- sidebar 行サイジング規約: project/thread 行は ROW_DENSE=28px token 固定高・非折返し単一行。ROW_COMPACT=36px はエディタ系（agent.rs composer）専用として残存。行内末尾要素は with_layout(right_to_left(Align::Center)) で右端配置、タイトルは残り幅に truncate+halign(Align::LEFT)
- ハードコード禁止の実例: status_dot の 8.0/4.0、projects.rs の 40.0、threads.rs の THREAD_ROW_RIGHT_WIDTH=140.0 を撤去し token 化
- 検証手順: geometry は headless テストで assert（theme_headless::compact_row_is_dense_and_centers_status_dot / sidebar_headless::sidebar_rows_are_single_line_dense_rows）。before/after 画像は headless_capture --demo [--error-thread] で取得し crates/gui/docs/screenshots/<unit>/ に README 付きで commit。赤ドット検証には --error-thread（demo fixture に Error 状態がないため追加）
- 落とし穴: horizontal_wrapped + add_sized(固定高 Label) は行を下方向にのみ拡張し小要素が上寄せになる。固定高行には allocate_ui_with_layout + Align::Center を使う

## v0.3 GUI chat composer の実装確定（issue #91、PR #92、2026-09-06）

- composer surface: Conversation ペイン footer（通常入力=chat、先頭 `/`+既知名=command dispatch）。入力モデル parse_input（Empty/Chat/Command/UnknownCommand）、補完は `/` 直後の無空白間のみ、`/` 単独で全件。コマンド registry は SLASH_COMMANDS（metadata 追加 + run_slash_command 分岐で拡張、senpi 踏襲）
- typing モデル: user message は TranscriptEntry::UserMessage、案内/エラー/help は Notice。MessageDelta の連結を分断しないよう別 entry として積む
- chat 経路: SendChat→chat_runs（thread→RunId map）→既存 keep-alive interactive run へ send_message、終端時は再 spawn。chat run は goal ledger 非登録。RunConfig.keep_alive で resume 後も待機継続
- provider 未設定ガード: ProviderStatus を composition root input として fail-closed（真の検出配線は provider-settings slice）
- 終端整合: send_message は終端 phase 記録後に必ず拒否。terminal 公開前に user inbox close で受理→喪失 race を封鎖

## v0.3 chat_sink_runtime flake 根治の実装確定（issue #98、PR #99、2026-09-07）

- 位相遷移の可観測規約: 状態確定は必ず通知に happens-before させる（commit-before-emit）。AgentRunStateChanged を観測した直後に phase 参照すると旧位相を読みうる emit-before-commit の順序を、agent_loop.rs LoopState::transition / runtime.rs transition_phase の両遷移経路で phase_tx.send_replace → emit に反転（PR #84 由来の決定論化規約に準拠）
- spawn の registered emit（Pending→Pending）は watch 初期値と一致するため対象外。テスト側はイベント待機 + 直後の inspect が安全
- run 状態復帰（send_message）と chat_runs キャッシュは、Waiting 到達を保証する wait_for_reply 後には決定的。resume 保証は phase 確定（:140 Waiting）に依存
- 検証: workspace tests 193 ok、修正対象テストを逐次 30/30 + taskset -c 0,1 6 並行 30 runs 30/30 の 2 条件で連続 PASS（変更前は 6 並行で 1 FAIL / x12 を記録、窓は CPU 競合でのみ露出）

## v0.3 Settings surface の実装確定（issue #93、PR #94、2026-09-06）

- 配置=egui::Modal の中央 overlay（dock/layout 不変、PanelKind 拡張なし）。導線=composer 案内行「Open Settings」（chat は NotConfigured でブロック継続）と Goal pane「Configure provider」（goal は非ブロッキング案内のみ）
- 編集項目=openai-compatible の base_url/api_key_env/models/default_model（sugar 形式）。保存先=project 層 evorch.toml、config::save_openai_compatible_provider が toml_edit で additive 書き戻し（他テーブル/コメント保持、schema v2 維持、version!=2 拒否、migrate/strict/Config 逆直列化の fail-closed 事前検証、atomic tmp+rename）
- credential ガード=api_key_env に大文字環境変数名 ^[A-Z_][A-Z0-9_]*$ のみ受理（平文拒否・ADR 0008）
- 検出=evorch-gui 非 demo 起動時 Config::load(project_dir) の providers 非空判定（真の配線、load 失敗は NotConfigured fail-closed）
- 検証: crates/gui/tests/provider_settings_headless.rs 9 件 + headless_capture --demo --open-settings（lavapipe）

## v0.4 Provider Settings UX 拡張の実装確定（issue #100、PR #101、2026-09-07）

- modal 幅: min(viewport×0.6, PROVIDER_MODAL_MAX_WIDTH=720px) clamp + 新 theme token。caption は combo 行から分離（同列配置だと modal が ~1180px に膨張し cap 突破）。検証: headless 幾何学テスト（input >400px・modal 右端 ≤720）+ PNG capture 800/1600px
- /v1/models 補完: providers::list_models(base_url, &ProviderAuth) が GET {base_url}/models を Bearer で呼ぶ。GUI は std::thread + tokio current_thread + mpsc で非同期取得（UI スレッド非ブロック）、ModelsFetchState {Idle/Loading/Loaded/Failed} を表示。成功時は fetched ids を default_model combo 候補に採用（現在値は選択可能を維持）、失敗時は models_text/default_model 手動入力 fallback + Refresh models 再取得
- 除外モデルフィルタ: ProviderProfileConfig.excluded_models を #[serde(default)] で additive 追加（旧 evorch.toml 読込可、deny_unknown_fields 維持、strict.rs PROVIDER_KEYS 追加）。正規化（trim/空除去/重複排除・初出順）後、非空のときのみ TOML 書き出し（空配列ノイズなし）。GUI multiline フィールドで編集・保存（round-trip 検証）
- default_model 見直し: 維持。router.rs resolve() の (1) session affinity 再解決時の concrete model (2) route candidate の model 未指定時 (3) fallback パスの anchor として必須（logical_model は論理名）。UI に用途を明示: "Used when a route doesn't override the model and when re-resolving a pinned session."
- mock-openai: spawn_with_models で /v1/models endpoint（OpenAI 互換 list 形式、scripts キュー非消費）を追加し headless/通常テスト両方で利用
- 検証: cargo test --workspace 1834/0（clippy -D warnings / fmt --check / git diff --check 全 PASS）。fetch headless テストは kittest step() による 1 フレーム確定進行（Loading 中 spinner の継続 repaint で run() が max_steps(4) を超過する CI flake を request-update で修正、commit 64f9c52）

## 受け入れ基準

- egui + egui_dock で基本 pane（agent / terminal / tasks 等）の dock / undock / floating ができること（landed）
- 製品 GUI が実 AgentRuntime から tasks pane を live 表示できること（landed、PR #30。`--demo` で外部 provider 不要の確認経路あり）
- Workspace Model が framework 非依存データとして保持され、GUI なしに layout を検証できること
- semantic UI API 経由で agent が panel を操作できること
- 仮想化 transcript widget が1万行規模の transcriptで操作が追従すること
- **offscreen レンダリングによるヘッドレス起動**が可能であること（自己改善の test instance / capture_ui 用。ADR 0009）

## Related decisions

- [ADR 0005: Headless Agent Kernel と GUI の分離](../../decisions/0005-headless-kernel-and-gui-separation.md)
- [ADR 0006: Harness 自身の診断と自己改善](../../decisions/0006-self-improvement-and-diagnostics.md)
- [ADR 0007: GUI 第一候補を egui + egui_dock に](../../decisions/0007-gui-framework-egui-first.md)

## Open questions

- Floem 評価用 prototype の実施タイミング（v0.2 並行で十分か）
- 大量 transcript の描画性能要件の具体値（目標フレームレート・行数の定量値）
