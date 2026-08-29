# ADR 0015: 検証運用 — CI mock 契約テスト + 定期実 API 検証の2層

## Status

Accepted（2026-08-29、grill による全体構想レビューから確定）

## Context

evorch は provider / cache / compaction / sandbox など外部依存が多いハーネスであり、feature overview に書かれた受け入れ基準を「どう証明するか」の運用設計が必要だった。

## Decision

受け入れ基準の証明は2層で運用する。

### 第1層: 継続的検証（CI、常時）

- **mock provider + 録画応答（recorded response fixture）** で契約テストを実行
- 対象: メッセージ変換、tool 呼び出し回線、event stream 形式、compaction、routing 解決、sandbox policy 適用、ADRs の構造的部分
- 外部 API への実アクセスは CI では行わない（料金・レート制限・障害による不安定化を防ぐ）

### 第2層: 定期実 API 検証（リリース前・週次・手動実行の matrix）

- 実 provider profile での end-to-end 確認。サブスクリプション系（anthropic-subscription / openai-codex / github-copilot）の実認証フロー検証を含む
- 実行結果は intent tree の verification 記録として残す（`operations/verification/` または各 feature の記録節。配置は v0.1 実装時に確定）

## Consequences

- mock provider の fixture 形式は v0.1 実装時に仕様化（recorded response の正規化・機密情報除去）
- 週次実 API 検証は GitHub Actions の schedule またはローカル運用から開始
