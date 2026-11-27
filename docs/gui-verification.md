# GUI 検証ガイド

`evorch-gui` は core-client である。単一の `evorch-core` に接続する表示・入力の窓であり、
自分では runtime・DB writer・owner socket を持たない（ADR 0045）。このガイドは、その窓と
core の間の契約をどのレイヤーで検証するかを示す。

旧 embedded GUI（`--legacy-embedded`、`--demo`、`--state` / `--settings` / `--layout` /
`--save-layout`）と、その画面専用だった検証基盤は撤去した（下記「撤去した検証」）。

## CI での実行範囲 (PR と main)

gui group のテストは、PR でも main push でも同じものを実行する。main だけで実行する GUI テスト、
画像証跡、native / Wayland 検査は無い（以前の `Main-only GUI tests` step、`offscreen-gate`、
`gui-evidence` は廃止した）。

| 検証 | PR | main push |
| --- | --- | --- |
| gui の通常テスト（ライブラリ、`attention`、`models_usage`、`pull_requests`、`settings_ui`、`transcript_cards`、`workbench_shell`、`runtime_threads`、`default_core_client`、`guard_entry`、gui の bin テスト）、`workspace-ui`、`workspace-state`、`core-client-e2e` | 実行する | 実行する |
| cache 契約（`core-client-e2e::default_gui_cache`、`evorch-core::core_renderer_cache` ほか。`scripts/contract-test-filters.sh`） | 実行する | 実行する |
| I/O 契約（`recorder::storage_bridge`、`workbench-host` の起動・flush テスト、`gui::diagnostic_ledger` ほか） | 実行する | 実行する |
| browser E2E (5 invocation)、CEF E2E (6 invocation) | 実行する | 実行する |

- 一覧・分割 (`scripts/verify-test-inventory.sh`、`ci` job) は event に依存せず、PR でも main でも
  同じ suite を列挙する。`default = cache + io + remaining`。gui group の suite が空になることは
  許さない（`scripts/ci-test-groups.json` の `allow_empty_suites` は gui では空）。
- cache / I/O 契約の登録を変える PR は、先に代替の検証を登録してから旧テストを消す。空の selector や
  常に成功する登録は置かない（`scripts/check-cache-contracts.sh`、`scripts/verify-test-inventory.sh`）。

## 検証レイヤー

### L1: モデル・状態単体テスト

- 役割: egui を介さないモデル・状態遷移・イベント変換の検証。
- コマンド: `cargo nextest run -p gui`
- 例: `crates/gui/tests/transcript_cards/transcript.rs`

### L2: ヘッドレス kittest 振る舞いテスト

- 役割: egui_kittest `Harness` による実ウィジェット（会話カード、composer、設定画面、sidebar、
  dock、terminal、diff など）のラベル・クリック・キー入力の検証。GPU アダプタ不要。
  core-client が使う共有ウィジェットだけを対象にする。
- コマンド: `cargo nextest run -p gui`

### L3: core-client の経路

- 役割: 実際の client actor（`gui::core_client`）が core の Unix socket に接続し、
  core の runtime・routing・HTTP adapter を通る経路の検証。
  - `core-client-e2e::default_gui_cache`: 窓から送った会話が provider に届く wire の prefix を保つこと
    （窓の history 読み取り、窓を閉じて別の窓が同じ thread を続ける場合を含む）。
    窓を閉じても core の run が止まらないことの検証でもある。
  - `core-client-e2e::gui_pull_request_cache`: 実 PR service を配線した core に対し、窓の client actor から
    link / auto-fix 切替 / refresh（同一 operation id の再送を含む）/ budget reset / 送信先変更 / manual notice を
    送り、受理を待ってから、notice が末尾に一度だけ追加され、余計な user request が無く、wire prefix が保たれること。
    notice を窓が一つも開いていない間に commit し、後から開いた窓の履歴復元・再読込・再接続で履歴と保存済み入力が
    変わらないことも含む（cache gate）。
  - `core-client-e2e::execution_teams_lanes` の `pending_lane_attention_read_and_shown…`: 未解決の lane attention
    （Ready のまま閉じられていない report）を画面と Web client が読み・表示しても、model 呼び出しと保存済み prefix が
    変わらないこと（cache gate）。
  - `gui::default_core_client`: 起動と引数の契約（core が無い場合の報告、削除した引数の拒否）。
  - `core-client-e2e` の各テスト: 添付、conversation、renderer 経路。
- コマンド: `cargo nextest run -p gui -p core-client-e2e`

### L4: ブラウザ fake-source テスト

- 役割: Chromium を起動せず、注入したフェイクビューで Agent browser タブの描画・操作履歴の重ね表示・
  スレッドごとのタブ表示を検証する（`crates/gui/tests/workbench_shell/browser.rs` ほか）。
- `cef` と `browser` は `gui` の既定 feature。`gui` は CEF なしのビルドを `compile_error!` で拒否し、
  `lint` ジョブは `--no-default-features` が拒否されることを確認する。

### L5: 実 Chromium / CEF E2E

- 役割: 実 Chromium をヘッドレスで起動する agent browser tools の検証（`browser-e2e`）と、
  実 CEF による renderer helper と artifact preview の検証（`cef-e2e`、
  `core-client-e2e::preview_cef` など）。通常実行では ignored。
- コマンド: `cargo test -p browser --test agent_chromium -- --ignored --nocapture`
- 環境変数: `CHROME`（chromiumoxide が参照する Chromium パス。CI で設定）
- CI ジョブ: `browser-e2e`、`cef-e2e`

### L6: 実機起動スモーク (手動)

- 役割: eframe がオペレータの実ディスプレイで起動し、ウィンドウ管理・フォーカス・終了がその環境で
  正しく動くことを確認する。CI でもエージェントでも実行してはならない。人間のオペレータが自分のマシンで行う。
- 手順:
  1. `evorch-core` を同じ install から使える状態にして、一意なタイトルで起動する。

     ```sh
     cargo build -p gui -p evorch-core
     cargo run -p gui --bin evorch-gui -- --window-title "evorch-smoke-$(date +%s)"
     ```

     `--window-title` を省略すると既定タイトルは `evorch`。最小サイズは `gui::window::MIN_INNER_SIZE`
     (960x600) が常に強制される。core が見つからない・起動できない場合は、窓の中（窓が作れなければ
     プロセスのエラー、終了コード 1）に報告される。他の起動方法への fallback は無い。
  2. ウィンドウを一意なタイトルで特定する（X11: `xdotool search --name evorch-smoke-`、
     Wayland: `hyprctl clients -j` / `swaymsg -t get_tree`）。
  3. スクリーンショットを撮り、sidebar と composer が描画されていることを確認する。
  4. ウィンドウを 960x600 未満にリサイズしようとして、WM に拒否されることを確認する。
  5. 閉じるボタン（X11 では `xdotool windowclose <id>`）で閉じる。窓を閉じても core の run は止まらない。
  6. 終了コードが 0 であることを確認する（`echo $?`）。

## 撤去した検証

旧 embedded GUI の撤去に合わせて、次の検証を意図的に廃止した。新 GUI に同等の保証があるとは
扱わない。新 GUI 向けの画像・native 検査は別課題である。

| 廃止したもの | 失った検証 |
| --- | --- |
| `headless_capture`、`scripts/gui-evidence-matrix.sh`、`scripts/gui-plan-evidence.sh`、`ci` の `gui-evidence`、`offscreen-gate`（ignored テスト一括と画像 artifact） | 旧 Workbench の状態・theme・サイズ・DPI ごとの PNG 生成と枚数の網羅検査 |
| `native_qa_window`、`scripts/check-gui-native.sh` | 旧 QA window の native（Xvfb + Openbox）の入力・ウィンドウ動作検査 |
| `scripts/check-gui-wayland-idle.sh` | 旧 GUI の Wayland idle 性能検査 |
| `scripts/ci-main-only-gui.sh`、`scripts/ci-main-only-gui-tests.json`、`main-only-gui` suite | main だけで走らせる gui 統合テスト 7 binary の登録と、新システムの混入検査 |
| 旧 GUI 専用の `html_preview` ignored テスト（CEF E2E の 1 invocation） | 旧 GUI 内蔵の HTML preview。renderer helper と artifact preview の実 CEF テストは維持 |
| `size_dpi_matrix`、`evidence_policy`、`theme_headless`、`theme_picker_headless` と、`*_headless` の旧画面テスト（`gui::app` / `HeadlessWorkbench` 前提） | 旧画面のサイズ x DPI ジオメトリ、画面ごとの kittest。共有ウィジェットのテストは残した |
| `docs` の `--demo`・旧 bin・旧 job の手順 | 旧画面の手動・自動手順 |

廃止した job・bin・script への参照が workflow・配布・検査スクリプトに残らないことは、
`scripts/tests/test_ci_workflow_config.sh` ほかの検査で確認する。
