## Goal

ADR 0008 の v0.1 security 層を `crates/sandbox/` に実装する。①approval 層: tool 実行を policy で分類し auto-allow / ask / deny を適用（ask は GUI/CLI の承認応答を待つ）。②sandbox 層: Linux で dangerous 操作を sandbox 実行し、承認しても sandbox 外では実行不可（二層分離）。③credential 隔離（keychain 優先、0600 平文 JSON fallback。agent / 子プロセス / env へ渡さない）。④network egress 既定 deny（provider endpoint のみ allowlist）。本 packet の最初のタスクで Linux sandbox 第一実装を選択し ADR 0021 として記録する。

## Why This Slice Exists Now

ADR 0008 は sandbox + approval 二層分離・credential 隔離・network egress 既定 deny・制御マーカー エスケープを v0.1 必須として確定済みであり、機能を動かす前に security 境界を作る slice が必須である。既存 harness は prompt injection を「防げない」と公表しており、evorch は OS-level sandbox + approval を持つ Codex 方式を採用する。また tools-sandbox overview の Open question『Linux sandbox の第一実装（Landlock vs bwrap）』がこの slice 冒頭で解消される。

## Current Observed State

Greenfield 状態（Rust コードは未存在。`v01-scaffold` が crate 骨格を作る予定）。approval policy・sandbox 実行・credential 隔離・network deny のいずれも存在しない。tool 実行は v01-tool-layer でそのまま実行される前提であり、security 層が無い状態。

## Accepted Baseline You May Assume

- ADR 0008: v0.1 に sandbox + approval 二層分離 / credential 隔離（keychain 優先、0600 fallback）/ network egress 既定 deny を実装。v0.2 は ContentOrigin / project trust、v0.3 は untrusted mode
- ADR 0009: v0.1 は Linux 先行。macOS（Seatbelt/keychain）は v0.2、Windows は v0.3 以降。crate 構成は OS 抽象層を前提
- Linux sandbox 選択は tools-sandbox overview の Open question のため、本 packet 冒頭で選定し ADR 0021 化（推奨: bwrap）。bwrap は user namespaces + network namespace で egress deny を実現でき、Landlock は filesystem のみで network 制御不可
- `v01-scaffold` / `v01-tool-layer` / `v01-event-stream` が crate 骨格・tool 実行経路・承認要求/応答を流す event 経路を用意済み
- 制御マーカー エスケープは v01-tool-layer の責務（ADR 0008）
- 承認 UI の GUI 実装は v01-gui-panes 側。本 slice は承認要求/応答の API と policy のみ

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/sandbox/`

Target part: 実行ポリシー・sandbox 実行・承認フロー・credential 隔離

## In Scope

- Linux sandbox 第一実装の選択と ADR 0021 化（最初のタスク。推奨: bwrap。bwrap 実行不可環境では fail-closed）
- approval 層: tool 実行の policy 分類（auto-allow / ask / deny）、approval モード（on-request / on-failure / never）、ask の承認要求イベント emit と応答待機
- sandbox 層（bwrap）: fs allowlist / workspace write scope / network deny（network namespace + 既定 deny、provider endpoint のみ allowlist）の OS enforcement。dangerous tool（shell 等）はこの層で実行
- credential 隔離: keyring（keychain）優先、0600 平文 JSON fallback。sandbox 子プロセスの env / filesystem から参照不可
- 二層分離: approval 通過後も sandbox 外の操作は実行しない
- platform 抽象（OS 抽象層。Linux 実装のみ）

## Out Of Scope

- macOS（Seatbelt / keychain）— v0.2、Windows（ConPTY / job object）— v0.3 以降候補（ADR 0009）
- ContentOrigin 型付け / project trust — v0.2（ADR 0008）
- untrusted mode（fs read-only / 一時コピー / network deny / credential 不可 / project 拡張無効）— v0.3
- GUI の承認 UI 実装（v01-gui-panes 側。本 slice は API のみ）
- 制御マーカー エスケープ（v01-tool-layer の責務）

## Standalone Child Issue Contract

`turtton/evorch` に `crates/sandbox/` 配下で、ADR 0008 の v0.1 security 層を実装する。最初のタスクとして Linux sandbox 第一実装（推奨: bubblewrap（bwrap）。network namespace で network egress deny を実現。Landlock は filesystem のみで不採用）を選択し `intents/evorch/decisions/0021-sandbox-linux-bwrap.md` として ADR 記録する。approval 層では tool 実行を policy で分類し auto-allow / ask / deny を適用し、deny は実行せず、ask は承認応答（event stream 経由）を待つ。sandbox 層では dangerous tool（shell 等）を bwrap で実行し fs allowlist / workspace write scope / network deny を適用し、承認しても sandbox 外では実行しない（二層分離）。credential は keychain（keyring）優先・0600 平文 JSON fallback で保存し、agent プロセス・子プロセス・env・sandbox ファイルシステムから参照不能にする。network egress は既定 deny で provider endpoint のみ allowlist。crate 構成は OS 抽象層とし Linux 実装のみ。macOS / Windows / untrusted mode は実装しない。PR は `main` をターゲットにする。

## Acceptance Criteria

- approval 層が tool 実行を policy で分類し、auto-allow / ask / deny を適用。ask は GUI/CLI の承認応答を待ってから実行する
- deny された操作は実行されない（拒否が観測可能なイベントとして emit される）
- credential ファイル・env を sandbox 子プロセスから参照できないことをテストで検証する
- network egress が既定 deny で、provider endpoint のみ allowlist を通ることをテストで検証する
- dangerous 操作が Linux sandbox（bwrap）で実行され、承認しても sandbox 外では実行されない（二層分離）
- Landlock vs bwrap の選択結果が ADR 0021 として記録されている

## Verification

- `cargo test`: policy 分類（auto-allow / ask / deny）と deny 時の非実行を検証
- credential 隔離: sandbox 子プロセスから credential ファイル・env が参照不可であることを test（疑似 credential fixture）
- network deny: allowlist 外 destination への接続遮断を bwrap network namespace で test
- 二層分離: 承認済みでも sandbox 外の操作が実行されないことの test
- `intents/evorch/decisions/0021-sandbox-linux-bwrap.md` の存在と内容を closeout で確認
- `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Related Links

- intents/evorch/features/tools-sandbox/overview.md
- intents/evorch/decisions/0008-threat-model-phased-adoption.md
- intents/evorch/decisions/0009-platform-linux-first-gui-only.md
- intents/evorch/technology/mvp-roadmap.md
- Predecessor: v01-scaffold / v01-tool-layer / v01-event-stream

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs
writeback is expected for this slice. Answer or explicitly decline:

- Intent placement: `features/tools-sandbox` overview（Open question の解消を記録）
- ADR candidate: ADR 0021（Linux v0.1 sandbox 第一実装: bubblewrap（bwrap）採用）
- Diagram candidate: none
- Docs update: none（role-facing surface を追加しないため）
- Closeout writeback expected: yes（ADR 0021 + tools-sandbox overview の Open question 解消）

## Guide Reachability (G645)

While the author still knows the answer, name the guide surface and role that route to every
role-facing surface this slice adds, or explicitly say that no role-facing surface is added. A
blank answer is not treated as no-surface. The closeout record is a debt check, not a merge gate.

この slice は role-facing surface（CLI / GUI / 対話 surface）を追加しない。承認 UI の GUI 実装は v01-gui-panes 側で作られ、本 slice は内部 crate（crates/sandbox/）に承認要求/応答 API を提供するのみであり、`no_role_facing_surface: true` を宣言する。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.