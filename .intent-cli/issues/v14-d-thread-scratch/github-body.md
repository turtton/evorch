# ADR 0033 ①: 一時領域を thread 単位の固定パスにし、shell とファイルツールで共有する

## Goal

shell の一時領域（scratch）を thread 単位の固定パスに改め、shell とファイルツール、親 run と子 run の間で `$TMPDIR` を通して一時ファイルを受け渡せるようにする。ADR 0033 の後半（ファイルツールの境界、v14-e）はこの一時領域を境界に含めるので、その前提になる。

## Why This Slice Exists Now

shell の `/tmp` の実体は sandbox ごとにランダムに作る `/var/tmp/evorch-scratch-*` で、ファイルツールの `/tmp` はホストの本物の `/tmp` を指す。shell で作った一時ファイルを `read` で読めず、`unsandboxed` に昇格した shell からも見えない。shared run は run ごとに sandbox を作り直すので、親 run が `/tmp` に置いたファイルを子 run に渡すこともできない。パスがランダムなので、v14-e のファイルツールの境界にも入れられない。

## Current Observed State

- `Scratch::new`（`crates/sandbox/src/scratch.rs`）が sandbox ごとに `/var/tmp/evorch-scratch-<ランダム>/` を作り、`build_argv` が sandbox の `/tmp` にマウントする。`wrap` は `HOME=/tmp/home`、`TMPDIR=/tmp` を設定する。
- shared run は run ごとに `factory.build` で sandbox を作る（`crates/runtime/src/agent_loop.rs`）。
- `unsandboxed` の shell は `ReviewedHostSandbox` でホストの環境を継承し、ホストの `/tmp` と `TMPDIR` を見る。
- 中身は sandbox の破棄時に後始末スレッドで消え、クラッシュ時は OS 任せで残る。

## Accepted Baseline You May Assume

- thread の owner（ADR 0024）は実装済みで、`OwnerHost` が `Claimed` / `Handoff` / `Released` などの `OwnershipEvent` を流す。
- thread と root run の対応は `bind_thread_root` が持つ。復元や escalation で同じ thread に別の root run が結び直されることがある。
- 内部の永続化形式は後方互換を保たなくてよい（AGENTS.md の Compatibility policy）。
- ファイルツールのパス境界と昇格の経路は v14-e で入れる。この unit ではファイルツールの挙動を変えない（もともと制限がないので、ホスト側のパスで一時領域を読み書きできる）。

## Target Repo / Path / Part

- Target repo: `turtton/evorch`
- Target paths: `crates/sandbox/` `crates/runtime/` `crates/tools/` `crates/gui/` `docs/` `intents/`
- Target part: scratch の作り方と受け渡し、bwrap のマウントと環境変数、`unsandboxed` の shell の `TMPDIR`、システムメッセージの案内、owner に合わせた後始末。

## In Scope

- thread 単位の一時領域を管理する仕組み（例: runtime が持つレジストリ）。キーは `bind_thread_root` で結ばれた thread id、結ばれていない root run は root run ID とする。子 run は `run_root` で root をたどって同じ領域を使う。
- パスは ownership root から決まる親ディレクトリ（0700）と thread のキーだけで決める。例: `/var/tmp/evorch-scratch-<uid>-<ownership root のハッシュ>/<キー>/`。
- `BwrapSandbox` が scratch を自分で作るのをやめ、渡された一時領域をホストと同じパスと `/tmp` の両方にマウントし、`TMPDIR` をそのパスにする。`HOME` と Cargo / rustup のキャッシュのマウントはこれまでどおり一時領域の中に置く。
- `unsandboxed` の shell にも同じ `TMPDIR` を渡す。
- 新しく始まる run のシステムメッセージに、`$TMPDIR` の実パスと `/tmp`・`~` の違いを案内する節を加える。workspace note とは別の節にし、復元時に書き換えない。案内を持たない既存の thread の復元では、次のターンで末尾に追記する。
- owner に合わせた後始末: `Released` / `Handoff` で削除し、`Claimed` で使い始める前に空にする。起動時に、生きている owner がいない thread の領域を削除する。ownership を使わない構成では runtime の破棄時に削除する。
- 空にした状態で復元・claim した thread への「一時領域は空になった」の追記。
- `docs/harness-reliability.md` の Sandbox `/tmp` の節の書き換え。

## Out Of Scope

ファイルツールのパス境界と昇格（v14-e）。許可ディレクトリのマウント（v14-f）。一時領域の容量制限。

## Acceptance Criteria

1. 同じ thread の root run と子 run の shell が同じ一時領域を使う。別の thread は別の領域を使う。
2. パスは ownership root と thread のキーだけで決まり、復元や root run の結び直しの前後で変わらない。親ディレクトリは 0700。
3. sandbox の中で一時領域がホストと同じパスに見え、`TMPDIR` がそのパスになる。`/tmp` も同じ領域を指す。`unsandboxed` の shell の `TMPDIR` も同じ。ホストの home や資格情報は見えない。
4. 新しい run のシステムメッセージに `$TMPDIR` の実パスと案内が載る。復元時に書き換えず、案内のない既存 thread には次のターンで追記する。
5. `Released` / `Handoff` で削除、`Claimed` で空にしてから使う、起動時に owner のいない thread の分を削除する。ownership を使わない構成では runtime の破棄時に削除する。
6. 空にした thread を復元・claim したとき、次のターンで「一時領域は空になった」を末尾へ追記する。
7. cache の end-to-end シナリオで、2 種類の追記の前後で以前の wire prefix が保たれる。
8. `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更範囲の `cargo nextest run`、`scripts/check-cache-contracts.sh`、`git diff --check` が pass する。

## Verification

1. Unit: パスの導出（キーの選び方、ownership root ごとの分離、0700）、bwrap の argv（ホストと同じパスと `/tmp` の bind、`TMPDIR`）、`unsandboxed` の `TMPDIR`、ownership イベントに応じた削除と空にする処理。待ち合わせは sleep ではなくイベントや完了通知で行う。
2. Integration: 親 run の shell が `$TMPDIR` に書いたファイルを、子 run の shell と `read` が同じパスで読めること。別 thread からは見えないこと。
3. Cache: `crates/runtime/tests/cache_preservation_e2e.rs` に、案内の追記と空にした通知の追記を含むシナリオを足す。
4. Build / lint: 上記のコマンド一式。

## Related Links

- `intents/evorch/decisions/0033-file-tool-path-boundary.md`
- `intents/evorch/decisions/0024-process-owner-claim.md`
- `intents/evorch/features/tools-sandbox/overview.md`

## Base Branch Policy

`main` から作業ブランチを切り、PR で `main` にマージする。
