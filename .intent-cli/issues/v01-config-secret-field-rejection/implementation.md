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
