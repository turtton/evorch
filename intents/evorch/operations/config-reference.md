# evorch config reference（v0.1）

設定は TOML。マルチソース・deep merge・version migration（ADR 0014）に基づく。
本書は `crates/config` の実装を正本とする利用者向けリファレンス。

## ソース層と優先順位（低い方から上書きされる）

1. **組み込み既定値**（`Config::default()`。全セクション省略可）
2. **ユーザ層**: `$XDG_CONFIG_HOME/evorch/config.toml`（未設定時は `~/.config/evorch/config.toml`）+ 同ディレクトリの `config.d/*.toml` drop-in
3. **プロジェクト層**: `<project_dir>/evorch.toml` + `<project_dir>/config.d/*.toml` drop-in（`LoadOptions.project_dir` で有効化）
4. **環境変数層**（`crates/config/src/env.rs` 経由）
5. **CLI 上書き**（最優先。`LoadOptions.cli_overrides`）

drop-in (`config.d/*.toml`) は deep merge（`merge.rs` の `deep_merge`）。同名キーは後に読んだ層が上書きし、テーブルは再帰的に統合される。

## ルート構造 (`Config`)

```toml
version = 2          # スキーマバージョン（現在 2。ADR 0014。大きい/欠落は migration 経由）

[providers.<profile-name>]   # マップキーがプロファイル名
[routing]
[panel]
[diagnostics]
[permissions]
[metrics]
```

未知キー・typo キーはロード時に拒否される。拒否は parse error になり、`diagnotics`（ルート直下の typo）、`diagnostics.log_lvl`、`providers.foo.timeout` のような dotted config path を含む。strictness は全ソース層（組み込み既定値 / ユーザ / プロジェクト / drop-in / 環境変数 / CLI 上書き）に一様に適用される。検証は deep merge と version migration の後のマージ済み値に対して走るため、どの層由来のキーでも同じエラーになる。

任意キーを許容するマップは `providers` のプロファイル名、`routing.routes` の route 名、`panel.keybinds` のキーのみ。これら以外のテーブルに定義済み以外のキーを書くとエラーになる。

version 管理 migration は起動時に適用（`migrate.rs`。古い version は現在値へ変換、未来の version はエラー）。

## providers（`ProviderProfileConfig`）

| キー | 型 | 説明 |
|---|---|---|
| `provider_type` | enum | anthropic / openai / openai-compatible 等（ADR 0004 の type） |
| `api_protocol` | enum | anthropic-messages / openai-chat 等（profile が protocol を選択） |
| `base_url` | string | エンドポイント（openai-compatible はここで切替） |
| `credential` | enum | **参照のみ。平文値は書かない**（ADR 0008） |
| `models` | string[] | この profile が提供するモデル id |
| `default_model` | string | 省略時の既定モデル |

### credential (`CredentialRefConfig`、`type` タグ付き、kebab-case)

```toml
# OS キーリングから取得（優先）
credential = { type = "keyring", service = "evorch", account = "anthropic-personal" }
# 環境変数から取得
credential = { type = "env", var = "ANTHROPIC_API_KEY" }
```

### 平文 credential の明示拒否

ADR 0014 どおり credential は config に書かない。`providers.<profile>` 直下、およびその `credential` テーブル内に `api_key` / `api-key` / `token` / `secret` / `password` / `credential_value` などの credential-like なキーを書くと拒否される。キー名の照合は大文字小文字を区別せず、`-` と `_` は同一視される（例: `API_Key`・`api-KEY` も一致）。

```toml
[providers.foo]
api_key = "sk-..."   # 拒否される
```

拒否時のエラーには config path（`providers.<profile>.<field>`）と remediation 案内が含まれる。以下のいずれかの参照形式に置き換えること:

```toml
credential = { type = "keyring", service = "evorch", account = "..." }
credential = { type = "env", var = "..." }
```

## routing（`RoutingConfig`）

Logical route → 候補 profile の順序付きリスト。fallback は先頭から試行。

```toml
[routing.routes]
claude-main = [
  { profile = "claude-business", model = "claude-sonnet-4-5" },
  { profile = "claude-personal" },   # model 省略時は profile の default_model
]
```

| キー | 型 | 説明 |
|---|---|---|
| `routes.<name>` | `RouteCandidateConfig[]` | `profile`（必須）+ `model`（任意）の候補順リスト |

## panel（`PanelConfig`）

| キー | 型 | 説明 |
|---|---|---|
| `layout` | string | パネル配置プリセット名 |
| `keybinds` | map | キー → コマンド名 |

## diagnostics（`DiagnosticsConfig`）

| キー | 型 | 説明 |
|---|---|---|
| `log_level` | string | tracing レベル（debug/info/warn/error） |
| `log_dir` | string? | ログ出力先ディレクトリ（省略時は既定） |

## permissions（`PermissionConfig`）

| キー | 型 | 説明 |
|---|---|---|
| `preset` | string | approval 既定ポリシー（auto-allow / ask / deny の基準。v01-sandbox-approval） |

## metrics（`MetricsConfig`）

| キー | 型 | 説明 |
|---|---|---|
| `enabled` | bool | downsampled metrics 記録の有効化（ADR 0012） |
| `retention_days` | u32 | 保持日数 |

## JSON Schema

`Config` から `schemars` で自動生成した JSON Schema を versioned artifact として公開している（ADR 0014）。

- **公開 artifact**: [`docs/config/evorch-config-v2.schema.json`](../../../docs/config/evorch-config-v2.schema.json)。ファイル名の `v{n}` は `Config` の `CURRENT_VERSION` に対応する（version bump 時は新しい `v{n}` 名の artifact を追加する）。
- **再生成**: `cargo run -p config --example dump_schema -- docs/config/evorch-config-v2.schema.json`。生成は deterministic（byte-identical）で、CI が checked-in artifact との drift を検査するため手編集は不可。引数なしで実行すると標準出力に出る。
- **エディタでの利用**: 設定ファイルの先頭に schema directive を書くと補完・検証が有効になる（taplo / Even Better TOML 拡張）。

  ```toml
  #:schema <このリポジトリへの相対パス>/docs/config/evorch-config-v2.schema.json
  ```

  `#:schema` には設定ファイルからの相対パスまたは URL を指定できる。

## 関連

- [ADR 0004: Provider Type / Profile / Logical Model / API Protocol の分離](../decisions/0004-provider-routing-separation.md)
- [ADR 0008: 脅威モデル](../decisions/0008-threat-model-phased-adoption.md)
- [ADR 0014: config アーキテクチャ](../decisions/0014-config-architecture.md)
- [features/provider-routing/overview](../features/provider-routing/overview.md)
