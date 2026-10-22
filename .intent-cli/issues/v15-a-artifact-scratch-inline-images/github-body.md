# ADR 0038 ①: render_artifact が一時領域のファイルを受け付け、HTML の相対パス画像を埋め込む

## Goal

`render_artifact` で、その thread の一時領域（`$TMPDIR`）のファイルも取り込めるようにする。HTML の取り込み時は、相対パスで参照している画像を data URI に置き換えて、自己完結した HTML として保存する。

## Why This Slice Exists Now

ADR 0038 で、画像生成の素材を HTML モックに組み込む方針に合意した。埋め込みを runtime が行えば、agent は普通の HTML と画像ファイルを書くだけで済む。この変更は画像生成（v15-c）に依存せず、普通の画像を載せたモックにも使えるので、先行して入れる。

## Current Observed State

- `read_workspace_file`（`crates/runtime/src/artifacts.rs`）は workspace の中だけを受け付ける。ADR 0029 は「workspace または scratch 配下」としていた。
- HTML は 2 MiB までで、外部ファイルは同梱されない。tool 説明と `category-visual` overlay は、画像をインライン化するよう指示している。
- run の一時領域は `LoopState::scratch_dir()` で得られ、ファイルツールの境界にも含まれている（ADR 0033）。

## Accepted Baseline You May Assume

- 取り込み元はワークスペースと、その run の thread の一時領域だけ。許可ディレクトリは含めない。
- 参照先に問題が 1 件でもあれば、一覧を示して拒否し、何も保存しない。
- 元の HTML は 2 MiB、埋め込み後は 8 MiB まで。secret_guard は元の HTML に適用する。

## Target Repo / Path / Part

- Target repo: `turtton/evorch`
- Target paths: `crates/runtime/` `crates/config/assets/presets/` `docs/` `intents/`
- Target part: `render_artifact` の境界と HTML の画像埋め込み、メタデータ、tool 説明・overlay・docs。

## In Scope

- 取り込み元の境界をワークスペースと一時領域にする（相対パスはワークスペース基準）。
- `<img src>` と CSS `url()`（style 要素・style 属性）の相対パス画像を data URI に置き換える。
- 埋め込む画像の形式・マジックバイト・8 MiB 上限の検査、境界の判定。
- 失敗時の参照一覧付きの拒否、埋め込み後の 8 MiB 上限。
- `ArtifactRecord.inlined_assets` の追加（旧形式と互換）。
- tool 説明、`category-visual` overlay、`docs/conversation-artifacts.md`。

## Out Of Scope

`srcset` / `<link>` / `<script src>` / フォントの同梱、許可ディレクトリからの取り込み、画像の縮小や形式変換、artifact store の容量管理。

## Acceptance Criteria

1. ワークスペースと、その run の thread の一時領域の中の通常ファイルを受け付け、別 thread の一時領域・許可ディレクトリ・境界の外・symlink による脱出を拒否する。相対パスはワークスペースから解決する。
2. HTML の `img src` と CSS `url()` の相対パス画像を、HTML ファイルのディレクトリから解決して data URI に置き換える。
3. 埋め込む画像は png / jpeg / gif / webp で、拡張子とマジックバイトが一致し、1 枚 8 MiB 以下、参照先がワークスペースか同じ一時領域の中にある。
4. 問題のある参照が 1 件でもあれば、一覧を示して拒否し、何も保存しない。
5. スキームを持つ参照（http / https / data など）、`#` だけの参照、空の参照は書き換えない。
6. 元の HTML は 2 MiB、埋め込み後は 8 MiB まで。secret_guard は元の HTML に適用する。
7. メタデータに埋め込んだ画像の元パスと sha256 を記録する。
8. 埋め込みのない HTML と画像は従来と同じ内容で保存される。
9. tool 説明と `category-visual` overlay に、相対パス画像の埋め込みと一時領域からの取り込みを記載する。
10. `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更範囲の `cargo nextest run`、`scripts/check-cache-contracts.sh`、`git diff --check` が pass する。

## Verification

1. Unit: 境界、埋め込み、失敗時の一覧、互換（従来と同じバイト列、旧 meta JSON）。
2. Integration: scripted model で `$TMPDIR` の画像を参照する HTML を取り込み、外部参照なしで表示できる。
3. Build / lint: `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`scripts/check-cache-contracts.sh`、`git diff --check`。

## Related Links

- `intents/evorch/decisions/0038-image-generation.md`
- `intents/evorch/decisions/0029-conversation-artifacts.md`
- `intents/evorch/decisions/0033-file-tool-path-boundary.md`

## Base Branch Policy

`main` から作業ブランチを切り、PR で `main` にマージする。
