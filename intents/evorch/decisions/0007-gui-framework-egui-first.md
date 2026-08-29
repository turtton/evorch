# ADR 0007: GUI 第一候補を egui + egui_dock に、Floem を評価用 prototype に限定

## Status

Accepted（2026-08-29、構想書 §22 の順位を更新）

## Context

構想書 §22 は「Floem を第一候補に docking prototype を作り、難しければ egui + egui_dock に切り替え」としていた。2026-08 の再評価調査で以下が判明した。

- Floem の安定版は v0.2.0（2024-11）のまま pre-1.0 で、汎用 dock API を提供していない。実績の Lapce も docking を自前実装しており、2026-06 時点でも dock 間 panel 移動の panic issue が存在する
- 一方 `anhosh/egui_dock` は 0.21.1（2026-08-06）まで活発にリリースされ、tab 移動/resize/undock/floating window、DockEvent による layout persistence を標準提供する
- どちらを選んでも構想書 §23 の「GUI framework と Workspace Model の分離」により不可逆にならない

## Decision

- v0.1 の GUI 第一候補は **egui + egui_dock** とする
- Floem は「docking 評価用 prototype」として限定利用する（mouse UX / dock-undock / multi-window / large transcripts の評価は構想書どおり実施）
- **GPUI + gpui-component** を長期候補として watch する
- Workspace Model（Split / Tabs / Panel / Floating / Window の framework-independent な保持）は変更不要

## Consequences

- 大容量 transcript は行単位 chunking + virtualization の自前 widget が必須（Floem でも同様）。この widget は framework 非依存の設計にし、将来の Floem / GPUI 切り替えを可能にする
- egui_dock の binary split 制約（3分割以上・grid は非標準）を受け入れる。初期画面の3ペイン構成は nested split で実現する
- Floem 側の docking prototype 評価が良好だった場合の切り替え経路は Workspace Model の分離により維持される
