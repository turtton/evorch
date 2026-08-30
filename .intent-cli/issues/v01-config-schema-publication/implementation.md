# v01-config-schema-publication Implementation Packet

## Goal

`Config` v2型から生成したJSON Schemaを安定したversioned pathへchecked-inし、再生成可能なcommandとCI drift checkを提供する。`config-reference.md`からartifactと利用方法を参照できる状態にし、typed configと公開schemaの同期を継続的に保証する。

## Why

issue #8 / `v01-routing-profiles` のv0.1 inspectで、ADR 0014の「schemarsで生成し公開」が半分しか実装されていないmedium driftが判明した。`crates/config/src/types/mod.rs:20-44` はversion 2の `Config` に `JsonSchema` をderiveし、`crates/config/src/schema.rs:1-14` と `examples/dump_schema.rs:1-5` は生成できる。しかしtracked artifactがなく、`intents/evorch/operations/config-reference.md:94-96` は「生成可能」とだけ書き、`.github/workflows/ci.yml:7-19` に同期checkもない。v0.1.1でユーザー/エディタが参照できる契約として完成させる。

## Scope

- 既存 `schemars = "1"` と `config::json_schema()` を採用する。dependencyは既にworkspace/config crateに存在するため追加libraryを評価・導入しない。
- `CURRENT_VERSION=2` に対応するartifactを、例として `docs/config/evorch-config-v2.schema.json` のようなstable/versioned pathへ生成・commitする。最終pathは一つに固定しpacket内全参照を合わせる。
- generatorは既存exampleをCIで使える決定的commandへ整える。出力順/末尾改行等を安定させ、artifactを手編集しない。
- CIに「一時生成 → checked-in artifactとbyte diff」または同等の同期checkを追加し、差分時に更新commandを示してfailする。
- schemaがConfigのroot section、nested types、enum、credential reference形状を含み、有効JSON Schemaであるtestを追加する。
- filename/schema metadataと `CURRENT_VERSION` のv2対応をtestまたはgeneratorで固定する。
- `config-reference.md` の全既存内容を保ちつつJSON Schema節をartifact path、生成/更新command、taplo等での利用方法へ更新する。

## Out of scope

- config TOML parsing、validation、deep merge、environment/CLI priority、version migrationの挙動変更。
- unknown key許容方針の変更。
- secret field拒否。別packet `v01-config-secret-field-rejection` が担当する。
- 新しいconfig fieldやCURRENT_VERSIONの更新。
- schema registry/CDN/release upload等の外部配布基盤。

## Verification

- generator commandを実行し、tracked v2 schemaと差分ゼロになること。
- `cargo test -p config` でschema JSON妥当性、主要section/enum、version整合を検証する。
- CI drift checkをローカルと同じcommandで実行し、artifactを意図的に変えたfixture/手順でfailure conditionも確認する。
- `cargo clippy -p config --all-targets -- -D warnings` / `cargo fmt --all --check` / `git diff --check`。

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: provider-routing/configの既存nodeを完成させる。新規node不要。
- ADR candidate: なし。ADR 0014がschemars生成・公開を決定済み。
- Diagram candidate: なし。
- Docs update: `intents/evorch/operations/config-reference.md` のJSON Schema節を実在artifactと更新commandへ変更（必須）。
- Closeout learning: artifact path、versioning、再生成command、CI drift checkをconfig-referenceへwrite back。`write_back_required: true`。
- Guide reachability (G645): artifact/docs追加はあるが新しいguide workflowやrole-facing操作surfaceではないため `no_role_facing_surface: true`。

`improve` (G456 / G460) is the later safety net; packet-time maintenance is the normal path.
