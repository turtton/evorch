## Goal

TOML config の root と nested type tree で未知 field を拒否し、`api_key` 等の plaintext credential-like field を Keyring/Env remediation 付き error にする。valid な credential reference は維持し、canonical config reference を実コードと strict policy に合わせる。

## Why This Slice Exists Now

v0.1 inspect で、issue #8 / `v01-routing-profiles` の config parser が unknown fields を許容し、plaintext secret を置いても parse success になる security gap が見つかった。ADR 0014 は credential を config に書かないと確定し、provider config は参照型だけを持つが、silent ignore では利用者に漏えいを知らせられない。v0.1.1 で parse boundary を strict にし、誤設定を即座に actionable error として止める。

## Current Observed State

- `crates/config/src/types/mod.rs:23-29` は `Config` に `#[serde(default)]` のみを付け、未知 key を前方互換のため許容すると明記する
- `crates/config/src/types/provider.rs:83-100` の `ProviderProfileConfig` も `deny_unknown_fields` を持たない
- `crates/config/src/types/provider.rs:53-73` の `CredentialRefConfig` は Keyring (`service`,`account`) / Env (`var`) の参照だけを表現し、secret material を保持しない hard contract である
- `intents/evorch/operations/config-reference.md:29` は未知 key を無視すると記載し、`:42-49` の Env example は実型の `var` ではなく `name` を使っている
- そのため `providers.main.api_key = "..."` は credential として利用されないが、拒否も警告もされず config file に plaintext が残る

## Accepted Baseline You May Assume

- ADR 0014: TOML + serde typed schema、versioned migration、credential は config に書かない
- `CredentialRefConfig` の valid forms は Keyring `{ type, service, account }` と Env `{ type, var }`
- config source は builtin/user/project/env/CLI と drop-in deep merge を持ち、最終 typed config に deserialize される
- schemars は型から JSON Schema を生成するが、schema publication は別 packet
- runtime credential resolution / provider auth injection は既存の Keyring/Env reference を消費しており、本 slice では変更しない

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/config/`, `intents/evorch/operations/config-reference.md`

Target part: typed TOML config tree の unknown field 拒否と credential-like field の actionable error

## In Scope

- Config root と全 nested typed config structs の unknown-field rejection
- provider profile / credential object の plaintext credential-like field に対する専用 actionable error
- Keyring / Env valid forms の維持と strict variant field validation
- multi-source merge 後の final typed parse でも同じ rejection を適用
- root/nested/alias/valid/merge path を覆う tests
- canonical config reference の strict policy、remediation、Env `var` field への更新

## Out Of Scope

- JSON Schema publication / editor integration
- runtime credential resolution、keyring/env lookup、provider auth injection
- config precedence / deep merge / migration semantics の変更
- secret scanner、自動 migration、自動 redaction
- provider/routing の新機能

## Standalone Child Issue Contract

`turtton/evorch` の `crates/config` で、root および nested typed config structs に unknown-field rejection を適用する。通常 typo は config path を含む parse error とし、provider profile または credential object に `api_key` / `api-key` / `token` / `secret` / `password` / `credential_value` 等の plaintext credential-like field がある場合は、plaintext credential を config に保存できないことと Keyring/Env reference の正しい形式を示す actionable error で拒否する。valid な Keyring `{ type = "keyring", service, account }` と Env `{ type = "env", var }` は維持する。user/project/drop-in/CLI の multi-source merge 後も final typed parse で同じ拒否を行う。`operations/config-reference.md` は未知 key の silent ignore 記述を撤回し、Env field を `var` に修正する。JSON Schema publication と runtime credential resolution は変更しない。PR は `main` をターゲットにする。

## Acceptance Criteria

- Config root と全 nested typed structs の未知 field が path-aware error になり、silent ignore されない
- provider profile の plaintext credential-like field が Keyring/Env remediation 付き error で拒否される
- CredentialRefConfig variant 内の余分な secret-like field も拒否される
- valid Keyring と valid Env (`var`) が parse / roundtrip する
- tests が root unknown、nested typo、api_key、alias、credential object、valid forms を覆い、path/remediation を assert する
- multi-source deep merge 後も全 source 由来の plaintext secret が final typed parse で拒否される
- runtime credential resolution / provider auth injection に変更がない
- config reference が strict unknown-key policy と Env `var` field を正しく記載する

## Verification

- `cargo test -p config`: strict type tree、credential-like rejection、valid Keyring/Env、merge/migration regression
- error assertions: provider path と Keyring/Env remediation が含まれる
- existing config loader/deep-merge/version migration tests
- code/docs consistency check for `CredentialRefConfig::Env { var }`
- `cargo clippy -p config -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Related Links

- intents/evorch/features/provider-routing/overview.md
- intents/evorch/decisions/0014-config-architecture.md
- intents/evorch/operations/config-reference.md
- Original v0.1 slice: issue #8 / v01-routing-profiles

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs
writeback is expected for this slice. Answer or explicitly decline:

- Intent placement: `features/provider-routing/overview.md`
- ADR candidate: none（ADR 0014 の既存 credential non-storage decision を強制）
- Diagram candidate: none
- Docs update: required — `operations/config-reference.md` の unknown-key 方針、credential rejection、Env `var` example
- Closeout writeback expected: yes。strict 対象型、credential-like deny names/error wording、canonical reference 修正を記録する

## Guide Reachability (G645)

While the author still knows the answer, name the guide surface and role that route to every
role-facing surface this slice adds, or explicitly say that no role-facing surface is added. A
blank answer is not treated as no-surface. The closeout record is a debt check, not a merge gate.

この slice は config parser の拒否と既存 operations reference の訂正であり、新しい CLI / GUI / 対話 surface を追加しない。`no_role_facing_surface: true` を宣言する。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
