## Goal

provider ストリーム切断・transport 障害に対する回復方針を実装する: 部分表示は保持して
履歴にはコミットしない契約を維持しつつ、bounded retry（既定3回、指数 backoff）を
provider 層に閉じ込め、連敗時は停止してユーザー/オーケストレータへ escalation する。

## Why This Slice Exists Now

2026-09-12 の wave で実証: "stream disconnected before completion" が同一タスクで3連発し、
無条件リトライでは回復しなかった。3連敗で停止して経路を変える（新規 session で完走）のが
有効だった。現状 evorch には transport 障害への体系的な retry 方針がなく、障害時の挙動が
一発勝負になっている。

## Current Observed State

- agent loop（crates/runtime/src/agent_loop.rs）は complete_streaming 経由で provider error を
  受け取る。transport 系失敗に対する retry 機構はない。
- codex quota client（crates/providers/src/provider/codex/quota.rs）に poll backoff clamp
  （30s..10min）の実装例がある。
- T10/T17（2026-09-12）で固定済みの契約: 部分表示と確定履歴は分離、final response の再 emit
  による補正はしない、キャンセル優先の biased select、RequestFailed(Other) 分類。

## Accepted Baseline You May Assume

- **2026-09-12 オペレータ確定**: retry 責務は provider 層に置く。backoff・再試行を providers 層に
  閉じ込め、agent-loop は最終失敗のみ受け取る。部分 delta の重複排除も provider 内で完結させる。
- T10/T17 の契約（部分表示保持・再 emit 補正なし・キャンセル優先）は壊さない。
- SessionSnapshot / event-bus は実装済み（v09-a で run 再開基盤あり）。

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/providers/ crates/runtime/ crates/gui/`

Target part: provider 層の bounded retry（既定3回、指数 backoff、重複 delta 排除）+
連敗時の停止と escalation 表示

## In Scope

- transport 系の失敗（stream 切断・timeout）に対する既定で有効な指数 backoff の bounded retry
  （provider 層実装）
- retry 中の部分表示保持と、retry 成功時の表示の非破壊・非重複（部分 delta の重複排除は
  provider 内で完結）
- 連敗上限到達時の run 停止 + 理由付き型付きエラーでの escalation（GUI に明示表示。
  silent retry loop にしない）
- 認証系エラー（ReauthenticationRequired 等）は retry せず即 escalation
- 回帰テスト: 断→retry→成功で表示が正しい、連敗で停止、認証エラーが retry されない

## Out Of Scope

- agent-loop 層での retry 実装（オペレータ確定により provider 層に閉じ込める）
- 最終失敗後の自動再開（v09-a の session continuation は別経路。本 unit は停止+escalation まで）
- quota/rate limit の backoff 調整（codex quota client の既存実装の領域）

## Standalone Child Issue Contract

evorch の providers 層に「transport 系失敗（stream 切断・timeout）への bounded retry
（既定3回、指数 backoff、部分 delta の重複排除を provider 内で完結）」を実装し、
連敗上限到達時は run を停止して理由付きの型付きエラーで escalation し GUI に明示表示する
機能を追加する。認証系エラーは retry せず即 escalation する。部分表示保持・再 emit 補正なし・
キャンセル優先の既存契約を壊さないことを、断→retry→成功の表示正しさ・連敗停止・認証エラー
非 retry の回帰テストで固定して PR として提出する。

## Acceptance Criteria

- transport 系の失敗（stream 切断・timeout）に対して既定で指数 backoff の bounded retry が効く
- retry 中も部分表示が保持され、retry 成功時に表示が破壊・重複しない
- 連敗上限に達したら run を停止し、理由付きの型付きエラーで escalation（GUI に明示表示）する。
  silent retry loop にならない
- 認証系エラー（ReauthenticationRequired 等）は retry せず即 escalation する
- 回帰テスト: 断→retry→成功で表示が正しいこと、連敗で停止すること、認証エラーが retry されないこと

## Verification

- `cargo test -p providers -p runtime -p gui` 全緑
- 新規回帰テスト: 断→retry→成功の表示、連敗停止、認証エラー非 retry
- `git diff --check`

## Related Links

- ADR 0025: intents/evorch/decisions/0025-agent-experience-evaluation.md
- intents/evorch/features/agent-runtime-kernel/overview.md

## Knowledge Maintenance

- Intent placement: agent-runtime-kernel overview（既存 intent、新規不要）
- ADR candidate: none
- Diagram candidate: none
- Docs update: none
- Closeout writeback expected: no

## Guide Reachability (G645)

- guide surface: `guide workflow task implementation-loop`（role: implementation）
- target surface: retry policy + escalation 表示

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
