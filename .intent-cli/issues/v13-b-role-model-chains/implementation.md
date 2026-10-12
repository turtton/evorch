# v13-b-role-model-chains Implementation Packet

## Goal

route を同じモデルへの経路プールに限定し、ロールが route を優先順に並べたチェーンを持てるようにする。推論強度は要素ごとに指定し、フォールバックは一周だけ回し、ピンを再起動後も維持する。

## Why

同じモデルの冗長経路と別モデルへの切り替えは性質が違う（ADR 0030）。名前・推論強度・切り替えのコストを分けて扱うため、2 層に固定する。

## Scope

- **Config v3:** `CURRENT_VERSION = 3`。`RouteCandidateConfig { profile, model, auto }`。`RoleBindingConfig` と `CategoryBindingConfig` に `models: Vec<ChainEntryConfig { route, reasoning_effort }>`。`logical_model` は削除し、`rename_logical_model_refs` などの補助関数を route 参照に合わせて改める。`ResolvedAgentBinding` はチェーンを返す。
- **移行:** `migrate_v2_to_v3`。同じファイル内の route から effort を引いてロール側の要素へ移す。候補間で effort が異なる場合は先頭を採用して警告、参照先が同じファイルにない場合は effort なしで移して警告。
- **Router:** チェーンの要素を展開した列とプールの境界を持つ。`resolve` は先頭から（ピンがあればピンから）。`next_fallback` は一周の走査とプールの境界の規則に従う。クールダウン中のメンバーは飛ばす。
- **Retry-After 待機:** 一時的な失敗でプールが尽き、記録された Retry-After が上限（既定 30 秒、`routing.fallback.max_retry_after_wait_secs`）以下なら、一度だけ待ってそのプールを再試行する。待機は注入可能な時計を使う。
- **effort:** 解決したメンバーの属するチェーン要素の `reasoning_effort` をリクエストに設定する。
- **ピンの永続化:** `RunRestoreDescriptor` にピン（チェーン内の位置、プロファイル、具体モデル）を保存する。復元時に affinity へ反映し、使えなければ次へ進む。
- **GUI:** ロール設定は先頭要素の route と effort を編集し、2 要素以上のチェーンは後続要素を保ったまま保存する。route 設定から effort を削除する。
- **Docs:** `docs/providers.md`、スキーマの再生成。

### 採用・却下案と理由

- `logical_model` を文字列またはリストの両方で受ける案は採らない。要素ごとに effort を持たせるため、型を 1 つにまとめる。
- 名前付きのチェーンは作らない（ADR 0030）。

## Out of scope

チェーンを並べ替える GUI、自動グルーピング（v13-d）、compaction（v13-c）。

## Verification

### Unit

- 移行（effort の移動、警告の条件）、ロード時の参照検証。
- 一周の走査、元の位置の再試行規則、プールの境界、クールダウン中のメンバーの読み飛ばし。
- Retry-After 待機（時計注入。sleep に依存しない）。
- effort の適用。

### Integration → build

- mock provider でのチェーン全体のフォールバック。
- スナップショットからの復元でピンが維持されること。
- GUI の保存と再読み込み（headless）。
- `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更 crate の `cargo nextest run`、`scripts/check-cache-contracts.sh`、`git diff --check`。
