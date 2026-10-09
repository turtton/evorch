# v12-conversation-artifacts Implementation Packet

## Goal

UI の相談でモック画像や HTML を会話上に掲載できるようにする。ADR 0029 の Phase 1 として、成果物の作成（`render_artifact`）と提示（`present`）を別権限の meta op として追加し、conversation transcript にカードを表示する。

## Why

文章だけではレイアウトや見た目の合意が取りにくい。既存の画像表示部品（`file_viewer` の loader）と root conversation 限定 tool の前例（`todo_write`）があり、Phase 1 は追加の外部依存なしで実装できる。

作成と提示を分けるのは、subagent の成果を常に会話の持ち主（Orchestrator）経由で見せ、Orchestrator が利用者の見たものを把握できるようにするためである。

## Scope

- **Event:** `event_bus::ToolEvent::ArtifactsPresented { run_id, call_id, presentation }`。`ArtifactPresentation { presentation_id, title, caption, artifacts: Vec<PresentedArtifact> }`、`PresentedArtifact { artifact_id, title, caption, media_type, path, byte_len, sha256 }`。fencing・transcript routing・storage projection の網羅 match に追加する。
- **Artifact store (runtime):** `AgentRuntime::with_artifact_store(dir)`。本体は `<dir>/blobs/<sha256>.<ext>`、メタデータは `<dir>/meta/<artifact_id>.json`（作成 run・root run・thread・元 path・title・caption・media type・サイズ・sha256・作成時刻）。未設定時は tool がエラーを返す。
- **`render_artifact`:** `{path, title, caption?}`。`executor.default_cwd()` で相対 path を解決し、canonicalize 後に workspace 内の通常ファイルであることを確認する。対応形式は png / jpeg / gif / webp / html。拡張子とマジックバイトの一致を確認する。上限は画像 8 MiB、HTML 2 MiB。HTML は secret-guard で検査し、検出時は拒否する。結果は `{artifact_id, title, media_type, bytes, reference}`。
- **`present`:** `{artifact_ids (1..=8), title?, caption?}`。`todo_write` と同じ root conversation 条件と ownership guard を通す。各 artifact の root run が呼び出し元と一致するか、artifact の thread が呼び出し元の thread と一致する場合だけ受け付け、1 件でも不正なら何も提示しない。
- **Policy:** `render_artifact` は General purpose の Worker で category が `visual`、または conversation かつ root かつ category が `conversation` の Worker。`present` は conversation かつ root の Worker / Orchestrator。dispatch 時にも同じ条件を再確認する。
- **GUI:** `TranscriptEntry::Artifacts`。カードに title / caption、画像の縮小プレビュー（`image_uri`）、HTML の種別表示、外部で開くボタンを置く。外部で開く操作は `AgentPaneAction` から app に渡し、portal（ashpd `open_uri`）で開く。ファイルが無い場合は利用不可と表示する。
- **Wiring:** `evorch-gui` は DB と同じディレクトリの `artifacts/` を store にする。demo は一時ディレクトリを使う。
- **Prompt:** `category-visual` overlay と tool 説明に、自己完結 HTML で作ること、subagent は `artifact_id` を返して Orchestrator が `present` することを記載する。
- **Docs:** `docs/conversation-artifacts.md`。

### 採用・却下案と理由

- 新しい Designer role は作らない。`visual` category の拡張で足り、ADR 0002 の role 追加の配線コストを避ける（利用者合意）。
- subagent の直接提示は採用しない。提示は会話の持ち主に限る（利用者合意）。
- artifact をイベント本文に base64 で埋め込む案は却下する。`max_event_bytes`（256 KiB）を超え、DB を肥大させる。
- 保存先を workspace 内にする案は却下する。利用者の repo を汚し、元ファイルの上書きで版比較ができなくなる。

## Out of scope

- HTML の Chromium スナップショット、複数 viewport、Browser pane で開く操作（Phase 2）。
- model への画像返却と自己確認（Phase 3）。
- `ask_user` の `artifacts` 指定（Phase 4）。
- webview の埋め込みと会話内での対話的 HTML 表示。
- artifact store の容量管理・GC、HTML が参照する外部 asset の同梱。

## Verification

### Unit

- runtime: policy の付与条件、`render_artifact` の path 検証・形式判定・サイズ上限・secret 検出・保存内容の不変性、`present` の権限・所有権・全件検証。
- event-bus: 新イベントの serde round-trip。
- gui: transcript への entry 追加と routing、カード描画（headless）、外部で開く action。

### Integration → build

- runtime の scripted model で visual Worker が作成し、root Orchestrator が提示する流れ。
- GUI の履歴復元でカードが再現されること。
- `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更 crate の `cargo nextest run`、`scripts/check-cache-contracts.sh`、`git diff --check`。

### 未確認事項

- portal による外部表示は実デスクトップでの手動確認が必要。headless では action の発行までを検証する。
