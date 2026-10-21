# ADR 0037: thread 単位の読み取り専用ディレクトリで、self-improvement の draft を調査 agent に読ませる

## Status

Accepted（2026-10-21）。利用者との設計相談で方針に合意した。[ADR 0006](0006-self-improvement-and-diagnostics.md) の自己改善の調査を、サンドボックスを緩めずに進めるための経路を定める。

## Context

self-improvement の draft は `~/.config/evorch/self-improvement/drafts/` にある。ここはプロジェクトルートの外で、サンドボックスの外でもある。

- 調査する agent は、draft を読むたびに `sandbox_access: "unsandboxed"` の承認が必要になる（[ADR 0033](0033-file-tool-path-boundary.md)）。draft が複数あると、承認の往復が調査の妨げになる。
- 利用者は、draft を読むためだけの専用ツールを増やしたくない。ツールが増えると、role の tool 集合（[ADR 0002](0002-role-capability-boundaries.md)）と権限の面が増える。
- プロジェクトの許可ディレクトリ（[ADR 0032](0032-project-allowed-directories.md)）は project 単位で、role に従って読み書きできる。draft の調査のためにプロジェクトの設定を恒久的に広げるのは、権限として広すぎる。

そこで、draft を読ませたい thread だけに、一時的に読み取りだけを許可する。

## Decision

### thread が `extra_read_dirs` を持つ

- thread ごとに `extra_read_dirs`（ディレクトリのパスの一覧）を持つ。`ThreadRecord` のフィールドとして、sidebar JSON に永続化する。
- runtime は、thread id をキーにこの一覧を保持する。GUI が宣言し、runtime は run を起動するときに読む。
- run の起動時に、この一覧を `WorkspaceMounts.ro_binds` に加え、[ADR 0033](0033-file-tool-path-boundary.md) の境界の組み立てに合流させる。
  - 境界は 1 か所で組み立てられるので、shell の bwrap のマウントとファイルツールの判定の両方に効く。
  - 昇格なしで `read` / `grep` と shell の読み取りが通る。
- 一覧は role に関係なく、常に読み取り専用にする。書き込みツールを持つ role でも読み書きにはならない。ro mount なので、shell からも書き込めない。
- System メッセージの Allowed directories 節に、`(read-only)` として載せる。載せ方は ADR 0032 の許可ディレクトリと同じ節にそろえる。

### ADR 0032 との違い

| | ADR 0032 の許可ディレクトリ | この ADR の `extra_read_dirs` |
|---|---|---|
| 単位 | project | thread |
| アクセス権 | role に従う（読み書き / 読み取り専用） | 常に読み取り専用 |
| 設定する者 | 利用者が project ダイアログで登録し、trust する | GUI の特定の導線が自動で設定する |

`extra_read_dirs` は ADR 0032 の一覧を置き換えない。両方あれば、境界には両方が入る。project の allowed dir が thread dir と同一・配下の場合は、後から適用する thread の RO を優先する。

### 継承の範囲

- 継承する: その thread の子 run、escalation 先の thread（copy）、fork / rewind で作る thread。
- 漏らさない: 別の thread、別の project。
- archive しても保持する。thread の記録が消えたときに、一緒に消える。
- restore では、sidebar の `extra_read_dirs` から宣言し直す。restore descriptor には権限を持たせない（[ADR 0027](0027-restore-contract.md)）。権限の源は、常に sidebar に残った thread の記録だけである。

### 安全策

追加できるディレクトリには、次の制限をかける。

- `/` と `$HOME` は拒否する。
- scratch、tool 出力の保存先、プロジェクトルートの祖先・同一・配下を除外する。既存の RW 領域を RO と誤表示したり、別の権限を持つ領域を意図と違う形で包んだりしない。GUI は project root と tool 出力を事前判定し、runtime は実際の scratch を含めて再判定する。
- シンボリックリンクは実体（canonicalize 後のパス）で判定する。run 起動時の canonical path と、Unix では dev/inode を保持する。shell の各 bind 構築時と file tool の各境界取得時に再照合し、別の実体・欠落なら fail-closed で除外する。差し替え後の実体を新しい許可ルートにしない。通常の ro_binds と project allowed directories の挙動は変更しない。
- FD による実体の固定まではしないため、照合と open/bwrap bind の間の TOCTOU、既に実行中の shell のアクセス撤回、inode 再利用までは保証しない。
- DB は含めない。現在使用する DB ファイル（および -wal/-shm 等）が draft_dir の配下にある場合、作成時のボタンと作成処理を拒否し、保存済み thread の再宣言時にもその dir を除外する。runtime でも RunStore の DB の位置を保護する。DB の内容を読ませるのは、`evorch inspect` の経路（read-only で開く）の役割である。

### 非対象

- `LivePaths`（実行中に変わる許可）にはしない。run の起動時に thread の一覧を読んで境界に合流させるだけで、実行中の run への反映は扱わない。
- ツールは追加しない。既存の `read` / `grep` / shell の境界を広げるだけである。
- issue の投稿などの外部送信はしない。draft から GitHub Issue を作る経路は [ADR 0011](0011-release-and-diagnostics-ops.md) の運用のままで、この ADR は読み取りだけを許可する。

### GUI の導線

Self-improvement ペインに「Create triage thread (reads drafts)」ボタンを置く。押すと次を行う。

1. triage 用の thread を作る。
2. draft のディレクトリを `extra_read_dirs` に読み取り専用で付与する。
3. 入力欄に、調査プロンプトの雛形を入れる。送信は利用者が行う。

雛形の構成は次のとおり。

- 本文は日本語で書く。
- 候補の表は、英字のヘッダで書く。表は現在の Status フィルタに従い、最大 100 行とする。
- thread のヘッダに、読み取り専用のディレクトリが付いていることを示すバッジを表示する。

## Consequences

- 調査の agent は、draft の読み取りで承認を求めなくなる。承認の往復がなくなる代わりに、draft の内容が agent の文脈に入る。draft に含まれる文字列は、他の外部入力と同じく信用しない。
- 書き込みの権限は増えない。draft を直す必要があるなら、既存の昇格の経路（ADR 0033）を使う。
- thread ごとに権限が持ち越されるので、fork / rewind / escalation の経路は、`extra_read_dirs` を渡すことをテストで固定する。別の thread や別の project に漏れないことも同様にテストする。

## Related

- [ADR 0002](0002-role-capability-boundaries.md)、[ADR 0006](0006-self-improvement-and-diagnostics.md)、[ADR 0011](0011-release-and-diagnostics-ops.md)、[ADR 0027](0027-restore-contract.md)、[ADR 0032](0032-project-allowed-directories.md)、[ADR 0033](0033-file-tool-path-boundary.md)
- [features/tools-sandbox](../features/tools-sandbox/overview.md)
- [features/diagnostics-self-improvement](../features/diagnostics-self-improvement/overview.md)
