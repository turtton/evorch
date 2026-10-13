# v14-e-file-tool-boundary Review Context

## Position and scope

ADR 0033 の 2 段のうち ②。ファイルツールのパス境界と昇格の経路を扱う。一時領域の固定パス化は v14-d、許可ディレクトリのマウントは v14-f であり、この PR に含まれないことを不足として扱わない。

## Intent references

- `intents/evorch/decisions/0033-file-tool-path-boundary.md`
- `intents/evorch/decisions/0002-role-capability-boundaries.md`
- `intents/evorch/decisions/0008-threat-model-phased-adoption.md`
- `intents/evorch/decisions/0021-bwrap-linux-sandbox.md`
- `intents/evorch/features/tools-sandbox/overview.md`

## Review focus

### 境界のすり抜け

- 相対パス、`..`、シンボリックリンク、まだ存在しない書き込み先、特殊ファイルで境界の外に出られないか。判定と実際の open の間にリンクを差し替えられる余地は、benchmark の境界と同じ前提（同時に動く信頼できないプロセスがリンクを変えない）に従っているか。
- `grep` がシンボリックリンクをたどって境界の外の内容を返さないか。
- 拒否したときの結果やイベントに、ファイルの中身が出ていないか。

### 一覧の一致

- bwrap のマウントとファイルツールの判定が、本当に同じ一覧から作られているか。片方だけに足された領域がないか。
- Cargo / rustup のキャッシュをホスト側のパスで判定しているか（sandbox の中の `/tmp/home` 以下のパスと混同していないか）。

### 昇格

- `"unsandboxed"` が空の justification で通らないか。off で必ず拒否されるか。
- 昇格で role の権限が外れていないか。読み取り専用の role の書き込みが通らないか。
- 審査役に渡す差分が長さを制限されているか。credential の値を審査役やイベントに流していないか。

### 互換と cache

- benchmark のスキーマから昇格の引数が取り除かれ、従来の固定した境界が使われているか。
- ツールのスキーマの変更が、セッションの途中でツール定義を変える経路を作っていないか。

## Acceptance-to-evidence map

packet.yaml の acceptance_criteria の各項目に対応するテスト名を PR 本文に列挙する。
