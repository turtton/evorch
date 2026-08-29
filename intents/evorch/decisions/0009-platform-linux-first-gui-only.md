# ADR 0009: v0.1 プラットフォーム Linux 先行・クライアント形態

## Status

Accepted（2026-08-29、grill による全体構想レビューから確定）

## Context

grill backlog の「プラットフォーム優先度」「headless/CLI 範囲」の2問に対し、オペレータとの Q&A で以下の議論があった。

- プラットフォーム: macOS / Linux / Windows のいずれを v0.1 の検証基盤にするか。開発環境が Linux のため検証効率が良い点、keychain 連携の成熟度で macOS が有利な点のトレードオフ
- クライアント形態: 最終ゴールは GUI だが、agent による自己改善のために TUI の方が都合が良いのでは、という仮説があった

## Decision

### プラットフォーム: Linux 先行

- v0.1 の検証・開発基盤は **Linux**。sandbox（Landlock / seccomp / bwrap）と PTY が最初に実装される対象とする
- macOS（Seatbelt / Keychain）は v0.2 で追従、Windows（ConPTY / job object）は v0.3 以降の候補
- ただし crate 構成は OS 抽象層を前提とし、Linux 固有実装の分離を v0.1 から徹底する

### クライアント形態: GUI のみ公式、offscreen 構成、TUI は見送り

- 人間向け公式クライアントは **GUI（egui）のみ**。TUI クライアントは製品として作らない（必要になった時点で再検討）
- 自己改善（§25）の agent I/F は Semantic UI API（Workspace Model 直接操作）であり renderer を経由しないため、TUI は不要と判断
- capture_ui / replay_interaction / spawn_test_instance は **GUI の offscreen レンダリング + UI Event Bus へのイベント注入**で実現する。egui の surface を offscreen render 可能にする抽象を v0.1 の初期設計に含める
- 開発者向けにイベントストリーム観測・session 一覧程度の **debug CLI** は v0.1 並行で任意に用意する（公式製品ではなく開発ツール扱い）

## Consequences

- gui-workbench の受け入れ基準に「offscreen レンダリングによるヘッドレス起動」を追加
- 将来 TUI が必要になった場合は Workspace Model の第二 renderer としての drift コストを支払う前提で再検討する
- mvp-roadmap の Open questions から「headless/CLI 範囲」が解消
