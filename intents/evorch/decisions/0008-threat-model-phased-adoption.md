# ADR 0008: 脅威モデル — 対策の段階導入

## Status

Accepted（2026-08-29、grill による全体構想レビューから確定）

## Context

構想書には sandbox（§19）があるが、untrusted コンテンツ（prompt injection、credential 保護、network egress）への言及がなかった。2026-08-29 に OpenCode / pi-mono / senpi / Codex CLI の防御実装を調査した結果：

- OpenCode / pi / senpi はいずれも prompt injection を「防げない」ことを公式に明記。permission/trust は確認 UI またはロード制御であってセキュリティ境界ではない
- pi / senpi の credential 保存は平文 JSON + `0600`（keychain 不使用）。既定 permission は `full-access`
- Codex CLI だけが OS-level sandbox（Seatbelt/Landlock）+ approval の二層構造を持つ

詳細は調査時の比較表（本 ADR の根拠）を features/tools-sandbox/overview.md と併せて参照。

## Decision

防御策7項目を**段階導入**する。

### v0.1 に実装

1. **sandbox + approval 二層分離**（Codex 方式）: ユーザー承認しても sandbox 外は実行不可。approval policy（on-request/on-failure/never）と OS enforcement（fs allowlist / workspace write scope / network deny）を分離
2. **credential 隔離**: agent プロセス・子プロセス・環境変数へ credential を渡さない。保存は keychain 優先、未対応環境は 0600 平文 JSON + 権限
3. **network egress 既定 deny**: provider endpoint のみ allowlist
4. **制御マーカーのエスケープ**: tool result 内の `<system-reminder>` 等の system 構文を無害化。system message と tool result の構文を物理分離

### v0.1 の設計に組み込み、実装は v0.2

5. **ContentOrigin 型**: tool result を `RepositoryUntrusted` / `WebUntrusted` / `ToolTrusted` 等で型付け（retfit が難しいため型は先行設計）
6. **project trust（ロード制御）**: `AGENTS.md` / skills / MCP 設定 / project 拡張を未承認時はロードしない。pi/senpi の弱点（trust 解決前でも context file を読む）は避ける

### v0.3

7. **untrusted mode**: fs read-only / 一時コピー書き込み / network deny / credential 不可 / project 拡張無効の統合モード

これにより mvp-roadmap の sandbox は v0.2 から v0.1 へ前倒し（後述 Consequences 参照）。

## Consequences

- mvp-roadmap v0.1 に sandbox / credential 隔離 / network deny / marker エスケープを追加、v0.2 の sandbox 項目は ContentOrigin 実装 + project trust に置き換え
- tools-sandbox feature の acceptance criteria に二層分離と credential 非露出を追加
- 既存 harness が「防げない」とする injection については「低減するが根除しない」ことを product/overview.md の non-goals に明記
