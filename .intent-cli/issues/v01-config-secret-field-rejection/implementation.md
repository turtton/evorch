# v01-config-secret-field-rejection Implementation Packet

## Goal

`crates/config` の TOML type tree を strict にし、plaintext credential-like field を silent ignore せず明示拒否する。現行 `Config` は `#[serde(default)]` のみで、doc comment も未知 key を許容すると明記する（`crates/config/src/types/mod.rs:23-29`）。`ProviderProfileConfig` も `#[serde(default)]` のみ（`crates/config/src/types/provider.rs:83-100`）であるため、`providers.foo.api_key = "secret"` のような field は credential として使用されないまま parse が成功し、利用者が secret を config に保存した事実を見逃す。root と nested typed structs に unknown-field rejection を適用し、credential-like names は config path と Keyring/Env remediation を含む actionable error にする。canonical `intents/evorch/operations/config-reference.md` も実装と同時に厳格方針へ更新する。

## Why

v0.1 inspect で、issue #8 / `v01-routing-profiles` が実装した typed config が ADR 0014 の hard boundary を強制していないことが判明した。ADR 0014 は「credential は config に書かない」（`intents/evorch/decisions/0014-config-architecture.md:24-30`）と定め、`CredentialRefConfig` も secret material を保持せず Keyring/Env の参照だけを表現する契約である（`crates/config/src/types/provider.rs:53-73`）。しかし unknown field tolerance により plaintext secret が拒否されず、canonical reference も `intents/evorch/operations/config-reference.md:29` で未知 key を無視すると記載している。security boundary は「利用されない」だけでは不十分で、保存時点で明確に拒否し remediation を返す必要がある。

## Scope

- `Config` と `misc.rs` / `panel.rs` / `provider.rs` / `routing.rs` の nested typed structs に `deny_unknown_fields` 相当を適用し、通常 typo を path-aware parse error にする。serde の `default` と strict unknown rejection の互換を tests で固定する
- provider profile および credential object の `api_key`, `api-key`, `token`, `secret`, `password`, `credential_value` 等の credential-like names を検出し、単なる generic unknown-field error ではなく「plaintext credential は禁止。`credential = { type = \"keyring\", ... }` または `{ type = \"env\", var = \"...\" }` を使う」という actionable message を返す
- `CredentialRefConfig::{Keyring, Env}` の各 variant でも余分な secret-like field を拒否する。正しい field は Keyring=`service`,`account`、Env=`var`
- raw TOML / merged intermediate value / final typed deserialize の実経路を確認し、user config、project config、drop-in、CLI override のどこから secret-like field が来ても最終 load が拒否する
- tests に root unknown、nested typo、provider `api_key`、hyphen/alias、credential object 内 secret、valid Keyring、valid Env、multi-source merge 後 rejection を含める。error path と remediation wording を assert する
- `intents/evorch/operations/config-reference.md` の unknown-key 記述を strict rejection に変更し、Env example の `name` を実型どおり `var` に修正する。plaintext rejection の error/remediation を明記する

## Out of scope

- JSON Schema の publish / 配布 / `$schema` integration — separate packet
- runtime credential resolution、keyring backend、environment lookup、provider auth injection の変更
- credential storage migration、secret scanning、既存 config file の自動 rewrite
- config source precedence、deep merge semantics、version migration policy の再設計
- provider profile / routing の新 field 追加

## Verification

- config unit tests: root と各 nested struct の unknown field が path-aware error になる
- security tests: provider profile と credential object に plaintext credential-like field を置くと、Keyring/Env を示す actionable error で拒否される
- valid form tests: Keyring (`service`,`account`) と Env (`var`) が parse / serialize roundtrip する
- loader tests: user/project/drop-in/CLI merge 後の secret-like field が final typed parse で拒否される
- regression tests: version migration、deep merge、valid provider/routing/panel/diagnostics/permissions/metrics config が通る
- docs check: canonical reference の strict unknown-key 方針と `var` field が code と一致する
- `cargo test -p config` / `cargo clippy -p config -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: `features/provider-routing/overview.md`。issue #8 の config contract を security invariant まで具体化する。新規 intent は不要
- ADR candidate: decline — ADR 0014 の credential non-storage decision を parser で強制する修正
- Diagram candidate: decline — config source/merge topology は変更しない
- Docs update: **required** — `operations/config-reference.md` の unknown-key tolerance を strict rejection に更新し、Env example を `var` に修正する
- Closeout learning: strict 対象型、credential-like deny names/error wording、reference の修正内容を記録する。`write_back_required: true`

- Guide reachability (G645): parser/error behavior と operations reference の修正で、新しい CLI / GUI / 対話 surface は追加しない。`no_role_facing_surface: true`

`improve` (G456 / G460) は later safety net。packet-time で docs/intent writeback を宣言済み。
## 実装確定（2026-08-30、PR #24 / issue #23）

closeout learning どおり、以下がコード確定:

- **strict 対象型**: Config root 全 nested struct をカバー。`deny_unknown_fields` 適用 8 struct = Config / ProviderProfileConfig / RoutingConfig / RouteCandidateConfig / PanelConfig / DiagnosticsConfig / PermissionConfig / MetricsConfig。`CredentialRefConfig` は内部タグ enum 制約のため serde 属性不可 → private ミラー構造体（`CredentialRefDe` + `KeyringRefDe`/`EnvRefDe`、各 `deny_unknown_fields`）+ 手動 `Deserialize` 委譲で variant 単位拒否。public enum の shape・`Serialize`・`JsonSchema`・wire format（`{ type = "keyring", ... }` 等）は不変
- **denylist・エラー文言**: denylist は `strict.rs` の `CREDENTIAL_LIKE_KEYS`（api_key / apikey / api_token / access_token / auth_token / refresh_token / token / secret / client_secret / secret_key / api_secret / password / passphrase / credential_value / credentials / private_key / bearer_token）。照合は小文字化 + `-`→`_` 正規化（`API_Key`・`api-KEY` も一致）。credential-like 検出時のメッセージは固定文（「use the credential reference instead: credential = { type = "keyring", service = ..., account = ... } or { type = "env", var = ... }」）、それ以外の未知キーは `unknown field, expected one of: ...`。path は dotted（配列は `[i]` 添字。`routing.routes.fast[0].weight`）
- **スコープ**: credential denylist は `providers.<profile>` 直下 + その `credential` テーブル内のみ適用（`check_credential_scope_keys`）。他の root セクションは通常の unknown-key 拒否。任意キー許容 map（providers のプロファイル名 / routing.routes の route 名 / panel.keybinds）は維持
- **ロード経路**: `Config::load` が deep merge + version migration 後・`try_into` 直前で `crate::strict::validate_strict(&merged)` を呼ぶため、builtin / user / project / drop-in / env layer / CLI override の全ソース層由来が同一 strict 検査を通る。v1 ファイルの migration 後値にも適用（`v1_file_secret_rejected_after_migration` test）
- **canonical reference 修正**: `intents/evorch/operations/config-reference.md` — 未知キー許容記述を strict rejection に撤回、Env example の field 名を `name` から正しい `var` に修正、平文 credential 拒否節（例・照合規則・remediation 例）追加、任意キー許容 map の明記
- **挙動変更の留意**: strict 化により `EVORCH_API_KEY` 等の root 未知キー env var は load error になる（contract 意図、PR body 明記済み）。`strict.rs` allowlist 定数は struct 定義と手動同期（将来 field 追加時は strict.rs も更新）
- **検証**: cargo test --workspace 0 failed（RED 11+4 → GREEN）、clippy -D warnings clean、fmt/diff --check clean。実 surface（providers.foo.api_key が path+remediation で拒否・valid keyring/env parse・dump_schema 健全）確認済み。CI pass。Reviewer Gate: plan reviewer 第1回 APPROVED（blocker 0 / note 12）
