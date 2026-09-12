# GUI 検証ガイド

evorch-gui の検証は 7 つのレイヤー (L1 から L7) で構成する。L1 から L6 は
コマンドで再現可能で、L7 のみオペレータの実ディスプレイ上での手動確認となる。

## 検証レイヤー

### L1: モデル単体テスト

- 役割: egui を介さない純粋なモデル・状態遷移・イベント変換の検証。
- コマンド: `cargo test -p gui` (CI では `cargo test --workspace` に含まれる)
- 例: `crates/gui/tests/transcript.rs`、`crates/gui/tests/evidence_policy.rs`、
  `crates/gui/tests/model_metadata.rs::config_roundtrip_with_preset_keeps_fields`
- 環境変数: なし
- CI ジョブ: `ci`

### L2: ヘッドレス kittest 振る舞いテスト

- 役割: `gui::headless::HeadlessWorkbench` (`with_pixels_per_point` /
  `screen_rect` / `scroll_label_into_view` / `label_rects`) と egui_kittest
  `Harness` による、実ウィジェットのラベル・クリック・キー入力の検証。
  GPU アダプタ不要で通常のテスト実行に含まれる。
- コマンド: `cargo test -p gui --tests`
- 例: `crates/gui/tests/sidebar_headless.rs`、
  `crates/gui/tests/composer_dispatch_headless.rs`、
  `crates/gui/tests/provider_settings_headless.rs`
- 環境変数: なし
- CI ジョブ: `ci`

### L3: サイズ x DPI ジオメトリマトリクス

- 役割: 4 状態 (empty / demo / error-thread / edit-profile) x 複数フレーム
  サイズで、主要コントロールがビューポート内に到達可能であることを
  `label_rects` の矩形で検証する。最小サイズ 960x600 が実アプリの
  `gui::window::MIN_INNER_SIZE` と一致することも確認する。
- コマンド: `cargo test -p gui --test size_dpi_matrix`
- 例: `crates/gui/tests/size_dpi_matrix.rs::demo_geometry_matrix`、
  `crates/gui/tests/size_dpi_matrix.rs::min_size_matches_real_app_viewport`
- 環境変数: なし
- CI ジョブ: `ci` (ワークスペーステストに含まれる)

### L4: オフスクリーンレンダ証跡

- 役割: wgpu オフスクリーンレンダで PNG 証跡を生成する。通常実行では
  ignored になっており、CI の lavapipe 環境で一括スイープする。
  `gui::evidence` のポリシーにより、アダプタが取得できない場合の挙動は
  `EVORCH_REQUIRE_ADAPTER` で制御する。
- コマンド:
  -  ignored テスト一括: `cargo test -p gui --tests -- --ignored --nocapture`
  -  証跡マトリクス: `scripts/gui-evidence-matrix.sh [OUT]`
     (デフォルト出力 `target/gui-evidence`、4 状態 x 4 サイズ @1.0 と
     demo / edit-profile @1.5, @2.0 の計 20 PNG を生成し、枚数を検査する)
- 環境変数: `EVORCH_REQUIRE_ADAPTER=1` (CI で必須化)、
  `EVORCH_METADATA_EVIDENCE` (model_metadata 証跡の出力先)、
  `WGPU_BACKEND=vulkan` (lavapipe 利用時の推奨指定)
- CI ジョブ: `offscreen-gate` (アーティファクト `gui-evidence`)

### L5: ブラウザ fake-source テスト

- 役割: Chromium を起動せず、注入したフェイクソースでブラウザペインの
  フレーム描画・アクションログ・DOM diff 証跡を実 egui ラベル経由で検証する。
- コマンド: `cargo test -j 1 -p gui --features browser --test browser -- --test-threads=1`
- 例: `crates/gui/tests/browser.rs::pane_uploads_fake_frame_without_launching_browser`
- 環境変数: なし
- CI ジョブ: `ci` (browser フィーチャの clippy / test ステップ)

### L6: 実 Chromium E2E

- 役割: 実 Chromium のヘッドレスセッションを起動し、localhost フィクスチャに
  対するスクリーンキャストとアクション証跡を検証する。通常実行では ignored。
- コマンド: `cargo test -p gui --features browser --lib browser::tests::chromium_screencast_and_action_evidence -- --ignored --exact --nocapture`
- 環境変数: `EVORCH_BROWSER_EVIDENCE_DIR` (証跡出力先)、
  `CHROME` (chromiumoxide の実行ファイル検出が参照。CI で設定)
- CI ジョブ: `browser-e2e` (アーティファクト `browser-e2e-evidence`)

### L7: 実機起動スモーク (手動)

- 役割: eframe が実ウィンドウを生成し、WM が最小サイズを強制し、アプリが
  正常終了することを、オペレータの実ディスプレイで確認する。
- コマンド: 下記「実機スモーク手順」を参照。
- CI ジョブ: なし (手動のみ、CI では絶対に実行しない)

## 実機スモーク手順 (AC6)

この手順はオペレータの実ディスプレイに実ウィンドウを開く。CI でも
エージェントでも実行してはならない。実行は人間のオペレータが自分の
マシンで行う。

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
   `FocusAgentPane` / `FocusTerminalPane` / `FocusTasksPane` / `SaveLayout` /
   `ResetLayout` のみで、終了用のキーバインドは存在しない。アプリの閉じる
   ボタンを使うか、X11 では `xdotool windowclose <id>` で閉じる。

6. 終了コードが 0 であることを確認する (`echo $?`)。

## カバレッジ表 (AC7)

| 機能 | レイヤー | テスト参照 |
| --- | --- | --- |
| 並列ツール呼び出し | runtime 統合テスト (L1 の前提) | `crates/runtime/tests/parallel_tool_calls.rs::shared_wave_runs_concurrently_exclusive_is_barrier`, `crates/runtime/tests/parallel_tool_calls.rs::results_returned_in_original_call_order`, `crates/runtime/tests/parallel_tool_calls.rs::all_permission_checks_happen_before_any_execution` |
| ストリーミング実行・トランスクリプト | L1, runtime 統合テスト | `crates/runtime/tests/streaming_agent_loop.rs::deltas_published_before_run_completes`, `crates/runtime/tests/streaming_agent_loop.rs::final_context_matches_buffered_semantics_no_double_emit`, `streaming_transcript.rs::interleaved_text_reasoning_deltas_render_in_order`, `streaming_transcript.rs::failed_or_cancelled_stream_keeps_partial_display` |
| ライブ telemetry (tok/s・TTFT・経過時間) | L1, L2 | `telemetry_live.rs::request_started_records_start_and_elapsed_advances`, `telemetry_live.rs::first_token_observed_sets_ttft`, `telemetry_live.rs::provisional_tok_s_replaced_by_final_on_request_completed`, `telemetry_live.rs::completed_metrics_render_as_headless_labels` |
| コスト・キャッシュ表示 | L1 | `telemetry_cost.rs::cache_hit_rate_uses_senpi_formula`, `telemetry_cost.rs::cost_hidden_when_pricing_unknown`, `telemetry_cost.rs::cache_segment_hidden_below_10_percent`, `telemetry_cost.rs::compact_line_format` |
| モデル料金設定 | config 統合テスト (L1 の前提) | `crates/config/tests/pricing.rs::model_entry_cost_fields_roundtrip_via_save`, `crates/config/tests/pricing.rs::static_toml_overrides_catalog_pricing`, `crates/config/tests/pricing.rs::unknown_pricing_returns_none` |
| 自動タイトル | L1, L2 | `auto_title.rs::first_user_message_on_default_title_triggers_rename`, `auto_title.rs::custom_title_never_overwritten`, `auto_title_provider.rs::production_title_uses_quick_route_or_explicit_thread_model` |
| 役割ごとのモデル割り当て | L2, L3, L4, config 統合テスト | `crates/config/tests/save_agent_bindings.rs::save_role_binding_roundtrips_logical_model_and_category_overrides`, `role_settings_headless.rs::role_binding_edit_saves_toml_and_reloads_runtime`, `role_settings_headless.rs::category_override_editable_per_role`, `role_settings_headless.rs::role_settings_geometry_matrix`, `role_settings_headless.rs::capture_role_settings_png_evidence` |
| 自動 write-mode | L1, runtime 接続テスト | `auto_write_mode.rs::submit_autostarts_unowned_thread`, `auto_write_mode.rs::owner_responsive_shows_notice_without_sending`, `auto_write_mode.rs::composer_autostart_reaches_real_runtime`, `auto_write_mode.rs::exists_race_with_other_owner_does_not_return_a_permit` |
| Waiting の既読/未読・確認応答 | L1, L2, L4 | `attention_ack.rs::ack_requires_displayed_focused_and_revision_match`, `attention_ack.rs::stale_revision_ack_is_ignored`, `waiting_badge_headless.rs::unread_waiting_badge_filled_info_accent`, `waiting_badge_headless.rs::read_waiting_badge_outline_only`, `waiting_badge_headless.rs::capture_waiting_read_unread_png_evidence` |
| Codex quota 取得・表示 | L1, L2, L4, providers 統合テスト | `crates/providers/tests/codex_quota.rs::rate_limits_read_parses_stdio_jsonrpc_response`, `crates/providers/tests/codex_quota.rs::wham_fallback_when_app_server_missing`, `codex_quota_display.rs::frame_loop_polls_injected_source_and_displays_quota`, `codex_quota_display.rs::stale_indicator_shown_when_refresh_failed`, `codex_quota_display.rs::capture_codex_quota_png_evidence` |
| コンポーザ `/new` | L1, L2 | `src/model/composer.rs::parse_new_without_args`, `composer_dispatch_headless.rs::new_command_creates_and_switches_thread` |
| サイドバー (プロジェクト/スレッド) | L2, L3 | `sidebar_headless.rs::add_and_select_project_persists_to_sidebar_file`, `sidebar_headless.rs::create_switch_pin_thread_via_ui_clicks`, `sidebar_headless.rs::thread_state_follows_lifecycle_events`, `size_dpi_matrix.rs::demo_geometry_matrix` |
| 会話/トランスクリプト | L1, L2, L4 | `transcript.rs::registry_routes_request_failed_with_run_id_to_thread_and_run`, `burst_transcript.rs::burst_1500_tokens_per_tick_reaches_transcript_without_drops`, `empty_states_headless.rs::conversation_with_messages_hides_placeholders`, `empty_states_headless.rs::capture_cjk_conversation_png_evidence` |
| コンポーザ (送信/スラッシュ/画像) | L2, L4 | `composer_ui_headless.rs::send_button_round_trip_issues_send_chat`, `composer_ui_headless.rs::slash_completion_button_fills_input`, `composer_dispatch_headless.rs::chat_send_issues_send_chat_and_shows_user_line`, `composer_image_input.rs::paste_and_drop_render_real_thumbnails_without_pasting_base64_into_text`, `src/panes/composer_capture_test.rs::capture_cancel_states` |
| プロバイダ設定 (一覧/編集/モデル/Codex 認証) | L2, L3, L4 | `provider_settings_headless.rs::save_valid_settings_writes_evorch_toml_and_flips_status`, `provider_settings/models.rs::type_into_add_model_and_press_enter_adds_row_with_enable_checked`, `provider_settings/tabs.rs::codex_tab_hides_openai_grid_and_shows_login`, `size_dpi_matrix.rs::edit_profile_geometry_matrix`, `provider_settings/capture.rs::capture_codex_auth_png_evidence`, `provider_settings/capture.rs::capture_modal_png_evidence`, `provider_settings/models.rs::capture_model_management`, `provider_settings/tabs.rs::capture_both_settings_tabs` |
| モデルピッカー | L2, L4 | `model_picker_headless.rs::picker_lists_profiles_and_models_from_catalog`, `model_picker_headless.rs::selecting_model_persists_on_thread`, `model_picker_headless.rs::picker_disabled_without_profiles_offers_settings`, `model_picker_headless.rs::capture_model_picker_evidence`, `model_metadata_capture.rs::capture_model_metadata` |
| エージェントグリッド | L2 | `agents_grid_headless.rs::agents_grid_fits_nine_columns_without_horizontal_scroll`, `agents_grid_headless.rs::agents_grid_respects_clip_rect_when_parent_scrolls_horizontally` |
| Diff | L2 | `diff_headless.rs::diff_pane_shows_loading_then_ready_from_fixture`, `diff_headless.rs::empty_diff_shows_explicit_empty_state`, `diff_headless.rs::git_error_is_shown_and_ui_keeps_rendering` |
| Terminal | L2 (レイアウト存在のみ) | `layout_v02.rs::undock_to_floating_and_reload_preserves_v02_panels` (terminal タブのドック操作)。端末コンテンツ描画のテストは gap |
| Tasks / Memory / Arena / Team ペイン | L2 | `workbench.rs::tasks_row_updates_after_state_change_event`, `memory_pane.rs::memory_pane_search_and_filter_use_persisted_projection`, `arena_pane.rs::arena_promotion_requires_a_second_explicit_click`, `team_pane.rs::team_pane_shows_claim_owner_and_ready_tasks` |
| Browser ペイン | L5, L6 | `browser.rs::pane_uploads_fake_frame_without_launching_browser`, `browser.rs::click_displays_action_log_and_dom_diff`, `browser.rs::failed_action_keeps_dom_evidence_visible_across_frames`, `src/browser/tests.rs::chromium_screencast_and_action_evidence` |
| Loop ステータス | L2 | `loop_status_headless.rs::loop_state_tracks_stage_and_rejections_from_bus_events`, `loop_status_headless.rs::state_controls_issue_pause_and_resume_goal_commands_once` |
| テーマ | L2 | `theme_headless.rs::install_applies_dark_design_tokens`, `theme_headless.rs::workbench_installs_theme_on_first_frame`, `theme_headless.rs::dock_style_distinguishes_tab_states` |
| Dock レイアウト | L2 | `dock_roundtrip.rs::workspace_dock_workspace_round_trip_preserves_nested_structure`, `dock_roundtrip.rs::complex_nested_split_round_trip_preserves_tree_and_active_tabs`, `layout_v02.rs::default_layout_is_sidebar_center_right_tabs`, `layout_v02.rs::v1_layout_file_loads_via_migration_into_workbench` |
| Empty ステート | L2, L3, L4 | `empty_states_headless.rs::conversation_without_project_offers_go_to_projects`, `empty_states_headless.rs::composer_is_docked_at_bottom_in_empty_state`, `sidebar_headless.rs::sidebar_without_projects_shows_placeholder_and_single_add_project_cta`, `size_dpi_matrix.rs::empty_geometry_matrix`, `empty_states_headless.rs::capture_empty_composer_evidence` |
| 最小ウィンドウサイズ | L2, L3, L7 | `window_options.rs::native_options_set_layout_sizes_when_created`, `window_options.rs::minimum_fits_default_when_sizes_are_compared`, `size_dpi_matrix.rs::min_size_matches_real_app_viewport`, L7 実機スモーク手順 4 |

テストファイルは特記なき限り `crates/gui/tests/` 配下。

## コマンド・環境変数リファレンス

| 名前 | 種別 | 意味 | 参照レイヤー |
| --- | --- | --- | --- |
| `EVORCH_REQUIRE_ADAPTER` | 環境変数 | `1` でオフスクリーンアダプタ未取得を skip ではなく失敗にする (`gui::evidence::AdapterPolicy`)。CI の offscreen-gate で設定 | L4 |
| `EVORCH_METADATA_EVIDENCE` | 環境変数 | `model_metadata_capture.rs::capture_model_metadata` の証跡出力先ディレクトリ | L4 |
| `EVORCH_BROWSER_EVIDENCE_DIR` | 環境変数 | 実 Chromium E2E の証跡出力先。未設定時は一時ディレクトリ | L6 |
| `CHROME` | 環境変数 | chromiumoxide の実行ファイル検出が参照する Chromium パス。browser-e2e で設定 | L6 |
| `WGPU_BACKEND` | 環境変数 | wgpu のバックエンド指定。lavapipe 環境では `vulkan` を推奨 | L4 |
| `cargo test -p gui --tests -- --ignored --nocapture` | コマンド | ignored オフスクリーンレンダテストの一括スイープ | L4 |
| `scripts/gui-evidence-matrix.sh [OUT]` | スクリプト | 20 PNG のサイズ x DPI 証跡マトリクスを生成し枚数を検査。内部で `headless_capture --size WxH --dpi F --demo/--error-thread/--edit-profile --out` を使用 | L4 |
| `cargo test -j 1 -p gui --features browser --test browser -- --test-threads=1` | コマンド | ブラウザ fake-source テスト | L5 |
| `cargo test -p gui --features browser --lib browser::tests::chromium_screencast_and_action_evidence -- --ignored --exact --nocapture` | コマンド | 実 Chromium E2E | L6 |
| `cargo run -p gui --bin evorch-gui -- --demo --window-title <text>` | コマンド | 実機スモーク起動。既定タイトル `evorch`、最小サイズ 960x600 | L7 |
