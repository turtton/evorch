# ADR 0033: 直接のファイルツールに shell と同じ境界と昇格の経路を持たせる

## Status

Accepted（2026-10-13）。利用者との設計相談で方針に合意した。[ADR 0031](0031-remove-isolated-workspaces.md) と [ADR 0032](0032-project-allowed-directories.md) の Open questions に残した「直接のファイルツールのパス制限」を閉じる。

## Context

`shell` と `git_diff` は bwrap の中で動くが、`read` / `write` / `edit` / `grep` は evorch の本体プロセスで、利用者と同じ権限のまま動く（`crates/tools/src/executor.rs` の `with_standard_tools_in`）。

- 承認の判定はツール単位で、パスは見ていない（`crates/sandbox/src/policy.rs` の `classify`）。
- 相対パスは cwd を基準に解決されるが（`scoped_args`）、絶対パスならどこにでも届く。`~/.ssh` を読むことも、プロジェクトの外へ書き込むこともできる。
- パスを閉じ込める仕組み（`crates/tools/src/executor/workspace_boundary.rs`）はあるが、benchmark のときだけの opt-in である。

[features/tools-sandbox](../features/tools-sandbox/overview.md) の「Worker = outside denied」は shell では守られているが、ファイルツールでは守られていない。ADR 0031 で isolated workspace をやめると、守るべき境界はプロジェクトルートと許可ディレクトリになるので、ファイルツールの穴がそのまま境界の穴になる。

shell には、呼び出しごとに sandbox を外してもらう経路がすでにある（`crates/tools/src/tools/shell.rs`、`crates/tools/src/tools/shell_escalation.rs`）。

- 引数に `sandbox_access: "network" | "unsandboxed"` と `justification` を付けて呼ぶ。既定は `"isolated"`（sandbox の中で実行）である。
- `ShellEscalationGate` が 1 回ごとに審査し、承認されたときだけ sandbox なしで実行する。
- 審査の方法は `sandbox.escalation_approval` で決まる。`auto` は別のモデルによる自動審査、`user` は利用者の承認、`off` は常に拒否である。`escalate_to_user_on_deny` で、自動審査の拒否を利用者に回せる。

## Decision

### 境界は shell の sandbox から見える範囲にそろえる

ファイルツールが昇格なしで触れる範囲を、shell の sandbox がマウントしている範囲と同じにする。パスはホスト側のパスで判定する。

- 読み書き可: プロジェクトルート、読み書き可の許可ディレクトリ（[ADR 0032](0032-project-allowed-directories.md)）、その thread の一時領域（下記の scratch）。
- 読み取り専用: bwrap の既定の読み取り専用パス（`/usr` `/bin` `/lib` `/lib64` `/etc` `/nix`）、ツール出力の保存先（`crates/tools/src/output.rs` の `output_root`）、shell に渡しているビルドキャッシュ（Cargo の registry / git / bin、rustup の toolchains）、読み取り専用の許可ディレクトリ。
- どの領域を読み取り専用にし、どこを読み書き可にするかは、ADR 0032 と同じく role の権限に従う。
- 境界の一覧は 1 か所で組み立て、bwrap のマウントとファイルツールの判定の両方がそれを使う。片方だけ変わってずれることを防ぐためである。

### 境界の外は、shell と同じ gate で 1 回ごとに昇格する

- `read` / `grep` / `write` / `edit` に、shell と同じ名前の引数 `sandbox_access` と `justification` を加える。値は `"isolated"`（既定。境界の中だけ）と `"unsandboxed"`（境界の外を許す）の 2 つとする。ファイルツールはネットワークを使わないので、`"network"` は持たない。
- `"unsandboxed"` の呼び出しは、shell と同じ審査の gate を通す。`ShellEscalationGate` を shell 専用からツール共通の gate に広げ、審査役にはツール名、対象のパス、理由を渡す。`write` / `edit` の場合は、書き込む内容（長さを制限した差分）も渡す。
- 審査の方法は shell と共通で、`sandbox.escalation_approval`（`auto` / `user` / `off`）と `escalate_to_user_on_deny` に従う。`off` のときは境界の外へのアクセスをすべて拒否する。
- 承認は 1 回の呼び出しに限る。同じ場所に何度もアクセスする必要があるなら、そのディレクトリを許可ディレクトリとして登録してもらう。拒否するときのメッセージで、昇格の引数と許可ディレクトリの登録の両方を案内する。
- 昇格で外れるのはパスの境界だけで、role の権限（[ADR 0002](0002-role-capability-boundaries.md)）は外れない。書き込みツールを持たない role が、昇格で書き込めるようになることはない。

### パスの判定

- 判定はパスを正規化してから行う。`..` を含むパスは字句のまま判定せず、存在する最も深い祖先を canonicalize して、シンボリックリンクで境界の外へ出ていないかを確かめる。`workspace_boundary.rs` の benchmark 用の判定を、複数の領域と読み取り専用の区別を扱えるように広げて使う。
- `grep` は、境界の中から始めても、シンボリックリンクをたどって境界の外へは出ない。
- benchmark では、これまでどおり固定した workspace の境界を使い、昇格は認めない。

### 一時領域（scratch）は thread 単位で共有し、shell とファイルツールで同じパスにする

2026-10-13 時点の scratch には次の問題がある。

- shell の sandbox を作るたびに `/var/tmp/evorch-scratch-<ランダム>/` を作り、sandbox の中の `/tmp` として読み書き可でマウントしている（`crates/sandbox/src/scratch.rs`、`crates/sandbox/src/bwrap.rs`）。shell の `HOME` も `/tmp/home` を指す。
- shell の `/tmp/foo` の実体はホストの `/var/tmp/evorch-scratch-<ランダム>/foo` で、ファイルツールの `/tmp/foo` はホストの `/tmp/foo` を指す。shell で作った一時ファイルをファイルツールで読めない。
- `unsandboxed` に昇格した shell はホストの `/tmp` を見るので、同じ run の中でも sandbox の中と外で `/tmp` が食い違う。
- shared run は run ごとに sandbox を作り直すので、scratch も run ごとに別になり、親 run と子 run の間で一時ファイルを渡せない。
- パスがランダムなので、ファイルツールの境界にも入れられない。

これを次のように改める。

- scratch は thread ごとに 1 つとし、その thread の root run と、その下の子 run はすべて同じ scratch を使う。キーには `bind_thread_root` が結んだ thread id を使い、thread に結ばれていない run は root run の ID を使う。復元やエスカレーションでは同じ thread に別の root run が結び直されるので、root run の ID をキーにすると途中でパスが変わってしまうためである。並列の run どうしで同じファイル名を使うと衝突するので、プロンプトでは一時ファイルに固有の名前を付けるよう案内する。
- パスは thread から決まる固定のものにする。例: `/var/tmp/evorch-scratch-<uid>-<ownership root のハッシュ>/<thread id>/`（ownership root は [ADR 0024](0024-process-owner-claim.md) の registry の置き場所。`$XDG_STATE_HOME/evorch/threads`）。親ディレクトリは 0700 とする。パスが復元の前後で変わらないので、システムメッセージに載せても Stable Prefix が崩れない。
- sandbox の中では、scratch をホストと同じパスでマウントし、`TMPDIR` をそのパスにする。`/tmp` を直接使うプログラムのために、`/tmp` も同じ scratch に bind する。`unsandboxed` に昇格した shell でも `TMPDIR` を同じパスにする。
- ファイルツールの境界に、その thread の scratch を読み書き可として加える。shell とファイルツールが同じパスで同じファイルを見るので、パスの変換は要らない。
- システムメッセージでは、workspace note とは別の節で「一時ファイルは `$TMPDIR`（実際のパス）に置き、ファイルツールでもそのパスを使う。`/tmp` と `~` は shell とファイルツールで指す先が違う」と案内する。workspace note は復元のたびに書き換えられる（`update_workspace_system_message`）ので、そこには入れない。この案内を持たない既存の thread には、システムメッセージを変えずに次のターンで末尾へ追記する。shell の `HOME` は、利用者の本物の home や資格情報を見せないため、これまでどおり scratch の中に置く。
- 中身の寿命は、その thread の owner プロセス（[ADR 0024](0024-process-owner-claim.md)）が持っている間だけとする。複数の evorch プロセスが同じデータディレクトリを使い、thread ごとに owner が分かれうるため、単位はプロセス全体ではなく thread とする。
  - owner が thread を手放すとき（プロセスの終了、安全な停止、handoff）に、その thread の scratch を削除する。
  - プロセスが thread の owner になったとき（起動後の復元や、クラッシュした owner からの claim）は、使い始める前にその thread の scratch を空にする。前の owner の一時ファイルを引き継がない。
  - 起動時には、生きている owner がいない thread の scratch を削除する。クラッシュしたまま誰も claim しなかった thread の分が残り続けないようにするためである。
- 再起動や claim の後に thread を復元すると、scratch は同じパスで空の状態から始まる。その場合は、次のターンで「一時領域は空になった」と末尾へ追記する（[ADR 0032](0032-project-allowed-directories.md) の途中変更の追記と同じ経路）。

### 「isolated」という名前について

shell とファイルツールの `sandbox_access: "isolated"` は「sandbox の境界の中で実行する」という意味である。[ADR 0031](0031-remove-isolated-workspaces.md) で廃止する isolated workspace（runtime が run ごとに git worktree を作る仕組み）とは関係がない。ADR 0031 の削除作業で、`sandbox_access` の `"isolated"` を巻き込んで消さないこと。値の名前を `"sandboxed"` などに改めるかどうかは、この ADR では決めない。

## Consequences

- ファイルツールからも、プロジェクトと許可ディレクトリの外へ黙って読み書きできなくなる。shell とファイルツールで、触れる範囲と外に出る手順がそろう。
- 依存ライブラリのソース（Cargo の registry）や `/nix/store` は、昇格なしでこれまでどおり読める。
- `~/.config` の設定ファイルや、登録していない別のリポジトリなど、境界の外を読む作業には昇格が要る。`auto` の場合は審査役のモデル呼び出しの分だけ時間とコストがかかる。
- 4 つのツールのスキーマが変わる。ツールのスキーマは Stable Prefix の一部なので、変わるのは evorch を更新したときだけで、セッションの途中では変わらない。
- 境界を入れることと昇格の経路を作ることは、同じ変更で入れる。経路なしで境界だけ入れると、今動いている作業が止まるためである。
- shell とファイルツールの間、親 run と子 run の間で、`$TMPDIR` を通して一時ファイルを受け渡せるようになる。代わりに、同じ thread の並列の run どうしで一時ファイルの名前が衝突しうる。
- 一時ファイルは、再起動や owner の交代で消える。それをまたいで残したいものは、プロジェクトの中などに置く必要がある。
- 実装は 2 段に分ける。①scratch を thread 単位の固定パスに改め、`TMPDIR` とシステムメッセージの案内を入れる、②ファイルツールの境界と昇格の経路を入れる。②の境界は①の scratch を含むので、①を先に入れる。

## Related

- [ADR 0002](0002-role-capability-boundaries.md)、[ADR 0008](0008-threat-model-phased-adoption.md)、[ADR 0021](0021-bwrap-linux-sandbox.md)、[ADR 0024](0024-process-owner-claim.md)、[ADR 0031](0031-remove-isolated-workspaces.md)、[ADR 0032](0032-project-allowed-directories.md)
- [features/tools-sandbox](../features/tools-sandbox/overview.md)
