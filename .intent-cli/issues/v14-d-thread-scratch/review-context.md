# v14-d-thread-scratch Review Context

## Position and scope

ADR 0033 の 2 段のうち ①。一時領域を thread 単位の固定パスにすることだけを扱う。ファイルツールのパス境界と昇格の経路は v14-e であり、この PR に含まれないことを不足として扱わない。

## Intent references

- `intents/evorch/decisions/0033-file-tool-path-boundary.md`
- `intents/evorch/decisions/0024-process-owner-claim.md`
- `intents/evorch/decisions/0003-cache-first-context-engine.md`
- `intents/evorch/features/tools-sandbox/overview.md`

## Review focus

### 隔離と秘匿

- 一時領域の親ディレクトリが 0700 で、別のユーザーや別の ownership root の領域と混ざらないか。
- `HOME` とキャッシュのマウントが一時領域の中のままで、ホストの home や Cargo の資格情報が sandbox から見えるようになっていないか。
- 削除が NOFOLLOW のままで、一時領域の中のシンボリックリンクから外のファイルを消さないか。

### 寿命

- `Claimed` で空にする処理が、まだ前の owner のコマンドが書き込み中の領域を消さないか（handoff は quiesce の後であることを前提にしてよいか）。
- 起動時の削除が、生きている別プロセスの thread の領域を消さないか。`owner_may_be_live` が「不明」を生存として扱う方針に従っているか。
- 子 run が root と同じ領域を使い、root の結び直しの前後でパスが変わらないか。

### cache

- 案内の節が workspace note の書き換え対象に入っておらず、復元時にシステムメッセージが変わらないか。
- 既存の thread への案内と「空になった」の通知が、送信済み prefix の後ろへの追記になっているか。run ごとに 1 回だけか。

## Acceptance-to-evidence map

packet.yaml の acceptance_criteria の各項目に対応するテスト名を PR 本文に列挙する。
