# ADR 0029: Conversationへの成果物提示（画像・HTMLモック）

## Status

Accepted（2026-10-09）。利用者との設計相談で方針に合意した。webviewによる対話的なHTML表示は保留し、後で再検討する。

## Context

UIの相談では、文章だけでなくモック画像やHTMLを見せながら合意を取りたい。Claudeのcanvasのように、agentが任意の画像やHTMLを会話上に掲載できる仕組みが必要である。

既存の部品は揃いつつある。`ContentBlock::Image`とcomposerの画像添付、`file_viewer`の画像loader、opt-inの`browser` feature（headless Chromiumのscreencast・screenshot）、外部ブラウザを開くOpenURI portal（`ashpd`）、`visual` worker category、`MultimodalLooker`がある。一方、tool結果は`ToolResultContent::Text`だけで、transcriptに成果物を表す項目はなく、egui内でHTMLを描画する手段もない。

[ADR 0002](0002-role-capability-boundaries.md)はroleを権限の境界として定義する。新しいroleを作らず、`ExecutionPolicy::for_run_config`がruntime由来のcategoryとroot判定で追加toolを付与する既存の仕組み（conversationの`web_search`、`todo_write`）に載せる。

関連feature: [GUI Workbench](../features/gui-workbench/overview.md)、[Orchestration](../features/orchestration/overview.md)、[Tools & Sandbox](../features/tools-sandbox/overview.md)。

## Decision

### 作成と提示を別の権限にする

- **`render_artifact`（仮称）**: workspaceまたはscratch配下のファイルを、呼び出し時点の内容でartifact storeへ保存し、`artifact_id`を返す。HTMLはheadless Chromiumでスナップショットを撮る。viewport（例: desktop / mobile）を複数指定できる。会話には表示しない。
- **`present`**: `artifact_ids`とtitle・captionを受け取り、conversationに成果物として表示する。
- 提示は会話の持ち主が行う。subagentは成果物を作って`artifact_id`を結果として返し、Orchestratorがまとめて提示する。subagentが直接会話に掲載することはない。

### 付与先

- `render_artifact`: `visual` categoryのWorker、conversation categoryのroot Worker。
- `present`: conversationのroot Worker（Direct run）とroot Orchestratorだけ。`todo_write`と同じ条件である。
- categoryやrootかどうかはruntimeが判定し、modelの入力から決めない。全roleには付与しない。新しいroleは作らず、`visual` categoryを拡張する。モック作成の指針は`category-visual` overlayに追加する。将来モック作成と実装で別modelを割り当てたくなった場合は、categoryの分割を検討する。
- `present`は自分のrun木（[ADR 0022](0022-parent-child-tree-addressing-and-nested-delegation.md)）に属するartifactだけを受け付ける。別threadのartifactは拒否する。

### 保存と復元

- Artifactは`present`ではなく`render_artifact`の時点で、content-addressedに保存する。元ファイルが上書きされてもv1・v2の比較ができる。
- 提示はイベントとして記録し、thread復元で再表示する（[ADR 0027](0027-restore-contract.md)）。Compaction後は`[artifact <id>: <title>]`の参照テキストとして残す。
- サイズ上限を設け、保存前にsecret-guardを通す。

### HTMLの表示

- Conversationにはスナップショット画像をカードとして表示する。操作したい場合は外部ブラウザかBrowser paneで開く。
- `browser` featureなしのビルドでは、スナップショットを撮らず、カードと外部で開く操作だけを提供する。
- レンダリングはoffline（`file://`のみ、ネットワーク遮断）で行う。
- egui内でのwebview埋め込みや、会話内での対話的なHTML表示は今回採用せず、後で再検討する。

### 段階

1. 画像: artifact store、`render_artifact`、`present`、transcriptのカード、外部で開く操作、復元。
2. HTML: Chromiumによる複数viewportのスナップショット、Browser paneで開く操作、`browser` featureなしの代替表示。
3. 自己確認: `render_artifact`のスナップショットをmodelへ返す。Tool結果への画像対応が必要で、providerごとの差はtool結果直後に画像付きuser messageを追加する形で吸収する（[ADR 0020](0020-canonical-message-normalization.md)）。対応までは`MultimodalLooker`への委譲で代替できる。
4. `ask_user`の`artifacts`指定: モックを並べた状態でA案・B案を選べるようにする。

## Consequences

UIの相談でモックを見せながら合意を取れる。提示の権限が会話の持ち主に限られるため、subagentの成果は常にOrchestratorを経由し、Orchestratorは利用者が何を見たかを把握できる。HTMLは静的なスナップショットが中心になり、操作には外部ブラウザかBrowser paneへの切り替えが必要になる。Artifactの保存領域とサイズ管理が新たに必要になる。
