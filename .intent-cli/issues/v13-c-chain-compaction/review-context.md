# v13-c-chain-compaction Review Context

## Position and scope

ADR 0030 の 4 段のうち ③。要約モデルの選択と、切り替え時の要約し直しを扱う。GUI と自動グルーピングは v13-d であり、この PR に含まれないことを不足として扱わない。

## Intent references

- `intents/evorch/decisions/0030-role-model-chains.md`
- `intents/evorch/decisions/0003-cache-first-context-engine.md`
- `intents/evorch/decisions/0027-restore-contract.md`
- `intents/evorch/features/context-engine/overview.md`

## Review focus

### 文脈の保存

- 切り替え先に blob が送られる経路が残っていないか（フォールバック、復元、fork、エスカレーション）。
- 要約し直しの入力が、blob ではなく checkpoint の範囲の元のメッセージになっているか。

### 選択と退避

- context window の判定がメンバー単位か。入力の推定がモデルに送る量と一致しているか。
- 使えるモデルがないときに構造的要約で必ず完了し、無限に再試行しないか。

### cache 契約

- 通常のターンで要約し直しが走らないか。切り替えのときだけ境界になり、その後のターンで prefix が保たれるか（`scripts/check-cache-contracts.sh`）。

## Acceptance-to-evidence map

packet.yaml の acceptance_criteria の各項目に対応するテスト名を PR 本文に列挙する。
