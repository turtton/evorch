# ADR 0030 ②: 設定 v3 のロールのモデルチェーン・一周するフォールバック・ピンの永続化

## Goal

route を「同じモデルへの経路プール」に限定し、ロールが route を優先順に並べたチェーンを持てるようにする。推論強度はチェーンの要素ごとに指定する。フォールバックは一周だけ回し、ピンを再起動後も維持する。

## Why This Slice Exists Now

route に別モデルを混ぜるしかなく、`kimi-k2` という route に `gpt-6.1-sol` が入るなど名前と中身がずれていた。planner を「Kimi K3 のサブスク → 別プロバイダの K3 → Claude」とするような指定ができない。さらに、再起動するとピンが消え、フォールバック後のセッションがチェーンの先頭に戻ってしまう。

## Current Observed State

- ロールは `logical_model` で route を 1 つだけ指す。
- `reasoning_effort` は route の候補にあり、ロールごとに変えられない。
- フォールバックは失敗位置より後ろの候補だけを走査し、先頭には戻らない。
- `SessionAffinity` はメモリ上にあり、run スナップショットの `selected_model` は表示にしか使われない。

## Accepted Baseline You May Assume

- チェーンは Tier 1（route）だけを参照し、チェーンがチェーンを参照することはない（ADR 0030）。
- 速度・料金の区分（`+fast` など）は route 側に残す。
- ピンは元に戻さない。新しいセッションはチェーンの先頭から解決する。
- v13-a のクールダウンレジストリがある。

## Target Repo / Path / Part

- Target repo: `turtton/evorch`
- Target paths: `crates/config/` `crates/routing/` `crates/runtime/` `crates/gui/` `docs/` `intents/`
- Target part: 設定 v3 と移行、チェーンの展開と一周するフォールバック、Retry-After 待機、ピンの永続化、GUI の最小限の追従。

## In Scope

- 設定 v3: `RouteCandidateConfig` から `reasoning_effort` を削除し `auto` を追加。`RoleBindingConfig` / カテゴリに `models` を追加し、`logical_model` を削除。
- v2 → v3 の移行と警告。スキーマの再生成。
- Router のチェーン解決、一周の走査、プールの境界、元の位置の再試行規則。
- Retry-After の短い待機（既定 30 秒、設定可能）。
- 要素ごとの `reasoning_effort` の適用。
- ピンの run スナップショットへの保存と、復元時の affinity への反映。
- GUI: ロール設定で route と effort を編集（単一要素）、route 設定から effort を削除、2 要素以上のチェーンを保持したまま保存。

## Out Of Scope

チェーンを並べ替える GUI と自動グルーピング（v13-d）。compaction の変更と切り替え時の要約し直し（v13-c）。

## Acceptance Criteria

1. 設定 v3 の route 候補は profile / model（と auto）だけを持ち、ロールとカテゴリは `models` を持つ。存在しない route の参照は厳格な読み込みでエラーになり、通常の読み込みでは警告して使用時に失敗する。
2. v2 の設定は v3 に移行され、effort がロール側に移る。移行できない箇所は警告される。v2 のファイルは次の保存で v3 として書き直す。
3. 位置 i の失敗後、i+1 → 末尾 → 先頭 → i-1 の順に一周だけ試す。元の位置は一時的な失敗のときだけ最後に再試行する。
4. プールの全メンバーが失敗済みかクールダウン中なら、次のプールへ進む。
5. Retry-After が短い一時的な失敗でプールが尽きたら、一度だけ待って再試行する。
6. 成功したメンバーにピンが移り、要素ごとの effort がリクエストに反映される。
7. ピンが保存され、再起動後に同じメンバーから再開する。ピン先が使えない場合は通常のフォールバックで次へ進む。
8. GUI で route と effort を編集でき、2 要素以上のチェーンを壊さない。
9. `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更範囲の `cargo nextest run`、`scripts/check-cache-contracts.sh`、`git diff --check` が pass する。

## Verification

1. Unit: 移行、チェーンの検証、一周の走査、プールの境界、Retry-After 待機（時計注入）、effort の適用。
2. Integration: mock provider でのチェーン全体のフォールバック、スナップショットからの復元でピンが維持されること、GUI の保存と再読み込み。
3. Build / lint: 上記のコマンド一式。

## Related Links

- `intents/evorch/decisions/0030-role-model-chains.md`
- `intents/evorch/decisions/0014-config-architecture.md`
- `intents/evorch/decisions/0027-restore-contract.md`

## Base Branch Policy

`main` から作業ブランチを切り、PR で `main` にマージする。
