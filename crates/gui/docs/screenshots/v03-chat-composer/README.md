# v03 GUI chat composer — headless capture 証跡

issue #91「v0.3: GUI chat composer（senpi 踏襲の通常チャット + スラッシュコマンド）」の before/after スクリーンショット。

## 画像一覧

| 画像 | 状態 | 説明 |
|---|---|---|
| `before-demo.png` | before / demo populated | 修正前。Conversation フッターは placeholder のみ（`Goal-driven — compose in the Goal panel`）。入力欄なし |
| `after-demo.png` | after / demo populated（既定 = provider 未設定） | composer（`Message or /command` hint + Send）常設。provider 未設定なので案内行（設定への誘導 + `/goal` と `/help` は使用可）を composer 直上に表示 |
| `after-demo-provider.png` | after / demo + `--provider-configured` | provider 設定済みの見かけ。案内行なしで入力欄がそのまま使える状態 |
| `after-demo-error.png` | after / demo + `--error-thread` | エラー thread 表示と composer の併存を確認 |
| `after-demo-pointer.png` | after / demo + `--provider-configured --pointer 640 680` | composer 領域への hover 状態を確認 |

## 取得手順（再現コマンド）

```sh
# before: commit c6d040b（composer 導入前）
nix develop -c env WGPU_BACKEND=vulkan cargo run -q -p gui --bin headless_capture -- --demo --out target/shots/before-demo.png

# after: 本 slice の HEAD
nix develop -c env WGPU_BACKEND=vulkan cargo run -q -p gui --bin headless_capture -- --demo --out target/shots/after-demo.png
nix develop -c env WGPU_BACKEND=vulkan cargo run -q -p gui --bin headless_capture -- --demo --provider-configured --out target/shots/after-demo-provider.png
nix develop -c env WGPU_BACKEND=vulkan cargo run -q -p gui --bin headless_capture -- --demo --error-thread --out target/shots/after-demo-error.png
nix develop -c env WGPU_BACKEND=vulkan cargo run -q -p gui --bin headless_capture -- --demo --provider-configured --pointer 640 680 --out target/shots/after-demo-pointer.png
```

- レンダラ: wgpu Vulkan / mesa lavapipe（`nix develop` 環境。CI と同一経路: `.github/workflows/ci.yml` の headless capture ジョブ）
- 解像度: 1280x720 PNG（RGBA8）
- before 取得時点 SHA: `c6d040b`
- composer の動作は `crates/gui/tests/composer_dispatch_headless.rs`（送信・`/goal`・`/help`・provider 未設定ガード等の dispatch）と `crates/gui/tests/composer_ui_headless.rs`（pane 統合・Send ボタン・補完ボタン）が headless で assert する
