# v14-d-thread-scratch Implementation Packet

## Goal

shell の一時領域を thread 単位の固定パスにし、shell とファイルツール、親 run と子 run の間で `$TMPDIR` を通して一時ファイルを受け渡せるようにする。

## Why

一時領域のパスが sandbox ごとにランダムで、sandbox の中では `/tmp` という別名でしか見えないため、ファイルツールや子 run、昇格した shell と一時ファイルを共有できない。v14-e でファイルツールの境界を入れると、パスが決まっていない一時領域は境界に含められない。

## Scope

- **一時領域の管理:** runtime に thread 単位の一時領域のレジストリを置く。キーは `bind_thread_root` で結ばれた thread id、結ばれていない root run は root run ID とする。run の sandbox を作るとき（`agent_loop.rs` の shared 分岐の `factory.build`、`set_default_cwd` での executor の作り直し）に、`run_root` でたどった root のキーから一時領域を引いて渡す。
- **パス:** `/var/tmp/evorch-scratch-<uid>-<ownership root のハッシュ>/<キー>/` のように、ownership root と thread のキーだけで決める。親ディレクトリは 0700 で作り、キーはファイル名として安全な形に正規化する。
- **sandbox:** `Scratch` の生成を `BwrapSandbox::detect_with_program` から外し、`BwrapConfig` で一時領域のパスを受け取る。`build_argv` はそのパスをホストと同じパスと `/tmp` の両方に `--bind` し、`wrap` は `TMPDIR` をそのパスにする。`HOME=/tmp/home` と Cargo / rustup のキャッシュのマウントは変えない（ホストの home や資格情報を見せないため）。一時領域を渡されない構成（テストなど）は、従来どおり sandbox ごとの領域にフォールバックしてよい。
- **昇格した shell:** `unsandboxed` で実行するとき、`CommandSpec::extra_env` で同じ `TMPDIR` を渡す。
- **システムメッセージ:** 新しく始まる run の最初のシステムメッセージに、`$TMPDIR` の実パスと「`/tmp` と `~` は shell とファイルツールで指す先が違う。一時ファイルは `$TMPDIR` の実パスで扱い、並列の run と衝突しない名前を付ける」という節を加える。workspace note（`is_workspace_note` で書き換え対象になる）とは別の接頭辞にし、`update_workspace_system_message` の書き換え対象に含めない。base context のプレビュー（`base_context/preview.rs`）にも同じ節を出す。
- **既存の thread:** 復元した run のシステムメッセージにこの節がない場合は、システムメッセージを変えず、次のモデル呼び出しの前に同じ案内を `push_user` で末尾に追記する。
- **寿命:** GUI の ownership 配線から runtime に、thread の一時領域を「空にする」「削除する」操作を呼ぶ。
  - `OwnerHost::start` / `claim` が成功したとき（`Claimed`）: 使い始める前に空にする。
  - `release_ready` で `Released` になったとき、`handoff` で手放したとき: 削除する。
  - 起動時: 親ディレクトリの下で、生きている owner がいない thread（registry にない、または `owner_may_be_live` が false）の領域を削除する。
  - ownership を使わない構成: runtime の破棄時に、作った領域を削除する。
  - 削除は既存の `cleanup_tree`（NOFOLLOW でシンボリックリンクをたどらない）を使い、UI スレッドで走らせない。
- **空になった通知:** 空にした thread の run が次にモデルを呼ぶ前に、「一時領域は空になった」と `push_user` で末尾へ追記する。追記は run ごとに 1 回だけにする。
- **Docs:** `docs/harness-reliability.md` の Sandbox `/tmp` の節。

### 採用・却下案と理由

- キーに root run ID だけを使う案は採らない。復元や escalation で同じ thread に別の root run が結び直されると、パスが変わってシステムメッセージの案内と食い違うためである。thread id が結ばれていれば thread id を使う。
- 案内を workspace note に混ぜる案は採らない。workspace note は復元時に書き換えられるので、文面を変えると既存の thread の prefix が復元のたびに変わる。
- ファイルツールの `/tmp` を一時領域へ読み替える案は採らない（ADR 0033）。同じパスで同じファイルを見せれば変換は要らない。

## Out of scope

ファイルツールのパス境界と昇格（v14-e）。許可ディレクトリのマウント（v14-f）。一時領域の容量制限。

## Verification

### Unit

- パスの導出: thread id が結ばれている場合と結ばれていない場合、root run の結び直しの前後で同じパスになること、ownership root が違えば親ディレクトリが分かれること、0700。
- bwrap の argv: 一時領域がホストと同じパスと `/tmp` に bind され、`TMPDIR` がそのパスになること。`HOME` と キャッシュのマウントが変わらないこと。
- `unsandboxed` の実行で `TMPDIR` が渡ること。
- ownership イベントに応じて、空にする・削除する処理が走ること。待ち合わせは sleep ではなく、イベントの受信や処理の完了通知で行う。

### Integration → build

- 親 run の shell が `$TMPDIR` に書いたファイルを、子 run の shell と `read` が同じパスで読めること。別 thread の run からは見えないこと。
- 空にした thread の復元で、通知が 1 回だけ末尾に追記されること。
- `crates/runtime/tests/cache_preservation_e2e.rs` に、案内の追記と空にした通知の追記を含むシナリオを足し、以前の wire prefix が保たれることを確かめる。
- `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更 crate の `cargo nextest run`、`scripts/check-cache-contracts.sh`、`git diff --check`。

### 未確認事項

- bwrap で同じディレクトリをホストと同じパスと `/tmp` の 2 か所に bind し、さらに `/tmp/home/.cargo` 以下へキャッシュを ro-bind したとき、マウントポイントの作成がホスト側の一時領域に残す空ディレクトリの扱いは実装時に確かめる。
