---
name: harness-diagnosis
description: "evorch 自身の不具合を、自己改善候補（improvement candidate）や診断イベントから根本原因まで辿り、再現テスト・修正・検証まで進める手順。`evorch inspect` で永続化済みの run・イベント・provider 要求を読み取り専用で調べる。trigger: 自己改善候補, improvement candidate, self-improvement draft, ハーネス不具合, harness bug, evorch inspect, NoProgress, IdenticalToolCalls, LearningPipelineFailed, CrashRecovered, AgentRunPanicked, LessonPromoted, ObserverLagged, 診断イベント調査"
---

# harness-diagnosis — 改善候補から evorch の不具合を直す

evorch が自分の不具合として集めた改善候補を起点に、原因の特定 → 再現テスト → 修正 → 検証 を進める。
対象は **evorch 自身**の不具合であり、ユーザーのプロジェクトの問題ではない。

このリポジトリで作業する外部エージェント（Claude Code など）向けの手順。
evorch の中で動くエージェントは sandbox のため store を読めない。

## 1. 入口を決める

- GUI の Self-improvement ペインに候補 ID がある場合: その ID から始める。ドラフトは `<store のディレクトリ>/self-improvement/drafts/<id>.md` にある。
- ID が分からない場合: 一覧を取る。

```sh
cargo run -q -p evorch -- inspect candidates --status new
# store の既定は <user-config-dir>/evorch-events.db（GUI と同じ）。別の store は --db <path>
cargo run -q -p evorch -- inspect --db <path> candidates --project <slug> --limit 20
```

`evorch inspect` は store を **read-only** で開き、migration もしない。

- `schema version N is older ...` と出た場合: 現在の evorch（GUI）で一度開いて migration を済ませてもらう。
- `newer` と出た場合: 手元のチェックアウトが store より古い。

出力はすべて `{"db": ..., "result": ...}` 形式の JSON。

## 2. 候補から run とイベントへ辿る

```sh
cargo run -q -p evorch -- inspect candidate <candidate-id>
```

候補の主な項目:

- `run_id`: 最初の発生
- `recent_run_ids`: 直近で重複が畳み込まれた run（最大 10 件）
- `occurrences` / `last_seen_at_ns`: 発生回数と最後の発生時刻
- `evidence`: 収集時の JSON
  - `observed_at_ns`: bus event の wall clock
  - `build`: 記録した build

`next` には、次に打つコマンドがそのまま入っている。

```sh
cargo run -q -p evorch -- inspect run <run-id>          # 下表の項目に加え、この run を指す候補
cargo run -q -p evorch -- inspect run <run-id> --full   # 上に加えて messages_json / checkpoints_json の全文
cargo run -q -p evorch -- inspect events --run <run-id> [--limit N]
cargo run -q -p evorch -- inspect events --around <unix-ns> [--window-ms 5000]
```

`inspect run` の主な項目:

| 項目 | 内容 |
|---|---|
| `context` | 最新スナップショット。`terminal_phase` と `config_json` を含み、メッセージ数は `messages_count` |
| `children` | 子 run |
| `ledger` | run ledger |
| `usage_requests` | provider 要求。失敗種別、`finish_reason`、トークン数 |
| `user_questions` | ユーザーへの質問 |
| `event_kinds` | イベント種別ごとの件数 |
| `diagnostics` | この run の診断イベント |

`events --run` は、payload のどこかの `run_id` がその run と一致するイベントだけを返す。
親子関係（`parent_run_id`）だけでは一致しない。子 run は `children` から個別に辿る。

## 3. code ごとの見どころ

| code | まず見るもの |
|---|---|
| `IdenticalToolCalls` | detail の `tool=<name> input_sha256=<16桁>`。`events --run` の Tool イベントで、そのツールの結果が毎回同じ失敗でないか確認する。ツールの結果がモデルに伝わっていない、エラーが曖昧、といったハーネス側の原因を探す |
| `NoProgress` | dedup key の source（`budget_tracker` か `tool_calls`）で発生元が分かれる。`usage_requests` の失敗や `finish_reason`、直前のツール結果を確認する |
| `LearningPipelineFailed` | detail の `cause:` 行。学習用の run は元 run の子ではなく別の root run（`agent_name` が `lesson` / `learning-evidence-review`）。`events --around <observed_at_ns>` の `AgentRunStarted` から見つける |
| `ContextSnapshotFailed` / `EscalationHandoffFailed` | detail と、その run の `context.terminal_phase` / `ledger`。escalation なら移譲先の run も `children` と events で確認する |
| `CrashRecovered` | evidence の `location` / `message` / `thread` / `build`（クラッシュしたプロセスの build）/ `backtrace`。backtrace は std・executor を除いた 1 行 1 フレームで、シンボルのない release build では空になる。同じ location の crash は 1 候補の `occurrences` に集まる |
| `AgentRunPanicked` | run のタスクが panic し、runtime が Error にした。detail の 1 行目が panic メッセージ、`site=` 行が発生位置（候補は位置ごとに分かれる）、`backtrace:` 以降がフレーム。finalize を通っていないので workspace は残り、shell job は停止済み。`inspect run <run_id>` の `context` で panic 直前の状態を見る |
| `LessonPromoted` | `content` は harness scope の lesson 本文。`evidence_refs` は `"<run_id>@<updated_at_ns>:m<i>:b<j>"` の配列で、`inspect run <run_id> --full` の `messages_json[i].content[j]` に当たる。ただしスナップショットの `updated_at_ns` が変わっていれば位置はずれ得る |
| `ObserverLagged` | `skipped_events` 件の取りこぼしがあった。その時間帯の診断は候補になっていない可能性があるので、`events --around` で直接確かめる |
| `SkillDiagnostic:*` | skill の読み込みや検証の問題。evidence の `skill` / `scope` / `detail` |

## 4. 切り分ける

次のどれに当たるかを決めてから直す。

1. **プロジェクトや設定の問題**: ユーザーの config、権限、外部ツールなど。直さず、理由を残して候補を dismiss する。
2. **一時的または外部の障害**: provider の 5xx、レート制限、ネットワークなど。`usage_requests.failure` や Provider イベントで裏付ける。
3. **ハーネスのバグ**: evorch のコードを直す。

`evidence.build` の version / revision が現在のコードより古い場合は、`git log` で修正済みかどうかを先に確認する。

## 5. 再現テスト → 修正 → 検証

- **再現は失敗するテストで書く。** 手動での再現で済ませない。参考になる既存パターン:
  - runtime の振る舞い: `crates/runtime/tests/`（例: `identical_tool_calls.rs` はスクリプト化した `ChatResponse` で AgentLoop を動かす）
  - CLI の e2e: `crates/evorch/tests/headless_e2e.rs`（mock-openai を使う）
  - GUI: `crates/gui/tests/` の headless テスト
  - 候補と store の経路: `crates/evorch/tests/inspect_e2e.rs`
- テストの書き方は test-policy skill に従う。固定 sleep は使わない。
- 変更は worktree と PR で行う。push の前に `cargo clippy --workspace --all-targets -- -D warnings` と `scripts/check-cache-contracts.sh` を通す。
- 直したら、GUI で候補を Reviewed にする（`evorch inspect` は書き込まない）。
- 候補を GitHub Issue にするときは ADR 0011 に従い、投稿前にオペレーターの明示的な確認を取る。ドラフトの `DRAFT — NOT FILED` はそのためにある。

## 6. 証跡の限界（調べても出てこないもの）

- **診断イベントは 30 日で消える**（sandbox の `escalation_review` を除く）。候補だけが残っていても、元のイベントがないことがある。ツール出力の artifact も 7 日で消える。
- **Info 重大度の診断は既定では保存されない**（`[diagnostics].persistence` の既定は warnings）。
- **SecretGuard が secret らしい文字列を検出すると、イベントも候補も丸ごと保存されない。** 痕跡も残らない。
- **記録のみの診断は候補にならない。** `ToolArgumentsMalformed`（ストリームで組み立てた tool-call 引数が JSON でなく、ツールは実行していない。detail に provider・model・ツール名・長さ・sha256 先頭 16 桁）、`CompactionFailed`（要約モデルや provider 側 compaction の失敗、要約サイズ制限）、`EscalationAdmissionFailed`（handoff 先 run を provider が受け付けなかった。ユーザーの停止では出ない）は一時的・外部要因に分類され、diagnostics にだけ残る。疑うときは `events --run <id>` で直接探す。
- **storage の書き込み停止は診断イベントを出さない。** `tracing` のログ（stderr）にしか出ないので、疑うときは GUI を stderr を保存した状態で起動して再現してもらう。
- **`events` テーブルの `session_id` は GUI では固定値。** run の絞り込みには `--run` を使う。
