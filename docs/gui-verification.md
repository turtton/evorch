# GUI 検証ガイド

evorch-gui の検証は 8 つのレイヤー (L1 から L8) で構成する。L1 から L7 は
コマンドで再現可能で、L8 はオペレータの実ディスプレイ上で確認する。

## CI での実行範囲 (PR と main)

L1 から L7 のうち、下表の検証は **main への push でだけ** CI が実行する。PR の `ci` が緑でも、
これらが成功したことは意味しない。

| 検証 | PR | main push |
| --- | --- | --- |
| `scripts/ci-main-only-gui-tests.json` に登録した gui 統合テスト 7 binary (`attention`、`models_usage`、`pull_requests`、`settings_ui`、`size_dpi_matrix`、`transcript_cards`、`workbench_shell`。L1〜L3、L5 の大半) | 実行しない (一覧と分割の検証だけ行う) | `tests (gui 1/1)` の `Main-only GUI tests` step |
| ignored テスト一括と画像証跡 (L4)、native / Wayland 検査 (L7) | 実行しない | `offscreen-gate`、`gui-evidence` |
| gui ライブラリの通常テスト、`runtime_threads`、`theme_*`、`evidence_policy`、gui の bin テスト、`workspace-ui`、`workspace-state`、`core-client-e2e`、cache / io 契約 | 実行する | 実行する |
| 新システム (core-client) のテスト、browser E2E (5 invocation)、CEF E2E (7 invocation)、`html_preview` の ignored テスト | 実行する | 実行する |

- **登録の単位**は integration test の binary だけである。テスト名や module の一覧は持たない。
  登録済み binary の中でテストを追加・改名・削除しても登録は変わらず、登録していない binary
  (新設の binary を含む) は自動的に PR 側に残る。
- **新システムのテストは登録済み binary に入れない。** gui group の inventory step
  (`scripts/run-test-group.sh inventory`、PR でも main でも実行) が、登録した各 binary の
  コンパイラの dependency-info (rustc が test binary の隣に書く `<binary>.d`) に列挙された
  **すべてのファイル**を、拡張子を問わず、構文解析せずにそのまま読み、文字列 `core_client` を含めば失敗させる
  (`scripts/ci-main-only-gui.sh check-depinfo`)。コメントの除去も単語境界の判定もしないため、
  コメントや文字列、`core_client_e2e` のような長い識別子の中の出現も拒否する (過剰に拒否する側。例外の仕組みは無い)。
  `include!("body.inc")` で Rust として取り込まれるファイルやバイナリも対象で、`include_str!` や
  `include_bytes!` で読むファイルも列挙されるため検査対象になる。module の解決はコンパイラのもので、
  入れ子、同名の子 module、inline module、`#[path]`、`include!` をすべて網羅する。dependency-info が
  見つからない、対応付けられない、列挙されたファイルが無い場合も失敗する。登録 binary に `core_client` を
  含む文章を書きたい場合は、その binary を登録から外す (PR 側に残す) 判断になる。
  これは直接の混入を見つける検査であり、テストの意味上の分類を証明するものではない。レビューで移動範囲を確認する。
- 一覧・分割 (`scripts/verify-test-inventory.sh`、`ci` job) は event に依存せず、PR でも main でも
  同じ suite を列挙する。`default = cache + io + ci-remaining + main-only-gui`、
  `remaining = ci-remaining + main-only-gui`。ローカルの `scripts/run-contract-tests.sh remaining` は
  従来どおり (cache と io を除く) すべてのテストを実行する。CI は `remaining` を実行せず、
  `ci-remaining` と、main では `main-only-gui` を実行する。
- 登録の変更 (`scripts/ci-main-only-gui-tests.json`、`scripts/ci-main-only-gui.sh`) は central file であり、
  integration PR だけが変更できる。
- main の `main-only-gui` が失敗すれば `tests (gui 1/1)` と `ci` が失敗する。`offscreen-gate` と
  `gui-evidence` の失敗はその job と workflow を失敗させるが、`ci` の `needs` には含まれない。
  成功への変換 (`continue-on-error` など) は行わない。

### main の失敗の確認責任

- **確認責任者は、当該変更を main にマージした担当者**とする。代理が必要なら、マージ時に明示する。
- マージ後、その変更を含んで完走した main の run で `tests (gui 1/1)`、`offscreen-gate`、
  `gui-evidence` を確認する。作業の終了時点で run が完走していなければ、対象の SHA と run、確認担当を
  引き継ぐ。確認していない状態で完了扱いにしない。
- 失敗は同じ作業セッション、遅くとも次の作業の開始時に切り分け、修正の担当を決める。
- 待機中の main の run は後続の push に置き換えられることがある。未検証のコミットが複数ある場合は、
  最後に成功した SHA 以降を原因の候補として扱う。

## 検証レイヤー

### L1: モデル単体テスト

- 役割: egui を介さない純粋なモデル・状態遷移・イベント変換の検証。
- コマンド: `cargo test -p gui` (CI では `cargo test --workspace` に含まれる)
- 例: `crates/gui/tests/transcript_cards/transcript.rs`、`crates/gui/tests/evidence_policy.rs`、
  `crates/gui/tests/models_usage/model_metadata.rs::config_roundtrip_with_preset_keeps_fields`
- 環境変数: なし
- CI ジョブ: `tests (gui …)` (登録 binary は main の `Main-only GUI tests` step のみ。上の表を参照)

### L2: ヘッドレス kittest 振る舞いテスト

- 役割: `gui::headless::HeadlessWorkbench` (`with_pixels_per_point` /
  `screen_rect` / `scroll_label_into_view` / `label_rects`) と egui_kittest
  `Harness` による、実ウィジェットのラベル・クリック・キー入力の検証。
  GPU アダプタ不要で通常のテスト実行に含まれる。
- コマンド: `cargo test -p gui --tests`
- 例: `crates/gui/tests/workbench_shell/sidebar_headless.rs`、
  `crates/gui/tests/transcript_cards/composer_dispatch_headless.rs`、
  `crates/gui/tests/settings_ui/provider_settings_headless.rs`
- 環境変数: なし
- CI ジョブ: `tests (gui …)` (登録 binary は main の `Main-only GUI tests` step のみ)

### L3: サイズ x DPI ジオメトリマトリクス

- 役割: 5 状態 (empty / demo / error-thread / edit-profile / theme-settings) x 複数フレーム
  サイズで、主要コントロールがビューポート内に到達可能であることを
  `label_rects` の矩形で検証する。最小サイズ 960x600 が実アプリの
  `gui::window::MIN_INNER_SIZE` と一致することも確認する。
- コマンド: `cargo test -p gui --test size_dpi_matrix`
- 例: `crates/gui/tests/size_dpi_matrix.rs::demo_geometry_matrix`、
  `crates/gui/tests/size_dpi_matrix.rs::min_size_matches_real_app_viewport`
- 環境変数: なし
- CI ジョブ: `tests (gui …)` (`size_dpi_matrix` は登録 binary なので main の `Main-only GUI tests` step のみ)

### L4: オフスクリーンレンダ証跡

- 役割: wgpu オフスクリーンレンダで PNG 証跡を生成する。通常実行では
  ignored になっており、CI の lavapipe 環境で一括スイープする。
  `gui::evidence` のポリシーにより、アダプタが取得できない場合の挙動は
  `EVORCH_REQUIRE_ADAPTER` で制御する。
- コマンド:
  -  ignored テスト一括: `cargo test -p gui --tests -- --ignored --nocapture`
  -  証跡マトリクス: `scripts/gui-evidence-matrix.sh [OUT]`
     (デフォルト出力 `target/gui-evidence`。3 テーマそれぞれで 6 状態 x 4 サイズ、
     demo / edit-profile x 2 DPI、既定の 6 パネルと Memory パネル、
     追加の 4 設定画面を撮影し、
     テーマごとに 39 枚、合計 117 PNG の枚数を検査する)
  -  会話の計画の証跡: `scripts/gui-plan-evidence.sh [OUT]`
     (デフォルト出力 `target/gui-evidence/conversation-plan`。基本マトリクスとは別に、
     3 テーマ x 4 状態 (未計画 / 階層+混在進捗 / 長文詳細+変更履歴 / 消去済み) の
     12 PNG を撮影し枚数を検査する。基本の 117 枚には含めない)
- 環境変数: `EVORCH_REQUIRE_ADAPTER=1` (CI で必須化)、
  `EVORCH_METADATA_EVIDENCE` (model_metadata 証跡の出力先)、
  `WGPU_BACKEND=vulkan` (lavapipe 利用時の推奨指定)
- CI ジョブ (main への push のみ): ignored テスト一括は `offscreen-gate` (アーティファクト
  `gui-test-evidence`)、証跡マトリクスは並列の `gui-evidence` (アーティファクト `gui-evidence`)。
  PR では実行しない。アーティファクトの名前と取得先は変わらない。
  マトリクスは `headless_capture` を 1 回だけビルドして直接起動し、
  `GUI_EVIDENCE_JOBS` (既定は CPU 数、最大 4) 並列で撮影する

### L5: ブラウザ fake-source テスト

- 役割: Chromium を起動せず、注入したフェイクビューで Agent browser タブの
  フレーム描画・操作履歴の重ね表示・閉じたページの最後のフレーム保持・
  スレッドごとのタブ表示と View ボタンを実 egui ラベル経由で検証する。
- コマンド: `cargo test -j 1 -p gui --test workbench_shell -- browser:: agent_browser_workbench:: --test-threads=1`
- 例: `crates/gui/tests/workbench_shell/browser.rs::a_closed_page_keeps_its_last_frame_and_says_so`
- 環境変数: なし
- CI ジョブ: `tests (gui …)` (`workbench_shell` は登録 binary なので main の `Main-only GUI tests` step のみ。`cef` と `browser` は `gui` の既定 feature なので既定のワークスペーステストで実行される。`gui` は CEF なしのビルドを `compile_error!` で拒否し、`lint` ジョブは `--no-default-features` が拒否されることを確認する)

### L6: 実 Chromium E2E

- 役割: 実 Chromium をヘッドレスで起動し、localhost フィクスチャに対して
  agent browser tools の操作・タブ・ネットワーク遮断を検証する。通常実行では ignored。
- コマンド: `cargo test -p browser --test agent_chromium -- --ignored --nocapture`
- 環境変数: `CHROME` (chromiumoxide の実行ファイル検出が参照。CI で設定)
- CI ジョブ: `browser-e2e`

### L7: Linux 仮想 X11 実ウィンドウ操作

- 役割: 専用 Xvfb と Openbox 内で GUI 本体と同じ `WorkbenchApp` を使う
  `native_qa_window` を起動し、実際のウィンドウへキーボードとマウスを入力する。
  Ctrl+1/2 のタブ移動、レイアウト保存・リセット、最小サイズの強制、
  正常終了を検証する。各操作の PNG、保存レイアウト、ログを証跡として残す。
  この fixture はランタイムとサンドボックスを起動しない。
- コマンド: `cargo build -p gui --bin native_qa_window` の後に
  `scripts/check-gui-native.sh [OUT]`。既定出力は `target/gui-native`。
- 必要コマンド: `Xvfb`、`openbox`、`xdotool`、ImageMagick、`jq`。
  Nix 開発環境では例えば
  `nix develop -c nix shell nixpkgs#xorg-server nixpkgs#openbox nixpkgs#xdotool nixpkgs#imagemagick nixpkgs#jq -c scripts/check-gui-native.sh`。
- 実アプリまで通す場合: `cargo build -p gui --bin evorch-gui` の後、
  bubblewrap が動作する Linux ホストで `GUI_QA_MODE=runtime scripts/check-gui-native.sh [OUT]`。
  GitHub Actions ランナーでは bubblewrap のネットワーク名前空間作成が拒否されるため、
  CI では fixture モードを使用する。
- 環境変数: `GUI_QA_MODE` (`fixture` / `runtime`、既定 `fixture`)、
  `GUI_BIN` (モードに応じた既定バイナリを上書き)、
  `GUI_QA_OUTPUT_DIR` (位置引数を省略した場合の出力先)。
- CI ジョブ: `gui-evidence` (main への push のみ。`gui-evidence` artifact の `native/`)

### PR の代表画面プレビュー (休止中)

PR への画像の自動添付は**休止している**。`ci` workflow は `gui-pr-preview` artifact を作らず、
既定ブランチの `GUI PR evidence` workflow の publisher job は `if: ${{ false }}` で runner を取得しない。
そのため **`GUI_EVIDENCE_TOKEN` の設定は現在は不要**である (設定済みでも読まれない)。
`ci` workflow の名前は変えていない。

- 画面証跡は main の `gui-evidence` (`gui-evidence` artifact) と `offscreen-gate`
  (`gui-test-evidence` artifact) に残る。これは PR の証跡ではない。main の画像で PR の代替としない。
- 再開は別の変更で行い、**先に producer を戻す**。同じ PR の run が `gui-pr-preview` (5 枚と
  `pr.json`) を作って upload する契約を復元し、その後で publisher の条件
  (run の event が `pull_request` で結論が `success`) を戻す。再開時には token の設定
  (対象リポジトリの書き込み権限を持つ OAuth token または PAT を Actions secret
  `GUI_EVIDENCE_TOKEN` に設定する。標準の `GITHUB_TOKEN` は添付 API に対応しない。
  PR コードが動く `ci` には渡さない) が再び必要になる。
- 休止への移行中は、この変更の PR head から起動した `GUI PR evidence` が、存在しない preview の取得で
  失敗することがある。休止の変更が main に入った後の PR では、publisher は skipped になる。

### L8: 実機起動スモーク (手動)

- 役割: eframe がオペレータの実ディスプレイで起動し、ウィンドウ管理・
  フォーカス・終了がその環境で正しく動くことを確認する。
- コマンド: 下記「実機スモーク手順」を参照。
- CI ジョブ: なし (手動のみ)

## 実機スモーク手順 (AC6)

この L8 手順はオペレータの実ディスプレイにウィンドウを開く。CI でも
エージェントでも実行してはならない。実行は人間のオペレータが自分の
マシンで行う。L7 は専用の仮想 X11 ディスプレイだけを使用する。

1. 一意なタイトル付きでデモ状態を起動する。

   ```sh
   cargo run -p gui --bin evorch-gui -- --demo --window-title "evorch-smoke-$(date +%s)"
   ```

   `--window-title` を省略すると既定タイトルは `evorch`。最小サイズは
   `gui::window::MIN_INNER_SIZE` (960x600) が常に強制される。

2. 新しいプロセスのウィンドウを一意なタイトルで特定する。

   - X11: `xdotool search --name evorch-smoke-` または
     `wmctrl -l | grep evorch-smoke`
   - Wayland (Hyprland):
     `hyprctl clients -j | jq '.[]|select(.title|startswith("evorch-smoke-"))'`
   - Wayland (sway):
     `swaymsg -t get_tree | jq '..|select(.name? and (.name|startswith("evorch-smoke-")))'`

3. 目視確認として、そのウィンドウのスクリーンショットを撮り、サイドバーと
   コンポーザが描画されていることを確認する。

   - wlroots 系: `grim -g "$(slurp)" smoke.png`
   - X11: `import -window <id> smoke.png`

4. ウィンドウを 960x600 未満にリサイズしようとして、WM に拒否されることを
   確認する (`with_min_inner_size` の実機での効果)。

5. 終了する。`crates/gui/src/keymap.rs` の `KeyAction` は
   `FocusAgentPane` / `FocusTerminalPane` / `SaveLayout` /
   `ResetLayout` のみで、終了用のキーバインドは存在しない。アプリの閉じる
   ボタンを使うか、X11 では `xdotool windowclose <id>` で閉じる。

6. 終了コードが 0 であることを確認する (`echo $?`)。

## カバレッジ表 (AC7)

| 機能 | レイヤー | テスト参照 |
| --- | --- | --- |
| 並列ツール呼び出し | runtime 統合テスト (L1 の前提) | `crates/runtime/tests/delegation/parallel_tool_calls.rs::shared_wave_runs_concurrently_exclusive_is_barrier`, `crates/runtime/tests/delegation/parallel_tool_calls.rs::results_returned_in_original_call_order`, `crates/runtime/tests/delegation/parallel_tool_calls.rs::all_permission_checks_happen_before_any_execution` |
| ストリーミング実行・トランスクリプト | L1, runtime 統合テスト | `crates/runtime/tests/provider_streaming/streaming_agent_loop.rs::deltas_published_before_run_completes`, `crates/runtime/tests/provider_streaming/streaming_agent_loop.rs::final_context_matches_buffered_semantics_no_double_emit`, `transcript_cards/streaming_transcript.rs::interleaved_text_reasoning_deltas_render_in_order`, `transcript_cards/streaming_transcript.rs::failed_or_cancelled_stream_keeps_partial_display` |
| ライブ telemetry (tok/s・TTFT・経過時間) | L1, L2 | `models_usage/telemetry_live.rs::request_started_records_start_and_elapsed_advances`, `models_usage/telemetry_live.rs::first_token_observed_sets_ttft`, `models_usage/telemetry_live.rs::provisional_tok_s_replaced_by_final_on_request_completed`, `models_usage/telemetry_live.rs::completed_metrics_render_as_headless_labels` |
| コスト・キャッシュ表示 | L1 | `models_usage/telemetry_cost.rs::cache_hit_rate_uses_senpi_formula`, `models_usage/telemetry_cost.rs::cost_hidden_when_pricing_unknown`, `models_usage/telemetry_cost.rs::cache_segment_hidden_below_10_percent`, `models_usage/telemetry_cost.rs::compact_line_format` |
| モデル料金設定 | config 統合テスト (L1 の前提) | `crates/config/tests/settings_suite/pricing.rs::model_entry_cost_fields_roundtrip_via_save`, `crates/config/tests/settings_suite/pricing.rs::static_toml_overrides_catalog_pricing`, `crates/config/tests/settings_suite/pricing.rs::unknown_pricing_returns_none` |
| 自動タイトル | L1, L2 | `settings_ui/auto_title.rs::first_user_message_on_default_title_triggers_rename`, `settings_ui/auto_title.rs::custom_title_never_overwritten`, `settings_ui/auto_title_provider.rs::production_title_uses_quick_route_or_explicit_thread_model` |
| 役割ごとのモデル割り当て | L2, L3, L4, config 統合テスト | `crates/config/tests/save_suite/save_agent_bindings.rs::save_role_binding_roundtrips_models_and_category_overrides`, `settings_ui/role_settings_headless.rs::role_binding_edit_saves_toml_and_reloads_runtime`, `settings_ui/role_settings_headless.rs::category_override_editable_per_role`, `settings_ui/role_settings_headless.rs::role_settings_geometry_matrix`, `settings_ui/role_settings_headless.rs::capture_role_settings_png_evidence` |
| ロールのモデルチェーン編集 | L2, L4 | `settings_ui/role_settings_headless.rs::chain::head_effort_offers_the_route_model_levels_and_saves`, `settings_ui/role_settings_headless.rs::chain::editing_the_head_keeps_the_fallback_entries`, `settings_ui/role_settings_headless.rs::chain::fallback_entries_can_be_added_reordered_removed_and_keep_their_effort`, `settings_ui/role_settings_headless.rs::chain::fallback_entries_only_offer_defined_routes`, `settings_ui/role_settings_headless.rs::chain::capture_role_chain_editor_png_evidence` (`target/gui-evidence/role-settings/role-chain-editor.png`) |
| 同一モデルのプール自動追加 | L1, L2, L4 | `crates/runtime/src/model_grouping/tests.rs`, `settings_ui/auto_grouping_headless.rs::saving_a_provider_adds_it_to_the_pool_of_the_same_model`, `settings_ui/auto_grouping_headless.rs::several_models_of_one_profile_are_offered_for_a_choice`, `settings_ui/auto_grouping_headless.rs::a_skipped_choice_is_not_asked_again`, `settings_ui/auto_grouping_headless.rs::a_pool_from_another_layer_is_not_copied_into_the_saved_file`, `settings_ui/auto_grouping_headless.rs::a_changed_canonical_id_warns_once_without_rewriting`, `settings_ui/auto_grouping_headless.rs::routing_settings_mark_automatic_candidates`, `settings_ui/auto_grouping_headless.rs::capture_auto_grouping_png_evidence` (`target/gui-evidence/auto-grouping/`) |
| 自動 write-mode | L1, runtime 接続テスト | `settings_ui/auto_write_mode.rs::submit_autostarts_unowned_thread`, `settings_ui/auto_write_mode.rs::owner_responsive_shows_notice_without_sending`, `settings_ui/auto_write_mode.rs::composer_autostart_reaches_real_runtime`, `settings_ui/auto_write_mode.rs::exists_race_with_other_owner_does_not_return_a_permit` |
| Waiting の既読/未読・確認応答 | L1, L2, L4 | `attention/attention_ack.rs::ack_requires_displayed_focused_and_revision_match`, `attention/attention_ack.rs::stale_revision_ack_is_ignored`, `attention/waiting_badge_headless.rs::unread_waiting_badge_filled_info_accent`, `attention/waiting_badge_headless.rs::read_waiting_badge_outline_only`, `attention/waiting_badge_headless.rs::capture_waiting_read_unread_png_evidence` |
| Codex quota 取得・表示 | L1, L2, L4, providers 統合テスト | `crates/providers/tests/codex_suite/codex_quota.rs::rate_limits_read_parses_stdio_jsonrpc_response`, `crates/providers/tests/codex_suite/codex_quota.rs::wham_fallback_when_app_server_missing`, `models_usage/codex_quota_display.rs::frame_loop_polls_injected_source_and_displays_quota`, `models_usage/codex_quota_display.rs::stale_indicator_shown_when_refresh_failed`, `models_usage/codex_quota_display.rs::capture_codex_quota_png_evidence` |
| コンポーザ `/new` | L1, L2 | `src/model/composer.rs::parse_new_without_args`, `transcript_cards/composer_dispatch_headless.rs::new_command_creates_and_switches_thread` |
| サイドバー (プロジェクト/スレッド) | L2, L3 | `workbench_shell/sidebar_headless.rs::add_and_select_project_persists_to_sidebar_file`, `workbench_shell/sidebar_headless.rs::create_switch_pin_thread_via_ui_clicks`, `workbench_shell/sidebar_headless.rs::thread_state_follows_lifecycle_events`, `size_dpi_matrix.rs::demo_geometry_matrix` |
| スレッドツリーの罫線・種別アイコン・家族間余白・折りたたみ時のロールアップ | L1, L2, L4 | `panes::sidebar::threads::tests::rails_continue_only_while_a_later_sibling_follows`, `panes::sidebar::threads::tests::branch_kind_names_escalations_and_forks_through_rewinds`, `transcript_cards/headless_conversation/thread_tree.rs::tree_rails_join_children_to_their_parent_and_stop_at_the_last_child`, `transcript_cards/headless_conversation/thread_tree.rs::child_rows_name_why_they_are_nested_beside_their_titles`, `transcript_cards/headless_conversation/thread_tree.rs::families_with_children_are_spaced_apart_from_neighbouring_rows`, `panes::sidebar::threads::tests::hidden_descendants_roll_up_the_whole_subtree_and_stop_at_the_next_sibling`, `transcript_cards/headless_conversation/thread_tree.rs::collapsed_parent_rolls_up_what_its_hidden_children_need`, `transcript_cards/headless_conversation/thread_tree.rs::a_collapsed_child_rolls_up_only_its_own_subtree`, `transcript_cards/headless_conversation/thread_tree.rs::a_row_s_own_attention_takes_precedence_over_its_hidden_children`, `transcript_cards/headless_conversation/thread_tree.rs::capture_thread_tree_png_evidence` (`target/gui-evidence/thread-tree/`) |
| 会話/トランスクリプト | L1, L2, L4 | `transcript_cards/transcript.rs::registry_routes_request_failed_with_run_id_to_thread_and_run`, `transcript_cards/burst_transcript.rs::burst_1500_tokens_per_tick_reaches_transcript_without_drops`, `workbench_shell/empty_states_headless.rs::conversation_with_messages_hides_placeholders`, `workbench_shell/empty_states_headless.rs::capture_cjk_conversation_png_evidence` |
| コンポーザ (送信/スラッシュ/画像) | L2, L4 | `transcript_cards/composer_ui_headless.rs::send_button_round_trip_issues_send_chat`, `transcript_cards/composer_ui_headless.rs::slash_completion_button_fills_input`, `transcript_cards/composer_dispatch_headless.rs::chat_send_issues_send_chat_and_shows_user_line`, `transcript_cards/composer_image_input.rs::paste_and_drop_render_real_thumbnails_without_pasting_base64_into_text`, `src/panes/composer_capture_test.rs::capture_cancel_states` |
| 送信拒否 (入力欄への復元・未配送行・再起動後の表示) | L2 | `transcript_cards/headless_conversation/rejected_send.rs::refused_chat_goes_back_to_the_composer_instead_of_the_transcript`, `transcript_cards/headless_conversation/rejected_send.rs::refused_follow_up_to_a_busy_run_is_not_shown_as_sent`, `transcript_cards/headless_conversation/rejected_send.rs::late_refusal_keeps_a_newer_draft_and_offers_the_message_above_the_composer`, `transcript_cards/headless_conversation/rejected_send.rs::refused_messages_are_not_replayed_as_sent_after_restart`, `transcript_cards/headless_conversation/rejected_send.rs::sidebar_saved_before_delivery_states_replays_messages_as_sent` |
| プロバイダ設定 (一覧/編集/モデル/Codex 認証) | L2, L3, L4 | `settings_ui/provider_settings_headless.rs::save_valid_settings_writes_evorch_toml_and_flips_status`, `settings_ui/provider_settings/models.rs::type_into_add_model_and_press_enter_adds_row_with_enable_checked`, `settings_ui/provider_settings/tabs.rs::codex_tab_hides_openai_grid_and_shows_login`, `size_dpi_matrix.rs::edit_profile_geometry_matrix`, `settings_ui/provider_settings/capture.rs::capture_codex_auth_png_evidence`, `settings_ui/provider_settings/capture.rs::capture_modal_png_evidence`, `settings_ui/provider_settings/models.rs::capture_model_management`, `settings_ui/provider_settings/tabs.rs::capture_both_settings_tabs` |
| モデルピッカー | L2, L4 | `models_usage/model_picker_headless.rs::picker_lists_profiles_and_models_from_catalog`, `models_usage/model_picker_headless.rs::selecting_model_persists_on_thread`, `models_usage/model_picker_headless.rs::picker_disabled_without_profiles_offers_settings`, `models_usage/model_picker_headless.rs::capture_model_picker_evidence`, `models_usage/model_metadata_capture.rs::capture_model_metadata` |
| エージェントグリッド | L2 | `runtime_threads/agents_grid_headless.rs::agents_grid_fits_nine_columns_without_horizontal_scroll`, `runtime_threads/agents_grid_headless.rs::agents_grid_respects_clip_rect_when_parent_scrolls_horizontally` |
| Diff | L2 | `workbench_shell/diff_headless.rs::diff_pane_shows_loading_then_ready_from_fixture`, `workbench_shell/diff_headless.rs::empty_diff_shows_explicit_empty_state`, `workbench_shell/diff_headless.rs::git_error_is_shown_and_ui_keeps_rendering` |
| Terminal | L2 | `workbench_shell/terminal_headless.rs::terminal_pane_renders_buffer_lines`, `workbench_shell/terminal_headless.rs::terminal_pane_renders_cjk_lines`, `workbench_shell/terminal_headless.rs::terminal_pane_renders_empty_buffer`, `workbench_shell/terminal_headless.rs::terminal_content_labels_within_viewport`, `workbench_shell/layout_v02.rs::undock_to_floating_and_reload_preserves_v02_panels` (terminal タブのドック操作) |
| Run 行モデル / Memory / Team ペイン | L2 | `workbench_shell/workbench.rs::tasks_row_updates_after_state_change_event`, `workbench_shell/memory_pane.rs::memory_pane_search_and_filter_use_persisted_projection`, `workbench_shell/team_pane.rs::team_pane_shows_claim_owner_and_ready_tasks` |
| Agent browser タブ | L1, L5, L6 | `crates/gui/src/app/agent_browser_tests.rs`, `workbench_shell/browser.rs::agent_pane_screencasts_the_selected_run_with_its_activity_overlaid`, `workbench_shell/browser.rs::a_closed_page_keeps_its_last_frame_and_says_so`, `workbench_shell/agent_browser_workbench.rs::a_page_opens_the_browser_tab_and_view_reopens_it_after_closing`, `transcript_cards/transcript_tool.rs::browser_tool_card_view_asks_to_show_its_runs_page`, `crates/browser/tests/agent_chromium.rs` |
| Loop ステータス | L2 | `loop_status_headless.rs::loop_state_tracks_stage_and_rejections_from_bus_events`, `loop_status_headless.rs::state_controls_issue_pause_and_resume_goal_commands_once` |
| テーマ | L2 | `theme_headless.rs::install_applies_dark_design_tokens`, `theme_headless.rs::workbench_installs_theme_on_first_frame`, `theme_headless.rs::dock_style_distinguishes_tab_states` |
| Dock レイアウト | L2 | `workbench_shell/dock_roundtrip.rs::workspace_dock_workspace_round_trip_preserves_nested_structure`, `workbench_shell/dock_roundtrip.rs::complex_nested_split_round_trip_preserves_tree_and_active_tabs`, `workbench_shell/layout_v02.rs::default_layout_is_sidebar_center_right_tabs`, `workbench_shell/layout_v02.rs::v1_layout_file_loads_via_migration_into_workbench` |
| Empty ステート | L2, L3, L4 | `workbench_shell/empty_states_headless.rs::conversation_without_project_offers_go_to_projects`, `workbench_shell/empty_states_headless.rs::composer_is_docked_at_bottom_in_empty_state`, `workbench_shell/sidebar_headless.rs::sidebar_without_projects_shows_placeholder_and_single_add_project_cta`, `size_dpi_matrix.rs::empty_geometry_matrix`, `workbench_shell/empty_states_headless.rs::capture_empty_composer_evidence` |
| 最小ウィンドウサイズ | L2, L3, L7, L8 | `settings_ui/window_options.rs::native_options_set_layout_sizes_when_created`, `settings_ui/window_options.rs::minimum_fits_default_when_sizes_are_compared`, `size_dpi_matrix.rs::min_size_matches_real_app_viewport`, L7 ネイティブ QA、L8 実機スモーク手順 4 |
| ツール承認 (相関 ID 表示・個別承認/拒否) | L1, L2, L4 | `attention/pending_approvals.rs::pending_approvals_removes_only_exact_resolved_key`, `attention/pending_approvals.rs::pending_approvals_ignores_unknown_and_other_scope_resolutions`, `attention/pending_approvals.rs::pending_approvals_workbench_removes_only_resolved_request`, `attention/approvals_panel_headless.rs::displays_correlations_and_arguments_when_requests_are_pending`, `attention/approvals_panel_headless.rs::approves_exact_scope_when_second_row_is_clicked`, `attention/approvals_panel_headless.rs::removes_only_resolved_row_when_resolution_arrives`, `attention/pending_approvals_capture.rs::capture_pending_approvals_png_evidence` |

テストファイルは特記なき限り `crates/gui/tests/` 配下。

## コマンド・環境変数リファレンス

| 名前 | 種別 | 意味 | 参照レイヤー |
| --- | --- | --- | --- |
| `EVORCH_REQUIRE_ADAPTER` | 環境変数 | `1` でオフスクリーンアダプタ未取得を skip ではなく失敗にする (`gui::evidence::AdapterPolicy`)。CI の offscreen-gate で設定 | L4 |
| `EVORCH_METADATA_EVIDENCE` | 環境変数 | `models_usage/model_metadata_capture.rs::capture_model_metadata` の証跡出力先ディレクトリ | L4 |
| `CHROME` | 環境変数 | chromiumoxide の実行ファイル検出が参照する Chromium パス。browser-e2e で設定 | L6 |
| `WGPU_BACKEND` | 環境変数 | wgpu のバックエンド指定。lavapipe 環境では `vulkan` を推奨 | L4 |
| `cargo test -p gui --tests -- --ignored --nocapture` | コマンド | ignored オフスクリーンレンダテストの一括スイープ | L4 |
| `scripts/gui-evidence-matrix.sh [OUT]` | スクリプト | 3 テーマ x 39 PNG のサイズ・DPI・パネル・設定画面の証跡を生成し枚数を検査 | L4 |
| `cargo test -j 1 -p gui --test workbench_shell -- browser:: agent_browser_workbench:: --test-threads=1` | コマンド | Agent browser タブのフェイクビューテスト | L5 |
| `cargo test -p browser --test agent_chromium -- --ignored --nocapture` | コマンド | 実 Chromium E2E | L6 |
| `scripts/check-gui-native.sh [OUT]` | コマンド | 専用 Xvfb 内の実ウィンドウを自動操作し、PNG・レイアウト・ログを残す | L7 |
| `GUI_QA_MODE` / `GUI_BIN` / `GUI_QA_OUTPUT_DIR` | 環境変数 | L7 の fixture / 実アプリ、起動バイナリ、出力ディレクトリを指定する | L7 |
| `cargo run -p gui --bin evorch-gui -- --demo --window-title <text>` | コマンド | 実機スモーク起動。既定タイトル `evorch`、最小サイズ 960x600 | L8 |
