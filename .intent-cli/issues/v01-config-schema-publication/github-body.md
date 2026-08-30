## Goal

Config v2のJSON Schemaをversioned artifactとして公開し、型からの再生成とCI同期checkを提供する。

## Why This Slice Exists Now

issue #8 / `v01-routing-profiles` のv0.1 inspectで、schema生成コードはあるが公開artifactがなく、ADR 0014のpublication要件を満たさないmedium driftが見つかった。エディタ補完と検証のstable targetをv0.1.1で提供する。

## Current Observed State

- `crates/config/src/types/mod.rs:20-44` は `CURRENT_VERSION=2` と `JsonSchema` derive済み `Config` を持つ。
- `crates/config/src/schema.rs:1-14` は `schemars::schema_for!(Config)` をJSON化し、`examples/dump_schema.rs:1-5` でstdout出力できる。
- `Cargo.toml:27` / `crates/config/Cargo.toml:10-15` ですでにschemars 1を使用する。
- `config-reference.md:94-96` は公開pathなし、`.github/workflows/ci.yml:7-19` はschema driftを検査しない。

## Accepted Baseline You May Assume

- config formatはTOML、schema versionは2、typed source of truthは `Config`。
- schemars 1 / serde 1 / serde_json 1 / toml 1を継続利用する。新しいschema libraryは不要。
- unknown key許容、migration、deep merge等のruntime挙動は既存契約のまま。

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/config/`, `docs/config/`, `.github/workflows/ci.yml`, `intents/evorch/operations/config-reference.md`

Target part: Config v2 JSON Schema artifactと同期保証

## In Scope

- stable/versioned pathへのv2 schema checked-in artifact。
- `config::json_schema()`を唯一のsourceとするdeterministic generator。
- artifactと再生成結果のCI diff check。
- schema妥当性、主要section/nested enum、version整合test。
- config-referenceのartifact path、更新command、エディタ利用説明。

## Out Of Scope

- parsing/validation/deep merge/migration/unknown-key挙動変更。
- config field追加やversion 3移行。
- secret field拒否（`v01-config-secret-field-rejection`）。
- CDN/schema registry/release upload。

## Standalone Child Issue Contract

既存の `schemars 1` と `config::json_schema()` を使い、`CURRENT_VERSION=2` のJSON Schemaを `docs/config/evorch-config-v2.schema.json` 等の一意なstable/versioned pathへchecked-inする。再生成commandを決定的にし、CIで再生成結果とartifactを比較して型変更時の更新忘れをfailさせる。schemaの有効性・主要Config構造・v2整合をtestし、`config-reference.md`からartifact、更新command、エディタ利用方法を参照させる。runtime config挙動とsecret-field policyは変更しない。

## Acceptance Criteria

- Config v2 schema artifactがstable/versioned pathに存在する。
- artifactが有効JSON Schemaで全root sectionとnested enumを反映する。
- generatorはschemars/config::json_schemaをsingle sourceとする。
- CIがartifact driftを検出して失敗する。
- config-referenceがartifact path・更新command・利用方法を記載する。
- CURRENT_VERSIONとartifact v2の整合をtestする。
- config runtime挙動を変更しない。

## Verification

- schema再生成command後のdiffが空。
- `cargo test -p config`。
- CI schema check、`cargo clippy -p config --all-targets -- -D warnings`、`cargo fmt --all --check`、`git diff --check`。

## Related Links

- [provider-routing/overview.md](../../../intents/evorch/features/provider-routing/overview.md)
- [0014-config-architecture.md](../../../intents/evorch/decisions/0014-config-architecture.md)
- [config-reference.md](../../../intents/evorch/operations/config-reference.md)

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs writeback is expected for this slice.

- Intent placement: 既存provider-routing/config領域。
- ADR candidate: なし（ADR 0014で決定済み）。
- Diagram candidate: なし。
- Docs update: config-reference JSON Schema節（必須）。
- Closeout writeback expected: yes（artifact path・再生成command・CI同期checkを記録）。

## Guide Reachability (G645)

新しいguide workflow/role-facing操作surfaceは追加しない（`no_role_facing_surface: true`）。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
