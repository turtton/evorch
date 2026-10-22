# v15-c-generate-image-tool Review Context

## Position and scope

ADR 0038 の 3 つ目の slice。provider の画像生成 capability と `generate_image` ツールを入れる。v15-a（HTML の画像埋め込み）と v15-b（モデルの種別と画像生成のチェーン）を前提にする。マスク編集・複数枚・`openai-compatible` はこの PR に含まれないことを不足として扱わない。

## Intent references

- `intents/evorch/decisions/0038-image-generation.md`（方針）
- `intents/evorch/decisions/0029-conversation-artifacts.md`（成果物の作成と提示）
- `intents/evorch/decisions/0030-role-model-chains.md`（チェーンとフォールバック）
- `intents/evorch/decisions/0033-file-tool-path-boundary.md`（ファイルの境界）
- `intents/evorch/decisions/0002-role-capability-boundaries.md`（権限は runtime が決める）

## Review focus

### 権限の境界

- `generate_image` の付与条件が policy と dispatch で一致しているか。子 run の conversation category や Orchestrator に付与されていないか。category・root・チェーンの有無を model の入力から決めていないか。
- run の途中で設定を再読み込みしても、ツール定義が変わらないか（キャッシュ）。

### ファイル

- 保存先と参照画像の判定が canonicalize 後の実パスか。存在しない保存先を親ディレクトリで判定しているか。symlink や別 thread の一時領域で抜けられないか。
- 書き込みが `write` と同じ境界と ownership の検査を通っているか。検証の前にファイルを書いていないか。取り消しで中途半端なファイルが残らないか。
- 参照画像の読み込みが上限で制限されているか。

### 送信と認証

- Codex のヘッダー付与が会話の送信と同じ関数を使い、トークンの更新とアカウント ID が共有されているか。
- API キーやアクセストークン、参照画像の内容がイベント・ログ・エラー本文・tool 結果に出ていないか。
- 参照画像を送ったことが結果に明記されているか。

### 応答と使用量

- 応答本文の上限がストリームで守られているか（全体を読んでから判定していないか）。時間の上限があるか。
- usage が 1 回だけ記録されるか。失敗した試行で記録していないか。
- エラーの分類が、フォールバックしてよいもの（認証・上限・一時的・非対応）と、してはいけないもの（内容の拒否・不正な要求）で分かれているか。

### Prompt

- tool 説明と overlay が、生成画像を UI モックの代わりにしないこと、採否は agent の判断であることを伝えているか。ツール名や引数名が実装と一致しているか。

## Acceptance-to-evidence map

packet.yaml の acceptance_criteria の各項目に対応するテスト名を PR 本文に列挙する。実アカウントでの手動確認は、実施の有無と結果を PR に記載する。
