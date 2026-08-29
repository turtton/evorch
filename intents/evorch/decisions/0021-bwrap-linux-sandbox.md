# ADR 0021: Linux v0.1 sandbox 第一実装に bubblewrap（bwrap）を採用する

## Status

Accepted（2026-08-30、PR #15 で実装確定。issue #6 / v01-sandbox-approval）

## Context

ADR 0008 は v0.1 に sandbox + approval 二層分離・credential 隔離・network egress 既定 deny を必須としたが、Linux sandbox の第一実装（Landlock vs bwrap）が tools-sandbox overview の Open question として残っていた。egress deny を含む隔離境界の土台選択は後続 slice（agent-roles / routing-profiles）の security 前提となるため、本 slice 冒頭で選定する。

## Decision

**bubblewrap（bwrap）0.11.2 を Linux v0.1 の第一実装として採用する。** Landlock は不採用。

### 選定根拠

- **network 隔離**: bwrap は user namespaces + network namespace（`--unshare-net`）で egress deny を強制可能。Landlock は filesystem 保護中心で、network namespace の代替にならず egress deny を実現できない（v0.1 必須要件に対して不十分）
- **rootless 実働**: user namespaces 前提で非 root 動作を本環境で実測確認（user ns + netns で外部接続 unreachable）
- **fail-closed**: bwrap の detect に失敗（実行ファイル不在・機能確認失敗）した場合は `BwrapUnavailable` を返し、危険ツールは実行されない。隔離なしでのフォールバック実行経路は存在しない

### 承認ポリシー（approval 層）

Capabilities 入力で classify（明示 override 最優先・読取のみ AutoAllow・それ以外 Ask）。ApprovalMode × resolve を pure な表で定義。`ApprovalGate` は `ApprovalRequested` を emit し、同一 call_id の `ApprovalResolved` を event stream で待機。timeout・ゲート未設定・Never はいずれも fail-closed で拒否する。

### 二層分離

承認層は `ToolExecutor`、隔離層は shell / git_diff が `Sandbox::wrap` 経由でのみ spawn（生経路なし）。`DirectSandbox` は明示的な opt-out としてのみ存在する。RecordingSandbox（テスト用）と実 bwrap の両方で「承認を通過しても隔離なしには実行されない」ことを証明済み（`two_layer.rs` テスト）。

### credential 隔離

`CredentialStore` trait で keychain 優先（keyring crate、**feature ゲートで default OFF**、vendored libdbus、sentinel プローブでフォールバック判定）、0600 平文 JSON フォールバック（0700 dir、NamedTempFile による原子書換）。子プロセスへの env 注入は allowlist（PATH / TERM / LANG / LC_ALL + extra_env）のみで親 env を非継承とし、#5 で残っていた shell.rs の env 漏えい経路を解消した。agent / 子プロセス / env への credential 非注入は構造で保証される。

### network egress deny

bwrap の netns は all-or-nothing のため、v0.1 は default-deny（`--unshare-net`）を固定とする。`NetworkPolicy`（`deny_all` / `providers_only` / `with_host` / `is_allowed`）が allowlist の表現を担う。per-endpoint 通過制御にはプロキシが必要だが、これは v0.1 スコープ外として明記する（allowlist 適用は v01-routing-profiles 以降の課題）。

### OS 抽象層

`crates/sandbox` は `Sandbox` trait + `CommandSpec` / `WrappedCommand` + bwrap 実装の構成。macOS（Seatbelt / keychain）は v0.2、Windows は v0.3 以降に trait 実装として追加する（ADR 0009 整合）。bwrap argv は `--ro-bind / /`（workspace 外への影響を元の filesystem 配置で抑制）+ `--tmpfs /tmp` + `--dir /tmp/home` + `--chdir` workspace + `--die-with-parent`。

### イベント互換

`ToolEvent` 内 variant 追加（`ApprovalRequested` / `ApprovalResolved` 等）は後方互換で、storage の projection は no-op アームの追加で無傷。event-sourced 観測経路（ADR 0017）に乗る。

## Consequences

- v0.1 の security 境界（approval 二層・credential 隔離・egress deny）がコードとして成立。bwrap 非利用環境では危険ツールは実行不能になる（fail-closed の意図した挙動）
- bwrap 統合テスト（資格情報隔離・別 netns 接続拒否・workspace 書込スコープ）は本環境 0.11.2 実働で 3/3 pass。CI（ubuntu-latest）では skip、fail-closed 単体検証は常時実行
- per-endpoint allowlist の強制（providers_only の実適用）は v0.1 では未実装 — v01-routing-profiles で provider endpoint 解決と接続する設計課題として残る
- macOS / Windows 対応は trait 実装追加で吸収可能な構造だが、Seatbelt / ConPTY の機能差（egress deny の実現方式）は実装時に再検証が必要
