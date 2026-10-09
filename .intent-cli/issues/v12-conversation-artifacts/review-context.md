# v12-conversation-artifacts Review Context

## Position and scope

ADR 0029 の Phase 1。成果物の作成と提示を別権限で追加し、conversation に画像・HTML のカードを表示する。HTML のスナップショット、model への画像返却、`ask_user` 連携は後続 Phase であり、この PR に含まれないことを不足として扱わない。

## Intent references

- `intents/evorch/decisions/0029-conversation-artifacts.md`（方針）
- `intents/evorch/decisions/0002-role-capability-boundaries.md`（role は権限の境界）
- `intents/evorch/decisions/0022-parent-child-tree-addressing-and-nested-delegation.md`（run 木）
- `intents/evorch/decisions/0027-restore-contract.md`（復元は権限を復活させない）
- `intents/evorch/features/gui-workbench/overview.md`、`intents/evorch/features/orchestration/overview.md`

## Review focus

### 権限の境界

- `render_artifact` と `present` の付与条件が policy（tool 一覧）と dispatch の両方で一致しているか。category や root の判定に model 入力を使っていないか。
- 子 run の `conversation` category や、delegate で指定できない category を経由して tool が付与されないか。

### ファイルの扱い

- workspace 外の path、`..`、symlink による脱出、特殊ファイルを拒否しているか。canonicalize 後の判定か。
- 読み込みとサイズ判定の間でファイルが差し替わっても上限を超えて読まないか。
- HTML の secret 検出時に保存されないか。

### 所有権

- `present` が別 thread の artifact を拒否し、escalation 後の新 root（同じ thread）からは提示できるか。
- 1 件でも不正な id があれば何も提示しないか。

### GUI と復元

- カードが thread transcript に出て、子 run の transcript に誤って出ないか。
- 外部で開く操作が `take_file_links` に横取りされず、in-app viewer で HTML ソースが開かれないか。
- イベント再生で同じカードが復元されるか。保存ファイルが無い場合に panic しないか。

### キャッシュ

- tool spec の追加が対象 run 以外の provider prefix を変えないか（`scripts/check-cache-contracts.sh`）。

## Acceptance-to-evidence map

packet.yaml の acceptance_criteria の各項目に対応するテスト名を PR 本文に列挙する。portal による外部表示は手動確認事項として PR に記載する。
