# v14-f-allowed-directories-mounts Review Context

## Position and scope

ADR 0032 の 3 段のうち ①。許可ディレクトリの受け渡しと、shell・ファイルツールのアクセス権だけを扱う。システムメッセージへの記載と途中変更の追記は v14-g、ルールと skills の読み込みは v14-h であり、この PR に含まれないことを不足として扱わない。

## Intent references

- `intents/evorch/decisions/0032-project-allowed-directories.md`
- `intents/evorch/decisions/0033-file-tool-path-boundary.md`
- `intents/evorch/features/tools-sandbox/overview.md`

## Review focus

### trust と境界

- `Unapproved` の許可ディレクトリが、宣言・マウント・ファイルツールの境界のどこにも入らないか。
- 別のプロジェクトの許可ディレクトリが、run に見えていないか。
- 読み取り専用の role が、shell の bind やファイルツールの判定の抜けで書き込めないか。

### 走行中の反映

- 削除や trust の取り消しが、走行中の run の次のコマンドから効くか。sandbox の生成時に一覧を固定していないか。
- bwrap のマウントとファイルツールの判定が、同じ一覧から組み立てられているか（片方だけ更新されていないか）。

### 失敗時

- 存在しないパスが、run の開始やコマンドの実行を止めないか。

## Acceptance-to-evidence map

packet.yaml の acceptance_criteria の各項目に対応するテスト名を PR 本文に列挙する。
