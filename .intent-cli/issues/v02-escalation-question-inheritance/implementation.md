# v02-escalation-question-inheritance Implementation Packet

## Goal

escalation で継承された未回答ユーザー質問を、同一 question ID と元 requester provenance のまま継承先 Orchestrator thread に表示し、正当な thread から回答できる追加契約を追跡する。

**実装は PR #133（squash `eb79a7f`）で完了済み。これは post-hoc の歴史的証跡であり、これから実装する計画や今回再実行したテストの証明ではない。** 完了済み PR #76 の `.intent-cli/issues/v02-direct-escalation-handoff/packet.yaml` は依存元の歴史的証跡として参照するだけで、変更・上書き・削除しない。

## Why

元の Direct→Orchestrator handoff に加え、質問の永続的な継承を GUI の表示・回答認可・起動案内へつなぐ追加契約が必要だった。元 requester を継承先で上書きすると provenance を失い、質問の作り直しでは ID・回答・blocking gate の追跡が分断される。表示だけを広げても、無関係 thread や古い ownership generation から回答できてはならない。

PR #133 はこの差分を実装し、orchestration / GUI workbench intent、ADR 0027、`docs/harness-runtime-improvements.md` に反映した。本 packet は、その追加契約を元 packet に遡及して混ぜずに記録する。

## Scope

以下は PR #133 で実施済みの内容である。`target_path` は当時の実装対象を示し、本 packet 作成作業にそれらの変更を許可するものではない。

- **Durable routing / provenance:** `user_question_links` を canonical consumer routing とし、質問の `id/run_id/root_run_id/root_name` を保持した。`UserQuestion.recipient_run_ids` を `#[serde(default)]` 付きで追加した。
- **Storage projection:** `crates/storage/src/repo/user_questions.rs` の `hydrate` を `get/list` に適用した。古い payload の projection を信用せず、links を `ORDER BY rowid` で読み、再起動や複数 continuation の継承順を復元する。
- **Shared visibility contract:** event-bus の `is_user_visible/belongs_to_thread` を GUI と runtime sink で共有した。root の user-visible 質問を元 thread または recipient run に結び付く thread に見せ、child-to-Orchestrator 質問と無関係 thread を除外する。
- **Inheritance publication / ordering:** 継承成功後の hydrate 済み質問を `UserQuestionUpdated` で再発行した。質問継承と workspace 準備を source terminal と新 root startup より前に行い、失敗時は起動しない。event fence 自体は緩和していない。
- **Answer authorization:** `runtime_sink` は consumer routing と execution ownership を別々に検証する。caller permit の thread/generation と storage 書込み中の `mutation_guard`、runtime の live recipient ownership 検証を維持する。storage acknowledgement (`UserAnswerSaved`) は古い run のイベントが fence されても回答済みカードを消せる。first-answer-wins と同じ回答の冪等性を保つ。
- **GUI / startup:** 元 thread に既存質問 ID と移動先 thread/run を示す notice を残した。継承先を Orchestrator として表示し、起動 prompt に未回答 ID、requester、root、blocking、質問文、選択肢を列挙する。同内容を新 ID で再質問せず、回答の到着と model による観測まで finish できないことを案内する。
- **Handoff memo:** `original_request` にユーザーと合意済みの制約・完了条件、`findings` に採用/却下案と理由、`blockers` に未確認事項を記す。未回答やメモにない内容を合意済みと推測しない。`meta/specs.rs` から `escalation/prompt.rs::TOOL_DESCRIPTION` に静的説明を接続し、ToolSpec schema/order や run 固有の provider prefix を変えずに案内へ到達可能にした。

### 採用・却下案と理由

- 採用: durable links と additive projection。永続的な配送関係を正本にし、元 provenance と後方互換を保てる。
- 採用: 表示・回答経路で共有 helper を用い、ownership/permit は別の境界で検証する。UI と認可条件のずれを防ぎつつ実行権限を広げない。
- 却下: `root_run_id/root_name` の継承先への書換え、または新 ID による再質問。元 requester と同一質問の履歴を壊すため。
- 却下: recipient projection を実行権限とみなす、または event fence を無効化する。無関係 thread・古い owner を認可し得るため。
- 却下: 元 packet を追加契約で上書きする。当時の受け入れ条件と今回の追加契約の境界を失うため。

## Out of scope

- 本作業による `crates/`、`intents/`、`docs/`、既存 packet の変更や PR #133 の再実装。
- question ID/provenance の書換え、重複質問の生成、未回答の自動合意、質問回答による tool permission の付与。
- event fence / ownership guard の緩和、in-flight tool の厳密な復元、自動再実行、固定 workflow。
- commit / push、branch / worktree 操作、GitHub issue / PR の新規公開。

## Verification

受け入れ条件の正本は `docs/harness-runtime-improvements.md:59-71` の6件であり、`packet.yaml` に転記した。以下は PR #133 で追加・更新された回帰証跡と再検証手順である。**本 packet の作成だけで下記 Rust テストを今回 pass したと扱わない。**

### Unit → storage contract

- `cargo test -p gui --lib app::questions::tests` — shared helper、child 質問の非表示、旧 event の serde default。
- `cargo test -p gui --lib runtime_sink::tests` — `inherited_question_requires_registered_recipient_and_current_thread_ownership` と `offline_question_rejects_caller_fenced_after_command_authorization` を含む consumer / owner の負例。
- `cargo test -p runtime --lib escalation::prompt::tests` — 既存 ID/provenance、未回答一覧、再質問禁止、finish 観測条件の起動案内。
- `cargo test -p storage --test user_questions` — `links_hydrate_all_reads_in_continuation_order_even_with_stale_payloads`、reopen、明示継承、無関係 recipient 拒否、first-answer-wins。

### Integration → build

- `cargo test -p runtime --test escalation_questions` — 回答前の finish 拒否、観測後の完了、継承前に保存された回答、継承失敗時に新 root を起動しないこと。
- `cargo test -p gui --test escalation_questions` — 同一 ID/provenance の表示、元 thread の移動先 notice、restart 後の再表示、継承先での回答と finish。
- `cargo test -p runtime --test cache_preservation_e2e inherited_question_answer_preserves_each_runs_wire_prefix_after_escalation` — 各 run 内で provider input prefix を保持すること。
- `cargo check -p event-bus -p storage -p runtime -p gui` — additive projection と ToolSpec 接続を含む crate 間のビルド整合。
- packet 作業の検証: intent-cli の利用可能な読み取り専用 validate、4ファイルの最終確認、`git status --short`、`git diff --check`、変更範囲と既存 packet の不変確認。

### 未確認事項

本 packet 作成時の Rust テスト再実行結果、実 GUI/display を使う手動検証、PR #133 の CI ログは、実装済みである事実とは分けて扱う。観測していない結果を pass と記載しない。queue や公開ライフサイクルの操作結果は packet の契約内容ではなく作業報告で管理する。
