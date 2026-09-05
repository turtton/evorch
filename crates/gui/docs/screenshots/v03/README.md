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
| `after-demo-hover.png` | after / demo populated | `--pointer 1175 22` で Goal タブを hover。hover 状態（明るい文字色・背景）を検証 |

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
# Goal タブ中心 (1175, 22) にポインタを置いて hover 状態を capture（座標は after-demo.png から計測）
nix develop -c env WGPU_BACKEND=vulkan cargo run -q -p gui --bin headless_capture -- --demo --pointer 1175 22 --out target/after-demo-hover.png
```

- レンダラ: wgpu Vulkan / mesa lavapipe（`nix develop` 環境。CI と同一経路: `.github/workflows/ci.yml` の headless capture ジョブ）
- 解像度: 1280x720 PNG（RGBA8）
- before 取得時点 SHA: `6e9ddcb`
- after 取得時点 SHA: `8ac6d52`（after-empty / after-demo-hover は `37c3b0b` で再取得: 空状態の空 header strip 除去と hover capture 追加のため）
- タブアテンションの `•` は U+2022（egui 同梱フォントに U+25CF がなく tofu 化するため）
