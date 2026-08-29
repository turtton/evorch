# v01-sandbox-approval Implementation Packet

## Goal

ADR 0008 の v0.1 security 層を `crates/sandbox/` に実装する。①approval 層: tool 実行を policy で分類し auto-allow / ask / deny を適用。ask は GUI/CLI の承認応答（event stream 経由）を待ってから実行。②sandbox 層: Linux で dangerous 操作を sandbox 実行し、承認しても sandbox 外では実行不可（二層分離）。さらに credential 隔離（keychain 優先、0600 平文 JSON fallback。agent / 子プロセス / env へ credential を渡さない）と network egress 既定 deny（provider endpoint のみ allowlist）を実装する。本 packet の最初のタスクで Linux sandbox 第一実装を選択（推奨: bwrap）し ADR 0017 として記録する。

## Why

ADR 0008（2026-08-29 確定）は sandbox + approval 二層分離・credential 隔離・network egress 既定 deny・制御マーカー エスケープを v0.1 必須と定めており、ソフトウェアとして動かす前に security 境界を作る slice が必須である。既存 harness（OpenCode / pi / senpi）は prompt injection を「防げない」と公表しており、evorch は OS-level sandbox + approval を持つ Codex 方式を採用する。また tools-sandbox overview の Open question『Linux sandbox の第一実装（Landlock vs bwrap）』が本 slice 冒頭で解消される。

## Scope

- **Linux sandbox 第一実装の選択（最初のタスク）**: Landlock vs bwrap の比較検討と ADR 0017 化。bwrap（bubblewrap）を推奨（user namespaces + network namespace で egress deny を実現でき、ADR 0008 の「network egress 既定 deny」に対応しやすい。Landlock は filesystem アクセス制御のみで network 制御ができない）。bwrap が実行不可の環境（非 root / kernel 制限）では fail-closed とする方針も ADR で記録
- **approval 層**: tool 実行を policy で分類（auto-allow / ask / deny）。approval モード（on-request / on-failure / never、ADR 0008）。ask は承認要求イベントの emit と応答の待機を含む
- **sandbox 層（bwrap）**: fs allowlist / workspace write scope / network deny（network namespace + 既定 deny、provider endpoint のみ allowlist）の OS enforcement。shell 等の dangerous tool はこの層で実行
- **credential 隔離**: 保存は keychain（keyring）優先、未対応環境は 0600 平文 JSON。sandbox 子プロセスの env / filesystem から credential を参照不可能にする
- **二層分離の順序**: approval（ユーザー承認）を通っても sandbox 外の操作は実行しない
- **platform 抽象**: crate 構成は OS 抽象層を前提（ADR 0009）。v0.1 は Linux 実装のみ

## Out of scope

- macOS（Seatbelt / keychain）— v0.2（ADR 0009）
- Windows（ConPTY / job object）— v0.3 以降の候補（ADR 0009）
- ContentOrigin 型付け / project trust（ADR 0008 v0.2 実装）
- untrusted mode（fs read-only / 一時コピー / network deny / credential 不可 / project 拡張無効の統合モード）— v0.3
- export to sandbox CLI のGUI設定画面（v01-gui-panes / config 側の別 slice）

## Verification

- `cargo test`: policy 分類（auto-allow / ask / deny）と deny 時に実行されないことの検証
- credential 隔離: sandbox 子プロセスから credential ファイル・env が参照できないことを test で検証（fixture: 疑似 credential を用意して参照不可を assert）
- network deny: allowlist 外の destination への接続が遮断されることを bwrap network namespace 上で test
- 二層分離: 承認済みでも sandbox 外の操作が実行されないことの test
- ADR 0017 の存在（`intents/evorch/decisions/0017-sandbox-linux-bwrap.md`）を closeout で確認
- `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: `features/tools-sandbox` overview を primary intent とする。範囲外の OS（macOS / Windows）は ADR 0009、対策自体は ADR 0008 で既定済み
- ADR candidate: **あり** — Linux sandbox 第一実装の選択（Landlock vs bwrap）は後戻りしにくい決定で、本 packet 冒頭の必須タスクとして ADR 0017 に記録する
- Diagram candidate: decline — 二層分離の概念は overview / ADR の記述で十分。図の変更は不要
- Docs update: decline — 本 slice は role-facing surface を持たない（承認 UI は GUI 側の別 slice）
- Closeout learning: ADR 0017 の新設と tools-sandbox overview の Open question 解消を write back する。`write_back_required: true`

- Guide reachability (G645): 本 slice は内部 crate（crates/sandbox/）を変更する。承認 UI 自体は v01-gui-panes 側で作られ、本 slice は承認要求/応答の API のみ提供するため role-facing guide surface を追加しない。`no_role_facing_surface: true` を宣言する

`improve` (G456 / G460) は later safety net。packet-time で上記を宣言済み。