# v03 GUI design fixes — headless capture 証跡

issue #87「v0.3: GUI デザイン修正（project 名ボックス過大・状態ドット配置/サイズ）」の before/after スクリーンショット。

## 画像一覧

| 画像 | 状態 | 説明 |
|---|---|---|
| `before-demo.png` | before / demo populated | 修正前。project 行 37px、状態ドット 8px で行内上寄り。thread カードは上段(情報)+下段(Pause ボタン)の 2 段 76px |
| `before-demo-error.png` | before / demo + error | 修正前 + `--error-thread`。先頭 thread が Error(赤ドット)。サイズ・配置の問題は通常版と同一 |
| `after-demo.png` | after / demo populated | 修正後。project/thread 行とも 28px(ROW_DENSE) 単一行、ドット 10px(DOT_SIZE) で行内中央配置 |
| `after-demo-error.png` | after / demo + error | 修正後 + `--error-thread`。先頭 thread の赤ドットが同行中央に拡大配置されていることを確認 |

## 計測サマリ

| 項目 | before | after |
|---|---|---|
| project 行高 | 37px | 29px (ROW_DENSE=28 + 描画丸め) |
| thread 行 | 76px 2 段カード | 28px 単一行 |
| 状態ドット | 8px、テキスト中央より 5-6px 上 | 10px、テキスト中央とのずれ 0-1px |

## 取得手順（再現コマンド）

```sh
# before: commit 12e478f（描画コードは修正前と同一。--error-thread はこの commit で追加）
nix develop -c env WGPU_BACKEND=vulkan cargo run -q -p gui --bin headless_capture -- --demo --out target/shots/before-demo.png
nix develop -c env WGPU_BACKEND=vulkan cargo run -q -p gui --bin headless_capture -- --demo --error-thread --out target/shots/before-demo-error.png

# after: commit b6b846d（修正適用後）
nix develop -c env WGPU_BACKEND=vulkan cargo run -q -p gui --bin headless_capture -- --demo --out target/shots/after-demo.png
nix develop -c env WGPU_BACKEND=vulkan cargo run -q -p gui --bin headless_capture -- --demo --error-thread --out target/shots/after-demo-error.png
```

- レンダラ: wgpu Vulkan / mesa lavapipe（`nix develop` 環境。CI と同一経路: `.github/workflows/ci.yml` の headless capture ジョブ）
- 解像度: 1280x720 PNG（RGBA8）
- before 取得時点 SHA: `12e478f`
- after 取得時点 SHA: `b6b846d`
- ジオメトリの自動検証: `crates/gui/tests/theme_headless.rs` の `compact_row_is_dense_and_centers_status_dot` と `crates/gui/tests/sidebar_headless.rs` の `sidebar_rows_are_single_line_dense_rows` が行高・ドットサイズ・垂直中央揃え・単一行配置を assert する
