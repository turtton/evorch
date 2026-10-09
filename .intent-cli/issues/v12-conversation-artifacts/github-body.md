# Conversation への成果物提示 Phase 1（render_artifact / present と画像・HTML カード）

## Goal

UI の相談でモック画像や HTML を会話上に掲載できるようにする。ADR 0029 の Phase 1 として、成果物の作成（`render_artifact`）と提示（`present`）を別権限の meta op として追加し、conversation transcript に画像・HTML のカードを表示する。

## Why This Slice Exists Now

利用者との設計相談で ADR 0029 の方針に合意した。既存の画像表示部品と root conversation 限定 tool の前例（`todo_write`、PR #193）があり、外部依存を増やさずに最初の縦切りを実装できる。後続の HTML スナップショットや model への画像返却は、この保存・提示の基盤に依存する。

## Current Observed State

- `ToolResultContent` は Text のみで、transcript に成果物を表す entry はない。
- `file_viewer` の `LocalImageLoader` はローカル画像を表示できるが、会話から使う経路はない。
- `file://` の `OpenUrl` は `take_file_links` で in-app viewer に横取りされる。
- `visual` worker category は存在するが、追加 tool は付与されていない。

## Accepted Baseline You May Assume

- 新しい role は作らず、`ExecutionPolicy::for_run_config` で category と root 判定から tool を付与する。
- 提示は会話の持ち主（conversation の root Worker / root Orchestrator）だけが行う。subagent は `artifact_id` を返す。
- HTML は Phase 1 ではスナップショットを撮らず、カードと外部で開く操作だけを提供する。
- 復元は既存のイベント再生に乗せ、旧 run の権限を復活させない（ADR 0027）。

## Target Repo / Path / Part

- Target repo: `turtton/evorch`
- Target paths: `crates/event-bus/` `crates/runtime/` `crates/agents/` `crates/config/assets/presets/` `crates/storage/` `crates/gui/` `docs/` `intents/`
- Target part: artifact store、`render_artifact` / `present` meta op、policy、`ArtifactsPresented` イベント、transcript カード、外部で開く操作、GUI wiring、overlay・docs。

## In Scope

- `ToolEvent::ArtifactsPresented` と関連型、fencing・routing・projection への追加。
- content-addressed artifact store（`<dir>/blobs`、`<dir>/meta`）。
- `render_artifact`: workspace 内の png / jpeg / gif / webp / html を検証して保存。画像 8 MiB・HTML 2 MiB 上限、マジックバイト確認、HTML の secret 検出時は拒否。
- `present`: root conversation 限定、run 木（同じ root または同じ thread）の所有権検証、全件検証。
- Policy: `render_artifact` は `visual` Worker と conversation root Worker、`present` は conversation root Worker / Orchestrator。
- GUI: `TranscriptEntry::Artifacts` のカード、画像プレビュー、HTML 種別表示、portal で外部表示、ファイル欠落時の表示。
- `evorch-gui` の store 配線、`category-visual` overlay、tool 説明、`docs/conversation-artifacts.md`。

## Out Of Scope

HTML の Chromium スナップショットと複数 viewport、Browser pane で開く操作、model への画像返却、`ask_user` の `artifacts` 指定、webview 埋め込み、artifact store の容量管理・GC、HTML が参照する外部 asset の同梱、新しい role の追加、subagent による直接提示。

## Acceptance Criteria

1. `render_artifact` は `visual` category の Worker と conversation category の root Worker にだけ公開され、他では policy と dispatch の両方で拒否される。
2. `present` は conversation の root Worker / root Orchestrator にだけ公開され、子 run では拒否される。
3. `render_artifact` は workspace 内に実在する通常ファイル（png / jpeg / gif / webp / html）だけを受け付け、workspace 外・symlink 脱出・ディレクトリ・未対応形式・上限超過・拡張子と内容の不一致を拒否する。
4. 保存は呼び出し時点の内容で行い、元ファイルを変更しても保存済み artifact は変わらない。
5. HTML に secret が検出された場合は保存せず拒否する。
6. `present` は同じ root または同じ thread の artifact だけを受け付け、別 thread や存在しない id を含む場合は何も提示しない。
7. 提示時に `ArtifactsPresented` が root run の id 付きで発行され、thread transcript にカードが表示される。
8. カードは title / caption、画像の縮小プレビューまたは HTML の種別表示、外部で開く操作を持つ。ファイル欠落時は利用不可と表示する。
9. 再起動後のイベント再生でカードが復元される。
10. tool 結果に `[artifact <id>: <title>]` の参照テキストが含まれる。
11. overlay と tool 説明に、自己完結 HTML で作ること、subagent は id を返し Orchestrator が提示することを記載する。
12. `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更範囲の `cargo nextest run`、`scripts/check-cache-contracts.sh`、`git diff --check` が pass する。

## Verification

1. Unit: runtime の policy・`render_artifact`・`present` テスト、event-bus の serde round-trip、gui の transcript / routing / カード描画 / action テスト。
2. Integration: scripted model で visual Worker が作成し root Orchestrator が提示する流れ、GUI 履歴復元でのカード再現。
3. Build / lint: `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`scripts/check-cache-contracts.sh`、`git diff --check`。
4. 手動: 実デスクトップで外部表示（portal）が動作すること。未実施の場合は PR に未検証と明記する。

## Related Links

- `intents/evorch/decisions/0029-conversation-artifacts.md`
- `intents/evorch/features/gui-workbench/overview.md`
- `intents/evorch/features/orchestration/overview.md`
- 前例: PR #193（`todo_write`）

## Base Branch Policy

`main` から作業ブランチを切り、PR で `main` にマージする。
