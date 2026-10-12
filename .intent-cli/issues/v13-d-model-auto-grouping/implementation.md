# v13-d-model-auto-grouping Implementation Packet

## Goal

プロバイダを追加したときに同じモデルの route へ冗長経路が自動で加わり、ロールのチェーンを GUI から組めるようにする。

## Why

route が経路プールになったことで（v13-b）、プロバイダの追加ごとに手作業が増えた。models.dev の canonical id で同じモデルを判定できる。

## Scope

- **catalog:** `WireModel` / `ModelMetadata` に `canonical_model_id` を追加し、（プロバイダ slug, モデル ID）から引く関数を用意する。プロバイダ slug は `models_dev_provider_candidates`、プロファイル名、一意一致の順で推定する。
- **キー:** （canonical id, 速度区分）。`parse_model_speed` で evorch の `+fast` を外して照合し、区分をキーに含める。
- **自動追加:** プロバイダの保存（追加・モデルの有効化）の後に、該当する route の末尾へ `auto = true` で追加する。既に同じ（profile, model）があれば何もしない。
- **衝突:** 同じプロファイル内で有効なモデルのうち複数が同じキーに当たる場合は追加せず、GUI の通知から 1 つ選ばせる。
- **警告:** 起動時と catalog の更新時に、既存の auto 候補の canonical id を照合し直し、変わっていれば通知する。設定は書き換えない。
- **GUI:** ロール設定にチェーン編集（要素の追加・削除・上下移動、route の選択、effort の入力）。route 設定で auto 候補に印を付ける。
- **Docs:** `docs/providers.md`、`docs/gui-verification.md`。

### 採用・却下案と理由

- 読み込みのたびに計算し直す案は却下する（ADR 0030。エイリアスの変化でプールが黙って変わる）。
- `family` による照合は却下する（粒度が粗い）。

## Out of scope

route の自動作成、models.dev 以外の照合元。

## Verification

### Unit

- canonical id の照合、プロバイダ slug の推定、速度区分。
- 衝突の判定、自動追加の規則（重複しない、ロールが参照しない route は作らない）、警告の条件。

### Integration → build

- 設定の保存と再読み込み。
- GUI の headless 操作（チェーンの編集、衝突の選択）と画面の証跡。
- `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更 crate の `cargo nextest run`、`git diff --check`。
