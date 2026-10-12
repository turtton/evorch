# ADR 0030 ④: models.dev の canonical id による同一モデルの自動グルーピングと、GUI のチェーン編集

## Goal

プロバイダを追加したときに、同じモデルの route へ冗長経路が自動で加わるようにする。ロールのフォールバック先（チェーン）を GUI から組めるようにする。

## Why This Slice Exists Now

v13-b で route が「同じモデルへの経路プール」になり、プロバイダを追加するたびに各 route へ手で候補を足す必要が出た。models.dev の `canonical_model_id` を使えば、`kimi-for-coding` → `kimi-k2.8-preview`、`k3` → `kimi-k3` のように、名前の違う同じモデルを判定できる。v13-b の GUI は単一要素のチェーンしか編集できない。

## Current Observed State

- catalog は `canonical_model_id` を読んでいない。
- route への候補の追加は手作業。
- GUI のロール設定はチェーンの先頭要素しか編集できない。

## Accepted Baseline You May Assume

- 自動グルーピングは追加した時点で設定に書き込み、読み込みのたびに計算し直さない（ADR 0030）。
- 速度・料金の区分は route 側の属性で、グループのキーに含める。
- 同じプロファイル内の衝突は利用者に選ばせる。

## Target Repo / Path / Part

- Target repo: `turtton/evorch`
- Target paths: `crates/catalog/` `crates/config/` `crates/gui/` `docs/` `intents/`
- Target part: canonical id の読み取りと照合、保存時の自動追加、衝突の選択 UI、エイリアス変化の警告、チェーン編集の GUI。

## In Scope

- catalog の `canonical_model_id` の読み取りと照合。
- プロバイダ保存時の自動追加（`auto = true`、最低優先度）。
- 同じプロファイル内の衝突を選ぶ GUI。
- 既存の auto 候補の canonical id が変わったときの警告。
- ロール設定のチェーン編集（追加・削除・並べ替え・要素ごとの effort）。
- `docs/providers.md` と GUI 検証の証跡。

## Out Of Scope

route の自動作成、models.dev 以外のメタデータ源による照合、`family` による照合。

## Acceptance Criteria

1. catalog が `canonical_model_id` を引け、速度区分を外して照合し、キーに含める。
2. 保存時に、同じ canonical id と速度区分の route があれば最低優先度に `auto = true` で追加される。ロールが参照していない route は作らない。
3. 同じプロファイル内の衝突は自動で追加せず、GUI で選ばせる。
4. canonical id がないモデルは自動で追加しない。
5. canonical id の変化は警告だけで、設定を書き換えない。
6. GUI でチェーンの追加・削除・並べ替えと effort の編集ができ、保存と再読み込みで保たれる。
7. `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更範囲の `cargo nextest run`、`git diff --check` が pass し、GUI の headless 検証で証跡を残す。

## Verification

1. Unit: canonical id の照合（プロバイダ slug の推定、速度区分）、衝突の判定、自動追加の規則、警告の条件。
2. Integration: 設定の保存と再読み込み、GUI の headless 操作と画面の証跡。
3. Build / lint: 上記のコマンド一式。

## Related Links

- `intents/evorch/decisions/0030-role-model-chains.md`
- `intents/evorch/decisions/0013-model-catalog.md`
- `intents/evorch/features/gui-workbench/overview.md`

## Base Branch Policy

`main` から作業ブランチを切り、PR で `main` にマージする。
