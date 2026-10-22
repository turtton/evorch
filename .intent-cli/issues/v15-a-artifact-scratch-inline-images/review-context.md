# v15-a-artifact-scratch-inline-images Review Context

## Position and scope

ADR 0038 の最初の slice。`render_artifact` の取り込み元を一時領域に広げ、HTML の相対パス画像を埋め込む。画像生成そのもの（v15-c）とモデルの種別（v15-b）はこの PR に含まれないことを不足として扱わない。

## Intent references

- `intents/evorch/decisions/0038-image-generation.md`（方針）
- `intents/evorch/decisions/0029-conversation-artifacts.md`（成果物の作成と提示）
- `intents/evorch/decisions/0033-file-tool-path-boundary.md`（一時領域を含む境界）

## Review focus

### 境界

- ワークスペースと一時領域の判定が canonicalize 後の実パスで行われているか。HTML 内の参照も同じ判定を通っているか。
- 別 thread の一時領域や、許可ディレクトリ、`..` と symlink による脱出を拒否しているか。
- 相対パスの基準が、HTML ファイルの実パスのディレクトリになっているか（symlink の HTML を経由して基準がずれないか）。

### 埋め込み

- 書き換えてよい参照だけを書き換えているか。スキーム付きの参照や `#` を誤って解決していないか。
- 属性の引用符、大文字小文字、CSS の引用符なし `url()`、コメントで壊れないか。
- 読み込みとサイズ判定の間でファイルが大きくなっても、上限を超えて読まないか。埋め込み後の上限を確かめてから保存しているか。

### 失敗の扱い

- 1 件でも問題があれば何も保存しないか。拒否の理由に、どの参照かが分かる情報があるか。ファイルの中身や秘密情報を理由に含めていないか。

### 互換

- 埋め込みのない HTML と画像が、従来と同じバイト列・sha256 で保存されるか。旧形式の meta JSON を読めるか。
- 新しい依存（`lol_html` を加えた場合）のライセンスとビルドへの影響。

### キャッシュ

- tool 説明の変更が、セッションの途中でツール定義を変える経路を作っていないか（`scripts/check-cache-contracts.sh`）。

## Acceptance-to-evidence map

packet.yaml の acceptance_criteria の各項目に対応するテスト名を PR 本文に列挙する。
