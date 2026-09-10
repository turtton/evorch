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

## v0.4 codex subscription ログイン導線の実装確定（issue #102、PR #103、2026-09-08）- codex subscription 認証導線: Settings modal 内 Codex subscription セクション。状態遷移は未認証 guidance（device URL + 手順）→ 認証中（user code + verification URL）→ 認証済み（expiry 表示）→ 失敗時固定文言 4 種
- トークン GUI 非接触規約: GUI 型は `CodexAuthSummary { expires_at_unix }` のみ。TokenBundle は provider adapter（`ProviderCodexAuthBackend`）内に限定、Debug も state 名のみ。`classify_provider_error` で provider/network エラー文字列（HTTP body 等）を GUI に流さず CodexAuthError（Network/StoreUnavailable/Rejected/Unavailable）に写像
- credential store 再利用: `routing::CredentialStoreTokenStore` 経由で既存 `CodexTokenStore` 契約を共有。credential key = openai-codex profile keyring account（default "codex"）、実体は `sandbox::open_default(<user-config-dir>/credentials)`
- adapter 構成: `DeviceAuthClient::with_default_http` + `CredentialStoreTokenStore`（`login_and_store` 合成）。GUI は CodexAuthModel（mpsc + named thread `evorch-codex-login` + per-frame poll）経由の CodexAuthBackend trait 越しで状態のみ受領
- 副作用の信頼性: `FileCredentialStore::set` を persist-first 化（保存失敗後の誤 Authenticated を排除、failed_set_preserves_memory_and_disk テスト）
- conductor 到達性: composer に Configured 状態用の Settings compact ボタン追加
- 検証: workspace 1866/0、clippy/fmt/diff-check clean。headless capture は kittest `Harness::step()` polling（継続 repaint 状態で run() による max_steps 超過 flake を回避）で 3 状態 PNG 証跡を CI lavapipe headless-capture ジョブで exact 名実行して必須化（codex-auth-authenticating/authenticated 等）

## v0.5 GUI UX 修正パックの実装確定（2026-09-09、本セッションで main 直接マージ）

ユーザーの具体的な UX 不満 9 件への修正。5 workstream（W-A プロジェクト追加 / W-B Settings タブ化 / W-C Codex 認証 / W-D composer / W-EF Goal・Merge 撤去）に分解し、git worktree 並列実装のうえ main に逐次統合。

- **プロジェクト追加**: `~`/`~/path` 展開（`~user` は型付きエラー）。`Browse…` ボタンで xdg-desktop-portal フォルダ選択（ashpd、std::thread + mpsc + frame poll で UI ノンブロッキング）。プロジェクト行の下に muted な `repo_root` パスを表示（hover でフルパス）
- **Allowed directories**: 非空時のみ表示、`CollapsingHeader` で既定折りたたみ＋件数表示。パス行はクリックでコピー
- **Settings modal**: 上部セグメント切替（OpenAI-compatible | Codex subscription）で同時表示を解消。「Refresh models」を base URL / credential 直下、Models 一覧の直上に再配置。Codex タブでは Save を無効化
- **API key 保存**: `sandbox::CredentialStore`（keychain feature 有効化、OS keyring 優先・不可時は 0600 ファイルフォールバック）に保存。TOML は `credential = { type = "keyring", ... }` のみで秘密値を含めない（strict.rs 平文拒否を維持）。Keyring 既定・Env 切替ラジオ。follow-up: `routing/factory.rs:170`・`routing/compose.rs:189` の keyring コンシューム接続（GUI runtime は現状 ModelSource::Fixed で未配線）
- **Codex 認証**: device-code → browser PKCE + localhost callback（127.0.0.1:1455/1457）へ全面置換。egui `open_url` + 「Open the sign-in page again」ハイパーリンク。device flow は providers に残置し交換 URI を `https://auth.openai.com/deviceauth/callback` に修正（旧 `codex/device` は stale）。GUI は 3 状態 PNG 証跡テスト（unauthenticated/authenticating/authenticated）
- **Chat composer**: 会話ペイン下端に常時ドック（t3code 準拠、空状態での中央寄せを解消）。`TopBottomPanel::bottom` + `CentralPanel` 分割、empty state は残領域を占有。複数行、Enter=送信 / Shift+Enter=改行、IME変換中の Enter 抑止（Preedit 監視）。高さ 48-180px、角丸等は theme tokens（COMPOSER_MIN_HEIGHT/MAX_HEIGHT/R_2XL）に集約、ペインへ裸ピクセル値なし
- **Goal / Merge pane 撤去**: `PanelKind::{Goal, MergeApproval}` を production から削除し、workspace schema を v3 に（`migrate_v2_to_v3` が nested split 折りたたみ・active clamp・floating/extra windows まで再帰的に prune）。`/goal` composer コマンドは保持（`submit_goal` 直接経路）。Merge 承認 runtime port（`ShellDeliveryAdapter` gh）と `decide_merge` 状態メソッドは保持し、GUI からは `MergeStateUpdated` の notice として surface。承認 UI 本来の居場所は Diff view へ一本化する方針をユーザーと確認済み（follow-up）
- **CJK フォント**: `theme/fonts.rs` で fontconfig（`sans:lang=ja`）経由のシステム Noto Sans CJK / Takao を実行時解決し proportional/monospace 両 family へ prepend。egui 既定 Hack/Ubuntu は CJK 非対応で tofu になっていたのを解消（同梱は避け、バイナリサイズ増なし）
- 検証: workspace 全テスト green / clippy（-D warnings）・fmt clean。visual-qa dual-oracle は実装 PASS 相当、初期キャプチャの staleness で REVISE→現行 HEAD から全枚再生成で解消。証跡 PNG は `/tmp/opencode/w-*.png` + `codex-auth-*.png`

## v0.6 実運用修正パックの実装確定（2026-09-09、main 直接マージ）

第1波 UX 修正(上記)の実稼働検証で判明した 4 件を 5 workstream に分けて再修正。

- **S-FOCUS composer フォーカス喪失**: `TextEdit` が auto-id を採用していたため、`/` 入力で補完 row が現れると auto-id がずれて focus が消失。`id_salt("composer-input")`/`id_salt("composer-scroll")` で固定。単体テストで `/g` 段階的な文字追加中にも `memory.focused().is_some()` が永続することを RED→GREEN で検証。
- **W-WIRE live model 接続**: これまで GUI 本番は `ModelSource::Fixed(DemoScriptModel)` で、保存された provider / credential が runtime に接続されていなかった。`routing/src/factory.rs` で OpenAI-compatible の Keyring 拒否を除去、`routing/src/compose.rs:169-190` `resolve_auth` で Keyring → `store.get(account)` 経由で解決、missing/empty 時は `RoutingError::MissingKeyringCredential`。`runtime/compose.rs` に `SwitchableModel`(`RwLock<Arc<dyn AgentModel>>` delegate、`replace` で入れ替え可能)と `compose_routed_model`(`RoutedModel` だけ合成)を追加。GUI 本番は `Config` load → `FileCredentialStore` → `SwitchableModel` seeded (fallback `UnconfiguredModel` は「no provider configured—open Settings」)、保存成功で hot recompose。
- **W-ERR エラー可視化**: `compose.rs:228-244` で `ProviderError` を profile/model 付きで保持(secret は `provider.auth` 値で置換)。`agent_loop` で run 失敗を tracing::error。`model/transcript.rs` に `TranscriptEntry::Error`、transcript registry で Lifecycle に run_id があれば Thread+Run 双方へ delivery。pane 側で danger color 描画。GUI binary 冒頭で tracing-subscriber を RUST_LOG EnvFilter で初期化(dev-dep → runtime dep 昇格)。
- **W-PROFILES multi-profile 管理**: `config` に `save_codex_provider`・`delete_provider` を追加。GUI Settings modal を profile 一覧+Add/Edit/Delete に置き換え(segmented 切替は廃止)。Codex profile は profile ごとに専用エディタ、`credential account` は既定で `codex/<profile-name>`。
- **W-ROUTE-SEAM runtime ModelPreference**: `RunConfig.model_preference`(watch 経由 mid-run 更新可)・`AgentInvocationContext.model_preference` 追加。`RoutedModel::complete` で `Some(pref)` 時は binding/Router スキップして profile を強制指定、model は pref.model or profile.default。選択中 profile/model は `SwitchableModel::available_profiles` 経由で GUI カタログに surface。
- **W-PICKER スレッド単位モデルピッカー**: workspace-ui `ThreadRecord.model_preference` (`#[serde(default)]`)。GUI モデルピッカーを composer 直上に配置(id_salt 固定で auto-id 問題を再発させない)。ChatSubmission で経路を伝搬、new run には RunConfig、existing run には `set_model_preference(run_id, pref)` を送信。未選択時の「Select model」表示と disabled + Settings 導線。
- **keyring v4 移行(nix 環境)**: `keyring = { version = "3", default-features = false, features = ["sync-secret-service", "vendored"] }` が nix 環境(dbus-1 pkg-config なし)で libdbus-sys ビルド不能、かつ gnome-keyring で NoEntry を返していたため、`keyring = { version = "4", features = ["v1"] }`(zbus-secret-service バックエンド、libdbus 不要)に移行。実機 debug ログで `creating entry with service evorch, user Crof`→`get password` 完走を確認。秘密値は一切ログ出力していない。ユーザー資産 `evorch.toml`(provider 接続設定)は認証情報パターンのため `.gitignore` に追加し git 管理外。

検証: workspace 全テスト green / clippy(-D warnings)・fmt clean。PNG 証跡あり。`headless_run_completes_with_single_mock_response` の flake は本変更不要の pre-existing 問題(MapEnv 使用で keyring 非依存)で、単体では常に成功。known follow-up として記録するのみ。

## v0.7 実運用パッチの実装確定（2026-09-09、main 直接マージ）

v0.6 の実稼働検証で判明した 5 件を 5 workstream (A〜E) に分解して再修正。CLIProxyAPI WebUI (router-for-me/Cli-Proxy-API-Management-Center) のモデル管理パターンを UX 参考に採用。

- **A モデルリスト管理の overhaul (A1〜A5)**: Config `ProviderProfileConfig.models` を `Vec<ModelEntryConfig>`(`id`+`enabled`)に昇格 (config b3f8edc)。`visit_str`/`visit_map` で TOML 後方互換 (旧形式 `models = ["a","b"]` は enabled=true に正規化)。JsonSchema transform で schema artifact は anyOf を明示。`strict.rs` で MODEL_ENTRY_KEYS の未知キー/key 型を `providers.<n>.models[i]` パスで reject (3e6e6ee)。`save.rs` は enabled→文字列 / disabled→inline table で書き戻し、enabled 0 個・default disabled を拒否 (3e6e6ee)。`routing/profile.rs` (5536642) で enabled フィルタ・default ∈ enabled を検証。GUI editor (10e49d3) で add/remove/enable/rename/default 自動再選択 (enabled 0 なら validation state)。pane UI (42aabe9) で CLIProxyAPI パターン: Configured リスト行 (enable チェック + Edit/Done + Remove) + Add 入力 + Fetch 後選択パネル (Already added / Apply selected (n))。
- **B chat で provider error 表示 (ef96cec)**: 既存の Lifecycle 経路のままでは request failure が chat 側に可視化されない問題を、transcript_registry/transcript で `ProviderEvent::RequestFailed` と `FaultEvent` を transcript entry 化。run_id を持つ RequestFailed を Thread+Run 両方へ route、run_id なしを Thread、SkillDiagnostic は Thread Error、SubscriberLagged は Notice。retry 可な failure_kind (RateLimited/Server 5xx/Transport)は Notice、確定失敗 (Auth/Timeout/InvalidResponse/Quota)は Error。
- **C usage WARN flood 解消 (71c0c44)**: `evorch-gui.rs:317-357` の全量転送を整理。`storage_bridge.rs` で `handle_event` が Usage を `UsageAggregator` へ、非 Usage を `storage.append_event` へ振り分け。60 秒間隔で `flush_into(&StorageHandle as &dyn UsageSink)` を呼ぶ (ADR0012 準拠)。同期 storage は `spawn_blocking` に隔離、bus drop 時に最終 flush。metrics 保存の 1 分バケット (provider/model)が機能。
- **D 動作中インジケータ (0cd1c56/a5a0708)**: theme tokens RUNNING (cyan 0x22d3ee) / WAITING (yellow 0xfbbf24) 追加 + phase_color 共有。新規 `panes/phase_indicator.rs` で Running→spinner+``running`` pill、Waiting→``waiting (input)`` pill (スピナーなし)、Done/Error/Pending は静 pill。chat header 右上の旧 mute label を置き換え。
- **E composer Esc でキャンセル (f5fe78d/3fc2071/592d1ed)**: slash completion が開いていれば Esc で dismiss (focus 維持)、閉じていれば `WorkbenchCommand::CancelChat` → sink が chat_runs[thread_id] から run_id を引き `AgentRuntime::cancel(run_id)` 既存 API (runtime.rs:663-668) に接続、成功後は chat_runs から削除 (次 Send が新 run spawn)。composer の Send ボタンは Running 時は danger Cancel に差し替わる (Waiting 時は Send のまま)。cancelled reason は Error ではなく Notice ``Run cancelled`` で表示。

検証: workspace 全テスト green (唯一残るが pre-existing flake の `headless_run_completes_with_single_mock_response`)、clippy(-D warnings)・fmt clean。各 wave で RED→GREEN 同一コミット、PNG 証跡は `/tmp/opencode/models-{fetched,selected,applied-default}.png` / `/tmp/opencode/e-cancel-{completion,running,waiting}.png`。

## v0.8 ツール可視性 + Markdown の実装確定（2026-09-09、main 直接マージ）

v0.7 の実稼働検証で判明した 3 件を追加でカバー。
- **A1 tool event payload 拡張 (8fd168f)**: `ToolStarted.input` (Option<serde_json::Value>) と `ToolCompleted.output` (Option<String>) を event-bus に追加。serde default で旧 event JSON 互換、skip_serializing_if でシリアライズ時に省略。executor (tools/src/executor.rs) で payload を populate: ToolStarted で input (execute の入力 JSON)、ToolCompleted で ToolResult.content を output に入れる。`detail` (メタデータ) は A1 前から既存そのまま。下流 (storage/tools/event-bus) のテスト fixture を `input: None` / `output: None` で埋める compile fix を c051ddb で追加。
- **A2 TranscriptEntry::Tool 拡張 (2f762db)**: GUI transcript model が tool call の input/output/detail/is_error を保持。ToolStarted で新規 entry (input 保持)、ToolCompleted で同 call_id 行を検索して output/detail/is_error を更新。call_id 不在でも entry 生成 (defensive)。approval 系は status 更新のみ。
- **A3 tool 折りたたみ UI (597a754)**: 新規 `panes/transcript_tool.rs` で Tool 行を clickable にし、pane ID + 完全 call_id 単位で expand/collapse state を保持。header に `[🔨 bash] call_id + status (spinner/✓/✗) + file_path/command の概要`。展開時に Input (pretty JSON) / Output (等幅、is_error 時は Error セクション ERROR_FG 色) / Detail を表示。output 本文 (read file content / bash stdout) は markdown 解釈せず injection 防止として等幅プレーン表示。Chat / Agents 詳細の両 transcript で共通。Message markdown render は A4 で導入済みで併用。
- **A4 markdown レンダリング (0ea5670)**: `panes/markdown_render.rs` 新規、`egui_commonmark 0.25` (egui 0.36 互換) を採用。`TranscriptEntry::Message` のみ markdown render (太字/斜体/取り消し線、code block/inline code、links、list、quote、H1/2/3)。UserMessage は plaintext、Reasoning は muted。tool の output/result は markdown として render しない (A5 で明示)。

検証: workspace 全テスト green (21 group、flake のみ)、clippy(-D warnings)・fmt clean。RED→GREEN 同一コミット。PNG 証跡は transcript_tool の collapsed/expanded/エラー状態で4画像。

## v0.9 shell tool 実用性 + tool transcript 可読性の実装確定（2026-09-10、main 直接マージ）

v0.8 の実稼働検証で判明した shell tool 実行不能、tool 入出力の可読性不足、ログ/レイアウト噪音を修正。

- **shell tool を `sh -c` 経由に修正 (ad83290)**: これまで `command` を直接 `execvp` 相当で起動していたため、`find . -name "*.rs" | head` や `git status && git diff` のような pipe/redirect/glob/chain が sandbox 内で失敗していた。`ShellTool` は command を `sh -c` に渡す方式へ変更し、通常の POSIX shell 構文を利用可能にした。`args` は後方互換のため optional/deprecated として残し、command 末尾へ結合して shell 解釈する。contract 判定も結合後コマンドに対して実施。stdout/stderr は単一 output に統合し、PTY 実行時のセクション見出しも除去。
- **tool transcript を CLI 風に整形 (f5fd196)**: tool header は bash/shell なら `$ command`、read/write/edit なら対象 path を表示。Input は bash/read 系で生 JSON ではなく command/path を優先表示し、その他 tool は JSON pretty print を維持。Output は stdout/stderr を統合表示し、collapsed 時は先頭5行、expanded 時は全文を表示する。共通 transcript card は左余白を増やし、entry accent 線が本文文字と重ならないように調整。
- **egui_extras loader WARN 抑制 (474aa2a)**: image attachment 未実装の間、`egui_extras::loaders` の画像 loader WARN が GUI 起動時に毎回出ていたため、log filter で `error` まで抑制。v05 実装時に再有効化する。
- **v05 image attachment intent の queue seed (9fb10e7)**: `.intent-cli/issues/v05-image-attachment-ui/` に packet/github-body/implementation/review-context を作成し、画像添付 UI intent を queue-state に登録。v0.9 では実装せず、依存関係付きで後続 issue 化する方針を記録。
- **style 修正 (71ad6e4)**: `logging.rs` の rustfmt 差分のみ解消。

検証: workspace 全テスト 21 group green（既知 flake `headless_run_completes_with_single_mock_response` のみ）、`cargo clippy --workspace --all-targets -- -D warnings` clean、`cargo fmt --all -- --check` clean。shell tool 回帰テスト10件、実 bwrap テスト3件、transcript tool 対象テスト17件が成功。tool failure の最終 Error、retryable failure の Notice、markdown/tool card 描画の既存回帰も維持。

## Bundle A / Browser / Process-owner 連携の採択（2026-09-10 競合調査反映）

### Bundle A: コア GUI UX（v0.6）

- Git snapshot undo/redo（working state を破壊せず巻き戻し・再適用する UI）
- 画像添付 UI（composer への画像ペースト/添付・thumbnail preview・削除・送信。`v05-image-attachment-ui` の内容を統合）
- Session tree / fork UI（thread 間の親子・分岐を可視化し切替可能にする）
- Keymap / theme の user-facing 設定（runtime reload 可能、既存 config 層へ接続）
- Slash command 拡張機構（新規コマンドを registry へ追加可能、既存 composer 構造を拡張）

### Bundle C': thread process owner / claim（v0.6）

GUI 複数起動時の連携は ADR 0024 に従う。GUI は thread へ attach して read-only で監視し、必要時に claim / handoff を実行する。終了時に active な thread がある場合は警告する。

### Bundle Browser: 埋め込みブラウザ（opt-in、v0.7）

既定は外部 Playwright/Chromium プロセス + CDP screencast を `Browser` pane へ JPEG フレームとして流す構成。将来 `cef-rs` OSR / `browser-relay` を feature flag で追加。headless 実行の可視化は action log + 前後 screenshot + DOM diff で担保し、フォーカスは明示操作時のみ取得する。

### Bundle B: GUI 補助要素（v0.6）

- Compaction UX 可視化: cache transition / compaction reason / after-token を Diagnostics と transcript へ表示
- Diagnostics v0.5 骨格: DiagnosticBus の event 契約と最小 collector

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

## v0.10 (multi-fix batch): shell tool ガード + grep ripgrep化 + tool 表示改善 + thread 復元

### L-A: shell tool command dispatch 保護 (d8f91a0)

- 根本原因: `ad83290` の `sh -c` 化は既に有効だった。別の bash ツール経路や wrapper は存在しない。
- ユーザーが見た `bwrap: execvp pwd && ls -la` は **旧バイナリ／再起動前のプロセス** が最有力。
- `Shell → production_executor → ToolExecutor → BwrapSandbox::wrap` の経路を固定する回帰テストを追加。
  - `pwd && ls -la`
  - `ls -la`
  - Shell→sandbox の CommandSpec および bwrap argv (tail が `sh -c <command>` ) を検査。
  - 旧実装へ一時復帰で RED、現行 `sh -c` で GREEN を確認。
- `cargo test -p tools` 212 件成功、実 bwrap 3件成功。

### L-B: grep tool ripgrep 化 + 出力制限 (c9a80f8)

- 原因: 同期で全ファイル読み込み + 全一致行を無制限に蓄積。LLM context が肥大化して provider が 413。
- 変更: 検索バックエンドを **非同期 ripgrep (rg )** に移行。PATH 上の rg を使用。
- 出力を **注記込みで最大 200 行・8KiB** に制限。超過分は `[truncated: N more lines]` で要約。
- ファイル順に `path:行番号:内容` 形式を維持。有界バッファでメモリ蓄積を廃止。60秒 timeout。
- runtime の `push_tool_result` は本文をそのまま履歴へ格納するため、grep 側でサイズを強制。
- `cargo test -p tools` 212 件成功、clippy clean。

### L-C: tool 表示の OpenCode/pi 準拠化 (966a98b)

- `read/write/edit` の Input が JSON 残っていたのは `file` キー未認識が原因 → **`Read: .`** 形式に統一。
- run ID 付き tool event が thread transcript に未配送だった → **起動直後から表示** (pending)。
- 実行中は **ヘッダー内 spinner＋Running のみ**。展開を抑止し、完了後に結果を表示。
- headless harness を固定フレーム進行に修正 (spinner の停止を待たない)。
- 指定 3 テスト RED→GREEN、関連 22 件成功。全体テストは基準 `2282a63` 由来の表示ラベル差異が残る。

### L-D: thread 履歴復元 + 切替 chat 更新 (9e32b7a)

- 原因: chat が全 thread 共通の transcript を参照していた。起動時に保存イベントを再生する経路がなかった。
- transcript を thread ごとに分離し、sidebar 選択に追随。
- バックグラウンドの応答を所属 thread へ配送。起動時に SQLite 保存イベントを再生。
- ユーザー入力を既存 sidebar JSON に追加保存し、thread 一覧・選択状態とともに復元。
- 指定 3 テスト RED→GREEN、関連 33/33 成功。
- 全体テスト失敗は既存の Markdown ラベル不一致を維持 (本変更とは独立)。

### 残課題

- ユーザー実機で shell tool が効いていない原因は、現在の GUI プロセスが旧バイナリ／未再起動の可能性。再ビルド済みバイナリで `cargo run -p gui --bin evorch-gui` から起動して shell tool で `pwd && ls -la` を実行して確認する必要がある。
- 413 Payload Too Large は grep の制限で緩和したが、会話履歴全体の蓄積は runtime/compose 側のコンテキスト管理で別途対象とする必要がある。
