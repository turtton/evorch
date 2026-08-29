# ADR 0011: リリース形態と Diagnostics Issue 化の運用方針

## Status

Accepted（2026-08-29、grill による全体構想レビューから確定）

## Context

grill backlog の「リリース・更新形態」「Diagnostics の Issue 化先」の2問に対するオペレータの決定。

## Decision

### リリース形態

- **nix flake が前提**（オペレータの開発環境）。flake によるビルド・実行を第一の導入経路とする
- バイナリ配布は **GitHub Releases**（`gh release` 運用）。v0.1 ではこれ以外の配布チャネルは用意しない
- バージョニングは semver。自動更新機能は持たず CHANGELOG で管理
- 将来的な Homebrew / cargo install 等は必要になった時点で追加

### Diagnostics Issue 化

- harness bug と判断された診断の GitHub Issue 化は **送信前にユーザー確認を必須**とする（誤送信・情報漏洩リスクの防止）
- 設定で **無効化可能**にする（config 項目 `diagnostics.auto_issue.enabled = false` 相当。設定アーキテクチャ確定後に正式定義）
- デフォルトの Issue 化先は evorch リポジトリ。ユーザー指定 repo は設定で変更可能（同上、設定確定後に定義）

## Consequences

- 設定アーキテクチャの backlog に「diagnostics / issue_target 設定項目」が追加される
- 自己改善 loop（ADR 0006 系）の Issue 化ステップに「operator 承認ゲート」が入る
