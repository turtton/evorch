# v03 GUI design refinement — headless capture 証跡

issue #81「v0.3: GUI デザインシステム整備（t3code 風ダークテーマへの洗練）」の before/after スクリーンショット。

## 画像一覧

| 画像 | 状態 | 説明 |
|---|---|---|
| `before-empty.png` | before / 空状態 | テーマ適用前の既定 egui ダーク。project 未追加 |
| `before-demo.png` | before / demo populated | テーマ適用前。`--demo` fixture による populated state |
| `after-empty.png` | after / 空状態 | テーマ適用後。placeholder + CTA |
| `after-demo.png` | after / demo populated | テーマ適用後。状態ドット・選択ハイライト・タブアクセント（`• Agents` / `• Merge` amber） |
| `after-demo-merge.png` | after / demo populated | `--activate merge-main`。Merge タブの amber `•` アテンションと merge 承認 pane（PR #81 / ci / reviewer バッジ） |

## 取得手順（再現コマンド）

```sh
# before: commit 6e9ddcb (gui(headless_capture): add --demo populated capture mode)
# のツリーから取得（テーマ未適用・--demo 実装済みの時点）
nix develop -c env WGPU_BACKEND=vulkan cargo run -q -p gui --bin headless_capture -- --out target/before-empty.png
nix develop -c env WGPU_BACKEND=vulkan cargo run -q -p gui --bin headless_capture -- --demo --out target/before-demo.png

# after: テーマ適用完了後の HEAD から同コマンドで取得
nix develop -c env WGPU_BACKEND=vulkan cargo run -q -p gui --bin headless_capture -- --out target/after-empty.png
nix develop -c env WGPU_BACKEND=vulkan cargo run -q -p gui --bin headless_capture -- --demo --out target/after-demo.png
nix develop -c env WGPU_BACKEND=vulkan cargo run -q -p gui --bin headless_capture -- --demo --activate merge-main --out target/after-demo-merge.png
```

- レンダラ: wgpu Vulkan / mesa lavapipe（`nix develop` 環境。CI と同一経路: `.github/workflows/ci.yml` の headless capture ジョブ）
- 解像度: 1280x720 PNG（RGBA8）
- before 取得時点 SHA: `6e9ddcb`
- after 取得時点 SHA: `8ac6d52`
- タブアテンションの `•` は U+2022（egui 同梱フォントに U+25CF がなく tofu 化するため）
