# v14-g-allowed-directories-prompt Implementation Packet

## Goal

許可ディレクトリの一覧を、run の開始時にシステムメッセージで伝え、途中の変更は末尾への追記で伝える。prompt cache の契約を保つ。

## Why

エージェントが許可ディレクトリを知らなければ、v14-f で使えるようになっても使われない。システムメッセージを書き換える方式では、スレッドの途中で一覧が変わるたびに cache が失われる。

## Scope

- **節:** `ContextSectionKind` に許可ディレクトリの節を加え、workspace note の近くに並べる。`compose_system_sections` が run の開始時の一覧（パスとアクセス権）を描画する。一覧が空なら節を出さない。preview（`base_context/preview.rs`）は同じ sections を使うので、inspector にも同じ内容が出る。
- **固定:** 節は `push_initial_system_message` でだけ書く。`update_workspace_system_message` のような書き換えはしない。復元された run は保存済みの履歴をそのまま使う。
- **最後に伝えた一覧:** `LoopState` に保持する。開始時の節を書いた時点で、その一覧を最後に伝えた一覧とする。`RunRestoreDescriptor` に `#[serde(default)]` で保存する。
- **追記:** `append_todo_context` と同じ境界（完全なツールラウンドの後、次のプロバイダ入力の直前）で、現在の一覧と最後に伝えた一覧を比べ、差分があれば 1 つのユーザー側メッセージ（例: `[workspace update] 追加: /path (read-only)` / `削除: /path`）を追記し、最後に伝えた一覧を更新する。未完了の ToolUse がある間は追記しない。
- **復元:** 復元時は、保存した一覧と現在の一覧の差分を同じ境界で追記する。
- **compaction:** `compaction/mod.rs` で `todo_context_pending` を立てる 2 か所と同じ位置で、許可ディレクトリの書き直しを予約する。次の境界で現在の一覧を全件書き直し、それを最後に伝えた一覧とする。
- **Docs:** `docs/`。

### 採用・却下案と理由

- システムメッセージを書き換える案は却下する（ADR 0032）。送信済みの prefix が変わり、cache が失われる。
- ツールで都度問い合わせる案は却下する。エージェントが呼ばない限り存在に気づかない。
- 変更のたびに即座に追記する案は採らない。ToolUse と ToolResult の間に割り込むと、プロバイダの形式を壊す。

## Out of scope

ルールと skills の読み込み（v14-h）、受け渡しとマウント（v14-f）。

## Verification

### Unit

- 差分の計算（追加・削除・アクセス権の変更・変化なし）。
- 節の描画と、空の一覧で節が出ないこと。
- 復元時に保存した一覧から差分が出ること。

### Integration → build

- `crates/runtime/tests/` の cache の contract テストに次を加える。input-derived の mock cache と、wire prefix の独立した照合を使う（固定の cached-token だけで保存を主張しない）。
  - 途中で許可ディレクトリを追加しても、以前の wire prefix がそのまま残る。
  - 差分を追記した後のターンで、その prefix が再利用される。
  - 復元の後、システムメッセージが変わらず、差分だけが追記される。
  - compaction の後、新しい prefix が以後のターンで保たれる。
- 待機はイベント駆動で行い、sleep に頼らない（test-policy）。
- `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更 crate の `cargo nextest run`、`scripts/check-cache-contracts.sh`、`git diff --check`。
