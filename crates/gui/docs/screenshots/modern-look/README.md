# Modern look trial — headless capture 証跡

egui のまま「ImGUI 感」を減らす試行 (dock chrome・左揃えリスト行・枠の入れ子解消・Inter + ウェイト階層・Phosphor アイコン・hover アニメーション) の before/after。

## 画像一覧

| 画像 | 状態 | 説明 |
|---|---|---|
| `before-graphite.png` | before / demo populated | 変更前 (main `b400202`)。リスト行が中央揃えのボタン、各ペインに死んだ「×」、会話の各イベントが罫線付きカード |
| `after-graphite.png` | after / demo populated | 変更後。左揃え行 + アイコン操作、ピル型タブ + パネルアイコン、会話は本文直置き + 1 行イベント、メトリクスはアイコン表記 |
| `after-tokyo-night.png` | after / Tokyo Night | 同じ変更を Tokyo Night preset で確認 |

## 取得手順（再現コマンド）

```sh
nix develop -c env WGPU_BACKEND=vulkan cargo run -q -p gui --bin headless_capture -- --demo --size 1600x1000 --out target/shots/before-graphite.png   # main
nix develop -c env WGPU_BACKEND=vulkan cargo run -q -p gui --bin headless_capture -- --demo --size 1600x1000 --out target/shots/after-graphite.png
nix develop -c env WGPU_BACKEND=vulkan cargo run -q -p gui --bin headless_capture -- --demo --theme tokyo-night --size 1600x1000 --out target/shots/after-tokyo-night.png
```

- レンダラ: wgpu Vulkan (`nix develop` 環境)
- 解像度: 1600x1000 PNG
