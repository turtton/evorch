# v03 follow-up cleanups — agents pane headless capture 証跡

issue #96「v0.3: follow-up cleanups（agents グリッドの列自動調整）」の before/after スクリーンショット。

## 画像一覧

| 画像 | 状態 | 説明 |
|---|---|---|
| `before-agents.png` | before / demo agents pane | 修正前。`run`/`name`/`role` ヘッダーまでしか読めず、`phase` 以降の右側列がペイン幅を超えてクリップされている |
| `after-agents.png` | after / demo agents pane | 修正後。約 340px の右ドックペイン内に 9 列すべてが省略記号（`pha...` 等）付きで収まる。水平スクロールバーなし。tokens 値セル (`0 / 0`) も可視 |

## 計測サマリ

| 項目 | before | after（検証済み） |
|---|---|---|
| 列数と表示 | 3 列 (`run`/`name`/`role`) のみ可読。`phase` 以降はクリップ | 9 列すべて (`run`/`name`/`role`/`phase`/`model`/`provider`/`current tool`/`tokens (in/out)`) が省略記号付きでペイン幅内に表示 |
| 水平スクロールバー | 不要な水平スクロールが発生 | なし（幅超過時のみ縮退フォールバックとして出現する設計） |
| トークン数セル | 数値が表示されない | 各 row の in/out トークン数が可視 |

## 取得手順（再現コマンド）

```sh
# before: commit 922a046（修正前）
nix develop -c env WGPU_BACKEND=vulkan cargo run -q -p gui --bin headless_capture -- --demo --activate agents-main --out crates/gui/docs/screenshots/v03-followup-cleanups/before-agents.png

# after: commit 7d98110（T3 修正適用後）
nix develop -c env WGPU_BACKEND=vulkan cargo run -q -p gui --bin headless_capture -- --demo --activate agents-main --out crates/gui/docs/screenshots/v03-followup-cleanups/after-agents.png
```

- レンダラ: wgpu Vulkan / mesa lavapipe（`nix develop` 環境。CI と同一経路）
- 解像度: 1280x720 PNG（RGBA8）
- before 取得時点 SHA: `922a046`
- after 取得時点 SHA: `7d98110`（コミット時に orchestrator が埋める）
- ジオメトリの自動検証: `crates/gui/tests/agents_grid_headless.rs` が `AGENTS_COL_MIN=56` / `AGENTS_COL_MAX=160` / `CELL_PAD_X=8` による auto-fit 列幅を assert する（480px 全列収容テスト + 実障害を再現する 340px 外側 ScrollArea 回帰テストの 2 本）

## 検証結果

修正版の `after-agents.png` を目視検証した結果、意図した状態を満たすことを確認した（orchestrator による再確認済み）。

- Agents ペイン実表示幅は約 340px（1280x720 demo レイアウトの右ドック）。この狭いペイン内で 9 列すべてが省略記号付きで表示される
- 各ヘッダー: `run` / `name` / `role` は全文表示、`phase`→`pha...`、`model`→`mo...`、`provider`→`pro...`、`current tool`→`cur...`、`tokens (in/out)`→`tok...` と省略記号による折り畳みでペイン内に収容。右端での省略記号なしの切断はなし
- tokens 値セルが可視（値は行の usage に追従。非ゼロ行は省略記号を含みうる）。水平スクロールバーは現れない
- 初回の after キャプチャではペインの可視幅を超えた幅計測（非有界幅の祖先に対し `available_width` が実可視幅より大きく返る）により右側列がクリップされる障害があったが、可視幅（clip rect）基準の計測へ修正し、MIN floor を下回る最終比例縮小を加えて解消した。回帰防止は上記 340px 外側 ScrollArea テストが担保する
