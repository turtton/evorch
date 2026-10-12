# v13-b-role-model-chains Review Context

## Position and scope

ADR 0030 の 4 段のうち ②。設定 v3、チェーンのフォールバック、ピンの永続化を扱う。チェーンを並べ替える GUI と自動グルーピング（v13-d）、compaction（v13-c）は含まれないことを不足として扱わない。

## Intent references

- `intents/evorch/decisions/0030-role-model-chains.md`
- `intents/evorch/decisions/0014-config-architecture.md`（設定の移行）
- `intents/evorch/decisions/0027-restore-contract.md`（復元）
- `intents/evorch/features/provider-routing/overview.md`

## Review focus

### 設定と移行

- 移行がファイル単位で完結し、別の層にある route を誤って書き換えないか。
- 存在しない route の参照、空のチェーンがロード時に拒否されるか。
- `role_profiles` 内の agents / routing も同じく移行されるか。

### フォールバック

- 一周で必ず止まり、同じメンバーを二度試さないか（元の位置の再試行規則を除く）。
- プールの境界の判定がこの一周の中の失敗に基づいているか。
- Retry-After 待機が一度だけで、上限を超える Retry-After では待たないか。
- usage の発行が勝者 attempt の 1 件だけのままか。

### ピン

- 再起動後に同じメンバーから再開するか。ピン先が設定から消えていても panic しないか。
- 新しい run がピンを引き継がず、先頭から解決するか。

### キャッシュ

- 通常のターンで provider の入力 prefix、reasoning_effort、affinity key が変わらないか（`scripts/check-cache-contracts.sh`）。

## Acceptance-to-evidence map

packet.yaml の acceptance_criteria の各項目に対応するテスト名を PR 本文に列挙する。
