# v13-d-model-auto-grouping Review Context

## Position and scope

ADR 0030 の 4 段のうち ④。自動グルーピングとチェーン編集の GUI を扱う。

## Intent references

- `intents/evorch/decisions/0030-role-model-chains.md`
- `intents/evorch/decisions/0013-model-catalog.md`
- `intents/evorch/features/gui-workbench/overview.md`

## Review focus

### 照合

- 速度区分の違うモデル（`+fast`、`-flex`）が同じ route に自動で混ざらないか。
- context window の違う同じ canonical id（`k3` と `k3-256k`）が衝突として扱われるか。
- canonical id がない、またはプロバイダ slug を推定できないモデルを誤って追加しないか。

### 設定の書き込み

- 自動追加が既存の候補の順番を変えないか。重複を作らないか。
- 上位の層（config.d、環境変数）が route を上書きしている場合に、書き込み先を誤らないか。

### GUI

- チェーンの編集が保存と再読み込みで保たれるか。存在しない route を選べないか。
- 衝突の選択と警告が、閉じた後に繰り返し出続けないか。

## Acceptance-to-evidence map

packet.yaml の acceptance_criteria の各項目に対応するテスト名と、GUI の証跡のパスを PR 本文に列挙する。
