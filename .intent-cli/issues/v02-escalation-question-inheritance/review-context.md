# v02-escalation-question-inheritance Review Context

## Position and scope

**PR #133（squash `eb79a7f`）で実装完了済みの追加契約を記録する post-hoc packet。** 実装前の仕様書として作成されたものではなく、新規実装や Rust テスト再実行の pass 証明でもない。

依存元 `.intent-cli/issues/v02-direct-escalation-handoff/packet.yaml` は完了済み PR #76 の歴史的証跡であり、上書きしない。当時の handoff 契約へ追加条件を遡及して混ぜない。今回の差分は新規 packet 4ファイルに限定し、`crates/`、`intents/`、`docs/`、既存 packet の変更を拒否する。

## Intent references

- `intents/evorch/features/orchestration/overview.md`
- `intents/evorch/features/gui-workbench/overview.md`
- `intents/evorch/decisions/0027-restore-contract.md`
- `docs/harness-runtime-improvements.md`（受け入れ条件6件と handoff memo 契約）
- `.intent-cli/issues/v02-direct-escalation-handoff/packet.yaml`（依存元・歴史的証跡のみ）

## Packet versus PR delta

PR #133 は durable links 自体を新たな権限として導入したのではなく、既存の質問継承を UI 表示・回答認可・起動案内へ接続した。`UserQuestion` projection、storage hydrate、継承後イベント、GUI の consumer 条件と permit 検証、ToolSpec 説明の接続、回帰テスト、および intent/ADR/docs への書き戻しを含む。本 packet はそれらに新しい実装差分を要求しない。

## Review focus

### Consumer routing versus execution ownership

- canonical consumer routing は `user_question_links`。`recipient_run_ids` は `ORDER BY rowid` の読み取り projection であり、元 `id/run_id/root_run_id/root_name` を書き換えない。
- `get/list` の両経路が hydrate され、stale payload の recipient 値を正本にしないこと。複数 continuation の順序は再起動後も保持されること。
- `is_user_visible/belongs_to_thread` を GUI と sink が共有すること。元 thread と登録済み recipient に結び付く thread は同一質問を扱え、無関係 thread と非 user-visible な child 質問は除外されること。
- routing 一致だけで回答保存を認可しないこと。caller permit の thread/generation・`mutation_guard` と live recipient ownership は別に検証し、古い generation や無関係 thread の回答を永続化前に拒否すること。
- 最初の回答を固定し、同じ回答の再送は冪等とすること。質問回答を tool permission の承認へ転用しないこと。

### Event fence and startup ordering

- 継承成功後に hydrate 済み `UserQuestionUpdated` が同一 ID/provenance で再発行されること。
- 質問継承と workspace 準備 → source terminal → 新 Orchestrator root startup の ADR 0027 ordering を維持し、準備失敗時に起動しないこと。
- storage acknowledgement (`UserAnswerSaved`) によるカード除去と run event fence を区別すること。古い run の event が拒否されても保存済み回答を UI に反映できるが、event fence 自体は維持すること。

### Compatibility and handoff guidance

- `recipient_run_ids` は `#[serde(default)]` により旧 event/payload を受理し、既存 source-thread 発見経路を保つこと。projection を持たない古い記録の hydration を確認すること。
- Orchestrator 起動時に未回答質問の既存 ID/provenance と回答案内を明示し、新 ID で再質問しないこと。回答到着だけでは足りず、model 観測まで blocking finish を拒否すること。
- 元 thread の notice に既存質問 ID と移動先 thread/run が含まれること。
- `meta/specs.rs` → `escalation/prompt.rs::TOOL_DESCRIPTION` の到達経路があり、memo に合意済み制約・完了条件、採用/却下案と理由、未確認事項を記すこと。未回答事項を推測で合意に変えないこと。
- 説明文接続のために ToolSpec schema/order や同一 run の provider prefix を変更しないこと。

## Acceptance-to-evidence map

以下は既存の回帰テストへの対応であり、この文書だけで今回の pass を主張しない。再実行コマンドは `implementation.md` の Unit → Integration → build に記載した。

| 条件 | 主な証跡 |
| --- | --- |
| 1: 同一 ID/provenance の宛先表示 | `crates/gui/tests/escalation_questions.rs::inherited_question_is_visible_after_restart_and_answered_in_destination_before_finish` |
| 2: 正当 thread の回答と観測後 finish | 上記 GUI 結合テスト、`crates/runtime/tests/escalation_questions.rs::escalated_root_cannot_finish_until_inherited_required_answer_arrives` |
| 3: 元 thread に移動先を表示 | 上記 GUI 結合テストの質問 ID / destination notice assertion |
| 4: 無関係 thread 拒否 | `crates/gui/src/runtime_sink.rs::tests::inherited_question_requires_registered_recipient_and_current_thread_ownership`、`offline_question_rejects_caller_fenced_after_command_authorization` |
| 5: 起動時の既存質問回答案内 | `crates/runtime/src/escalation/prompt.rs` unit tests、上記 GUI 結合テストの initial prompt assertion |
| 6: restart / 複数 continuation | `crates/storage/tests/user_questions.rs::links_hydrate_all_reads_in_continuation_order_even_with_stale_payloads`、上記 GUI 結合テストの reopen 経路 |
| 追加: memo / prefix / 失敗時停止 | `meta/specs.rs` の静的説明接続、`crates/runtime/tests/cache_preservation_e2e.rs::inherited_question_answer_preserves_each_runs_wire_prefix_after_escalation`、`failed_question_inheritance_reports_error_without_spawning_an_escalated_root` |

## Historical writeback and unverified items

PR #133 は orchestration / GUI workbench intent、ADR 0027、`docs/harness-runtime-improvements.md` を更新済み。本 post-hoc 記録から新しい writeback や role-facing surface は生じないため、packet の maintenance metadata は追加更新なしとする。元 PR が writeback 不要だったという意味ではない。

今回未実行の Rust テスト、実 GUI/display による検証、未参照の CI ログを pass と扱わない。未観測の結果と実装済み事実を分離する。新規4ファイルの内容、6件の転記一致、リンク先、`git diff --check`、変更範囲を確認し、検証不能や CLI 拒否は作業報告に明示する。
