# ADR 0014: 設定アーキテクチャ — TOML・マルチソース・versioned schema

## Status

Accepted（2026-08-29、grill による全体構想レビューから確定）

## Context

構想書に設定アーキテクチャの言及が皆無だった。grill backlog の「設定まわり」として追加し、配置場所・形式・schema・nix 連携を確定した。

## Decision

### 配置と優先順位（高→低）

```
CLI 引数 / 環境変数
  > project config（./evorch.toml）
  > user config（~/.config/evorch/config.toml、XDG 準拠）
  > builtin defaults（コンパイル時）
```

project config は git 管理可能でチーム共有できる。

### 形式と schema

- **TOML + serde typed schema**。構想書 §8.1 の provider TOML 想定と一貫
- **version フィールド** を各 config に持たせ、マイグレーション関数で将来の schema 変更に対応（OpenCode V2 の V1→V2 破壊の教訓。ADR 0010-6）
- **JSON Schema 生成**: schemars で config 構造体から生成し公開。ユーザー config の先頭に `$schema` 一行（taplo / Even Better TOML が補完・検証に対応）でエディタ補完を提供
- **credential は config に書かない**（ADR 0008: keychain 側。config には profile 名の参照のみ）

### nix / home-manager 連携

- loader を **マルチソース deep merge** にする: `~/.config/evorch/config.d/*.toml` を辞書順にロードして merge + `config.toml` 本体。後勝ち
- home-manager は `config.d/00-nix-generated.toml` を生成（00 プレフィックスで優先度最低固定）。ユーザー手編集は `config.d/50-*.toml` 等で上書き。宣言的生成と手編集の衝突をファイル分割で可視化
- home-manager module（programs.evorch）自体は v0.2 以降で正式提供。v0.1 で必要なのは loader の merge 機構のみ（これがあればユーザーが独自 module を書ける）

### config も domain transform 対象

ADR 0010 整合: config 由来の定義（agent override 等）は core の registry に transform として流れる。builtin と外部の差を作らない。

### v0.1 で公開する設定領域

provider profiles / model routing / panel layout・keybind / diagnostics（auto_issue 無効化・issue 先）/ permission preset / 計測（cache 閾値・metrics 保持期間・panel 表示設定）

## Consequences

- architecture.md の crate 構成に config 層を追加
- operations/ に config リファレンスの配置予定（v0.1 実装時）
