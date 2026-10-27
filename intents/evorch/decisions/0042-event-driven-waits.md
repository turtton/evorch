# ADR 0042: shell の poll と Orchestrator の wait を、出来事が起きるまで待つ方式にする

## Status

Accepted（2026-10-27）。利用者との相談で、poll と wait の待機時間の上限をなくし、出来事が起きたときだけ戻す方式に合意した。poll の `return_on: "output"` は残し、待機時間は実行時間の予算に数えないことも合わせて決めた。

## Context

### 計測（2026-10-27、GUI の store から）

GUI の store（`run_contexts` / `usage_requests` / diagnostics）を読み取り専用で集計した。対象は直近の Orchestrator と Worker の run である。

- Orchestrator 3 run では `wait` が最も多いツールだった（682/1864、377/996、221/753 呼び出し）。`wait` が戻った理由の多くは inbox の新着で（464/682、171/377、79/221）、そのたびに `wait` → `inbox` の 2 往復が要った。時間切れで戻った `wait` も run ごとに 51〜102 回あった。1 要求の平均入力は 18〜32 万トークン。
- `IdenticalToolCalls: tool=wait` の停止が 4 回（10/23、10/25 に 2 回、10/26）。最大 10 分の `wait` を同じ引数で繰り返すと、9 回（90 分）で正常に待っている Orchestrator が止まった。Orchestrator の `NoProgress`（max_no_progress_rounds）停止も 8 回あった。`wait` と `inbox` は進捗に数えないため、子のメッセージが続くと上限に達する。
- 子に未回答の質問があると、答えなくてよい質問でも `wait` が毎回すぐ戻った（level-triggered）。再現テストでは 0.24 秒でモデルを 11 回呼び、`IdenticalToolCalls` で停止した。
- Worker 3 run では、shell の `poll` の大半が `yield_ms: 1000` だった（81〜109 回）。1 秒の poll でまだ終わっていなかったものが、1 run で 47 回あった。PR4a（#370）で poll の既定を 60 秒の完了待ちにしたが、それでも長いビルドは 1 分ごとに 1 ターンかかる。Worker が実行時間の上限（2 時間）で止まった記録は 9 回あった。shell を待つ時間も上限に数えていた。
- 長い待機の後に provider が timeout で落ちた記録はなかった。待機後の最初の要求では cache が切れることがある。Anthropic の API key 経由は 5 分を超えると読込率が 2〜5% に落ち、Claude（subscription）と Codex は 30〜60 分まではほぼ保たれる。10 分ごとに起きる今の方式でも同じだけ切れるため、上限をなくしても cache の費用は増えない。

## Decision

### shell の poll

- `poll` は `yield_ms` を無視し、待機時間の上限を持たない。次のいずれかで戻る: job の終了、run へのホスト入力、親からのメッセージ。新しい出力だけでは戻らない。
- 入力やメッセージで戻ったときは job を止めず、最新の出力（末尾優先）と戻った理由を返す。
- 対話的なプロンプトのため、`return_on: "output"` を指定した `poll` は出力でも戻る。
- `poll` で待った時間（`ToolResult::idle_wait_ms`、runtime 専用）は `max_elapsed` から除く。
- 終わらないコマンドを放置しないよう、モデルが起動した shell は既定で 1 時間の `timeout_ms` を持つ。直接呼び出し（`UntilTerminal`）は既定の制限時間を持たない。

### Orchestrator の wait

- `wait` は待機時間の上限を持たない。次のいずれかで戻る: 対象の終了（mode に従う）、呼び出し元にまだ伝えていない子の質問、エージェントのメッセージ、ユーザー入力、呼び出し元の cancel。
- 質問は呼び出し元ごとに「伝え済み」を覚える。`wait` か `subagent_questions` で伝えた質問では、以後の `wait` は戻らない（edge-triggered）。
- `wait` を終わらせたメッセージは、結果の `messages` に本文を入れて消費する。`inbox` を呼ぶ往復は要らない。自動の完了通知は inbox に残す。同期の `delegate` は子の結果を返す形を保つため、従来どおり `inbox_ready` で知らせるだけにする。
- 過去の会話に残る `timeout_ms` は、エージェントループが検証前に取り除く。待たずに状態を見るには `run_output` を使う。

### ループ検出

- `wait` は shell の `poll` と同じく、入力ではなく結果で判定する。初めて観測した子の完了、新しい質問、メッセージ、ユーザー入力のいずれかがあれば進捗とする。何も新しくない即時の戻り（完了済みの子への繰り返しの `wait` など）は停滞として数え、`max_identical_tool_call_repeats` で止める。
- 新しいメッセージと質問は `max_no_progress_rounds` の進捗にも数える。空でない `inbox` の結果も同じ。

## Alternatives

- 待機時間の上限を延ばす（例: 30 分）。時間切れの往復は減るが、同一呼び出しの停止と、質問による空回りは残る。
- `wait` を同一呼び出しの検知から外すだけにする。誤停止は防げるが、完了済みの子への空回りを検出できなくなる。
- 時間切れのたびに cache を保つための定期的な要求を送る。cache の読込料金と、切れた後の書き直し 1 回とがほぼ同じで、ターン数が増える。

## Validation and limits

- 回帰テスト: `crates/runtime/tests/event_driven_waits.rs`（数時間の待機が 1 回の `wait` で済む、伝え済みの質問で空回りしない、完了済みの子への繰り返しは止まる）、`shell_jobs_integration.rs`（長い job の poll が実行時間の予算に数えられない、親のメッセージで子の poll が戻る）、`waiting/tests.rs` と `shell/jobs/wait_tests.rs` の単体テスト。
- ツール説明と schema の変更が、実際のモデルの使い方を変えるかは評価していない。主モデルと fallback モデルでの比較（ツール選択の誤り、ターン数、トークン、cache 利用）は未実施。
- 長い待機後の最初の要求は、provider の cache 保持時間を超えると cache を書き直す。
- ADR 0022 の「wait timeout は二重の安全網」は成り立たなくなった。親子ツリー規則で相互 await は構造的に起きない、という主たる根拠は変わらない。
