# escalation 継承質問の同一 ID 表示と継承先回答認可

## Goal

escalation で継承された未回答ユーザー質問を、同一 question ID と元 requester provenance のまま継承先 Orchestrator thread に表示し、正当な thread から回答可能にする追加契約を記録する。

**PR #133（squash `eb79a7f`）で実装完了済み。本 packet は post-hoc の歴史的追跡であり、新規実装依頼ではない。** 以下は同 PR の実装差分に基づく本文相当の要約であり、元の GitHub PR 本文の転載ではない。

## Why This Slice Exists Now

元の Direct→Orchestrator handoff とは別に、継承質問の GUI 表示・回答認可・起動案内の追加契約を追跡する必要があった。完了済み PR #76 の `v02-direct-escalation-handoff` packet を歴史的証跡として保持し、当時の条件を遡及して書き換えないため、独立した follow-up とする。

## Current Observed State

基準 commit `eb79a7f` には PR #133 の event-bus / storage / runtime / GUI 実装と回帰テスト、および intent / ADR / docs 更新が含まれる。以前の不足は、durable な質問継承を宛先 thread の表示・回答経路に接続する契約が必要だったことである。本 packet は実装前に作られた計画書として扱わない。

## Accepted Baseline You May Assume

- durable `user_question_links` が canonical consumer routing。質問 ID と元 requester の `run_id/root_run_id/root_name` は不変。
- `recipient_run_ids` は serde default 付き projection であり、実行権限ではない。storage `get/list` は links を `ORDER BY rowid` で hydrate する。
- consumer routing と current ownership / permit は別々に検証する。event fence と first-answer-wins を維持する。質問への回答は tool permission を与えない。
- 質問継承と workspace 準備は source terminal と新 root startup より前に行い、失敗時は起動しない（ADR 0027）。

## Target Repo / Path / Part

- Target repo: `turtton/evorch`
- Target paths: crates/event-bus/ crates/storage/ crates/runtime/ crates/gui/ intents/ docs/
- Target part: PR #133 で実装済みの escalation 継承質問 UI・回答認可・handoff 案内契約。
- 本 packet 作成時の変更対象は `.intent-cli/issues/v02-escalation-question-inheritance/` の新規4ファイルのみ。当時の実装対象パスを再変更する指示ではない。

## In Scope

- `UserQuestion.recipient_run_ids` と共有 `is_user_visible/belongs_to_thread` helper により、GUI 表示と回答認可の consumer 条件を一致させた。
- storage `hydrate` で durable links を読み、stale payload に依存せず再起動・複数 continuation の recipient 順を保持した。
- 継承成功後に `UserQuestionUpdated` を再発行した。元 thread には質問 ID と移動先 thread/run の notice を表示し、同一 ID を継承先に表示した。
- `runtime_sink` は caller permit の thread/generation と `mutation_guard`、live recipient ownership を別途確認する。storage acknowledgement は古い event が fence されてもカードを消せるが、event fence 自体は緩和しない。
- Orchestrator 起動時に未回答 ID/provenance と既存質問への回答案内を注入した。回答到着・model 観測前の finish を拒否する。
- `meta/specs.rs` を `escalation/prompt.rs::TOOL_DESCRIPTION` に接続し、memo の合意済み制約・完了条件、採用/却下案と理由、未確認事項の記載を案内した。schema/order や各 run の provider prefix は維持した。

## Out Of Scope

新規実装、既存 packet の変更、`crates/`・`intents/`・`docs/` の再変更、質問の作り直しや requester の書換え、未回答の合意扱い、event fence / ownership guard の緩和、commit / push、branch / worktree 操作、GitHub への新規公開。

## Standalone Child Issue Contract

この記録が単独で読まれても、PR #133（`eb79a7f`）の完了済み実装を再実装タスクとして開始しないこと。成果は継承質問の同一 ID/provenance、正当 thread の回答認可、無関係 thread の拒否、handoff 案内、restart / continuation 維持を検証可能な契約として歴史的に紐付けること。元 `v02-direct-escalation-handoff` packet は依存元の歴史的証跡であり、上書きしない。

## Acceptance Criteria

`docs/harness-runtime-improvements.md:59-71` の6件を転記する。

1. An inherited unanswered question appears in the destination Orchestrator thread with its original question ID and requester provenance unchanged.
2. An answer to that existing ID from the legitimate destination thread is accepted after checking current recipient / ownership. After the model observes the answer, the inherited blocking question no longer prevents finish.
3. The source thread shows that the question has been handed off and identifies the destination. It does not create a replacement question ID.
4. An answer submitted from an unrelated thread is rejected, even when it names a valid inherited question ID.
5. Orchestrator startup explicitly lists inherited unanswered questions and directs the user to answer the existing questions, not ask again with new IDs.
6. Storage restart recovery and multiple continuations preserve question identity, original provenance, and tracking of the current recipient / ownership.

追加要件: handoff メモにユーザーと合意済みの制約・完了条件、採用/却下案と理由、未確認事項を記載する。未回答やメモにない事項を合意済みと推測しない。

## Verification

PR #133 の回帰テストに対応する再検証手順。今回の packet 作成での実行済み結果とは区別する。

1. Unit: `cargo test -p gui --lib app::questions::tests`、`cargo test -p gui --lib runtime_sink::tests`、`cargo test -p runtime --lib escalation::prompt::tests`。
2. Storage contract: `cargo test -p storage --test user_questions`（stale payload に対する links の優先、順序、reopen、明示継承、冪等回答）。
3. Integration: `cargo test -p runtime --test escalation_questions`、`cargo test -p gui --test escalation_questions`（表示、回答、finish、移動先 notice、継承失敗時の停止）。
4. Prefix regression: `cargo test -p runtime --test cache_preservation_e2e inherited_question_answer_preserves_each_runs_wire_prefix_after_escalation`。
5. Build: `cargo check -p event-bus -p storage -p runtime -p gui`。
6. Packet: intent-cli の利用可能な読み取り専用 validate、`git diff --check`、新規4ファイルのみで既存 packet が不変であることの確認。

実 GUI/display 検証や CI ログ未参照を pass と主張しない。

## Related Links

- 実装完了: https://github.com/turtton/evorch/pull/133 （squash `eb79a7f`）
- 依存元・歴史的証跡: `.intent-cli/issues/v02-direct-escalation-handoff/packet.yaml`（PR #76 完了。上書きしない）
- `intents/evorch/features/orchestration/overview.md`
- `intents/evorch/features/gui-workbench/overview.md`
- `intents/evorch/decisions/0027-restore-contract.md`
- `docs/harness-runtime-improvements.md`

## Base Branch Policy

`direct-main`。PR #133 は `main` に squash 済み。この作業は記録作成のみで、commit / push、branch 操作、新規 PR 作成を行わない。

## Additional Required Guidance

metadata の変更は intent-cli の対応コマンドに限り、queue / labels を直接編集しない。read-only guide を実行許可と取り違えず、実行契約による拒否を迂回しない。実装 child に親 host の packet / queue / intent tree 変更を委ねない。intent-cli から AI provider を起動しない。新たな振る舞い変更が必要なら、この歴史的 packet を実装前の計画に見せかけず別途合意する。
