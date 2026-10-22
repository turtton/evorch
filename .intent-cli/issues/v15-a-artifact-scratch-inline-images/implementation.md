# v15-a-artifact-scratch-inline-images Implementation Packet

## Goal

`render_artifact` で、その thread の一時領域（`$TMPDIR`）のファイルも取り込めるようにする。HTML を取り込むときは、相対パスで参照している画像を data URI に置き換え、自己完結した HTML として保存する。ADR 0038 の「`render_artifact` の拡張」の節を実装する。

## Why

画像生成（v15-c）で作った素材を HTML モックに組み込むには、HTML に画像を埋め込む必要がある。数 MB の base64 を model に手で書かせるのは現実的でなく、shell で組み立てさせると手順が増えて失敗しやすい。取り込み時に runtime が埋め込めば、agent は普通の HTML と画像ファイルを書くだけで済む。

参考用の素材や試作は repo を汚さないよう一時領域に置きたい。ADR 0029 は取り込み元を「workspace または scratch 配下」としていたが、実装は workspace だけなので、ADR に合わせる。

この変更は画像生成がなくても、普通の画像を載せたモックに使える。v15-c に先行して入れられる。

## Scope

- **取り込み元の境界:** `read_workspace_file` を、ワークスペースと run の一時領域（`LoopState::scratch_dir()`）の 2 つを境界として受け取る形にする。canonicalize 後の実パスがどちらかの中にあれば受け付ける。相対パスは従来どおりワークスペースから解決する。一時領域がない run（テストなど）はワークスペースだけを境界にする。
- **HTML の画像埋め込み:** HTML の取り込みで、次の参照を data URI に置き換える。
  - `<img src="...">`
  - `<style>` 要素と `style` 属性の CSS `url(...)`
  - 相対パスは HTML ファイルの実パスのディレクトリから解決し、canonicalize 後に同じ境界の中にあるか判定する。
  - 書き換えないもの: `http:` / `https:` / `data:` / `file:` などのスキームを持つ参照、`#` で始まる参照、空の参照。`srcset`、`<link>`、`<script src>`、`<iframe>` は今回扱わない（従来どおり、外部ファイルは同梱されない）。
- **画像の検査:** 埋め込む画像は既存の `ArtifactKind`（png / jpeg / gif / webp）の拡張子とマジックバイトの検査を通し、1 枚 8 MiB 以下とする。読み込みは既存と同じく `take(limit + 1)` で制限する。
- **失敗の扱い:** 境界の外・存在しない・形式が合わない・上限超過の参照が 1 件でもあれば、該当する参照（HTML 中の値と理由）の一覧を返して拒否し、何も保存しない。多すぎる場合は先頭の数件と件数にする。
- **上限:** 元の HTML は 2 MiB 以下（従来どおり）、埋め込み後の HTML は 8 MiB 以下（`MAX_BUNDLED_HTML_BYTES`）。secret_guard は元の HTML に適用する。
- **メタデータ:** `ArtifactRecord` に `inlined_assets: Vec<{source_path, sha256, byte_len}>` を加える（`#[serde(default, skip_serializing_if = "Vec::is_empty")]`）。既存の meta JSON は読み込める。
- **HTML の書き換え方法:** 推奨は `lol_html`（Cloudflare のストリーミング HTML 書き換え）を runtime の依存に加えること。要素の属性と style 要素のテキストを位置を保ったまま書き換えられる。CSS の `url()` は引用符あり・なし・エスケープを扱う小さな走査で十分とし、CSS パーサは入れない。依存を足さずに自前で走査する場合は、コメント・属性の引用符・大文字小文字の違いをテストで押さえる。
- **Prompt:** `render_artifact` の tool 説明と `category-visual` overlay を更新する。「HTML から相対パスで参照した画像は取り込み時に埋め込まれる。参考の素材や試作は `$TMPDIR` に置いてもよい」。既存の「画像をインライン化し」の記述は、相対パスで参照すればよいことに書き換える。
- **Docs:** `docs/conversation-artifacts.md` の Capture rules。

### 採用・却下案と理由

- 参照先が無いときに黙って飛ばす案は採らない。利用者が見るモックで画像が欠けても agent が気づかないためである。
- 埋め込みを agent の shell 作業に任せる案は採らない。手順が増え、`$TMPDIR` と `/tmp` の取り違えなどで失敗しやすい。
- 許可ディレクトリ（ADR 0032）まで取り込み元に含める案は今回採らない。成果物の置き場として必要になった時点で検討する。
- HTML の上限を一律に引き上げる案は採らない。元の HTML の 2 MiB は秘密情報の検査とモデルが書く量の目安として残し、埋め込み後だけ別の上限にする。

## Out of scope

`srcset` / `<link rel=stylesheet>` / `<script src>` / フォントの同梱。許可ディレクトリからの取り込み。画像の縮小や形式変換（v15-c で生成時に行う）。artifact store の容量管理。

## Verification

### Unit

- 境界: ワークスペース内・一時領域内の受け付け、別 thread の一時領域・境界の外・symlink による脱出・`..` の拒否、一時領域がない run。
- 埋め込み: `img src` と CSS `url()`（引用符あり・なし、大文字の要素名・属性名、style 属性）の置き換え、書き換えない参照（http / https / data / `#` / 空）、HTML ファイルのディレクトリ基準の解決。
- 失敗: 存在しない・境界外・形式不一致・上限超過の参照の一覧付き拒否と、何も保存されないこと。埋め込み後の上限超過。
- 互換: 埋め込みのない HTML と画像は従来と同じバイト列・sha256 で保存される。旧形式の meta JSON を読み込める。

### Integration → build

- scripted model で、visual Worker が `$TMPDIR` の画像を相対パスで参照した HTML を取り込み、保存された HTML が外部参照なしで表示できる（inspect_artifact か GUI の外部表示）。
- `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更 crate の `cargo nextest run`、`scripts/check-cache-contracts.sh`、`git diff --check`。

### 未確認事項

- `lol_html` を加えた場合のビルド時間とバイナリサイズへの影響は PR で報告する。
