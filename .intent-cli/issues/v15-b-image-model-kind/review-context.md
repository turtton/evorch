# v15-b-image-model-kind Review Context

## Position and scope

ADR 0038 の 2 つ目の slice。モデルの種別タグと画像生成ロールのチェーン、その両方向の検査を入れる。`generate_image` と provider の画像 capability（v15-c）はこの PR に含まれないことを不足として扱わない。

## Intent references

- `intents/evorch/decisions/0038-image-generation.md`（方針）
- `intents/evorch/decisions/0030-role-model-chains.md`（チェーン、route、自動グルーピング）
- `intents/evorch/decisions/0014-config-architecture.md`（設定の層と互換）

## Review focus

### 種別の決まり方

- 実効の種別を決める処理が 1 か所にまとまり、config・runtime・GUI が同じ関数を使っているか。GUI だけ別の判定を持っていないか。
- `model` を省略した候補が `default_model` の種別で判定されているか。登録のない `model` の上書きが名前の推定で判定されているか。
- 推定の接頭辞が会話用モデルを誤って画像と判定しないか（境界の例のテスト）。

### 検査の漏れ

- 通常のロール、各カテゴリ、compactor、ロールプロファイルのすべてのチェーンが検査対象か。暗黙のロール名の route（`models` 未指定）も種別の確認を通るか。
- image_generator が `binding_for` や暗黙のフォールバックで agent のロールとして扱われていないか。
- 通常の読み込みで警告にとどめた設定が、送信前の確認で必ず止まるか。止まったときにプロンプトを送っていないか。

### 互換

- `kind` なしの既存設定、文字列だけのモデル要素、ロールプロファイルが変わらず読み込めるか。保存し直したときに、利用者が書いていない `kind` を勝手に書き足していないか（GUI での追加時を除く）。
- スキーマの再生成が `kind` と `image_generator` 以外の差分を含んでいないか。

### GUI

- 候補の絞り込みで、既存の設定にある種別の合わない route が見えなくなって編集できなくなる、ということがないか（エラーとして表示されるか）。
- 自動グルーピングの衝突の解決 UI に画像モデルが混ざらないか。

## Acceptance-to-evidence map

packet.yaml の acceptance_criteria の各項目に対応するテスト名を PR 本文に列挙する。
