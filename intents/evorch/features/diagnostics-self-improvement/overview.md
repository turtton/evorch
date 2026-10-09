# Feature: Diagnostics & Self-improvement（診断と自己改善）

[features 一覧](../) / [agent-runtime-kernel](../agent-runtime-kernel/overview.md) / [gui-workbench](../gui-workbench/overview.md)

## 概要

Harness 自身の不具合を runtime が直接捕捉し、Issue 化し、dogfooding によって自分自身を改善可能にする。

## 要件

- **DiagnosticBus**: 全 component が Diagnostic（ProviderProtocolViolation / CacheRegression / ToolCrash / SandboxViolation / AgentDeadlock / UiError / CompactionFailure / SessionCorruption / UnexpectedModelSwitch 等）を送信する
- **Session 終了時の自動 Issue 化**: quick diagnostic agent が diagnostic bundle を分類（project problem / transient provider issue / probable harness bug）。harness bug と判断されたら version / OS / provider / model / event timeline / stacktrace / cache transition / tool call / sanitized reproduction をまとめて GitHub Issue 化
- **Crash spool**: panic 等で session-end hook が実行できない場合は `~/.harness/crash-spool/` 等へ durable に保存し、次回起動時に処理
- **Self-improvement introspection API**: harness.inspect_session / inspect_agents / inspect_cache / inspect_provider / inspect_ui / spawn_test_instance / capture_ui / replay_interaction / report_bug。不便を検出 → 改善案作成 → workspace config 変更または source 変更 → test instance → 検証 の自己改善 loop
- **UI 自己改善との連携**: Level 3 の framework implementation 変更は worktree → source modification → build → test harness instance → semantic inspection → screenshot / interaction replay で自己検証
- **Role/model evaluation との連携（v0.7 Bundle E4）**: evaluation trace / failure attribution / prompt variant の試行結果を memory backend に保存し、改善候補の promotion 判断へ利用する
- **Browser diagnostics（v0.7 Bundle Browser）**: headless browser の action log / screenshot / DOM diff を diagnostic evidence として記録可能にする
- **Lesson 由来の改善候補は harness scope のみ（2026-10-08）**: 昇格済み lesson のうち scope が `harness` のものだけを `LessonPromoted` 候補にする。`project` / `user` lesson は task memory であり、改善候補には混ぜない（[storage-memory](../storage-memory/overview.md)）
- **候補は元 run への索引（2026-10-08）**: cooldown 内の重複は捨てずに既存候補の `occurrences` / `last_seen_at_ns` / `recent_run_ids`（最大 10）へ畳み込む。dedup key は `diag:{source}:{code}` で emitter ごとに分ける。evidence には bus event の wall clock（`observed_at_ns`、`events.wall_clock_ns` と一致）と記録した build（version・`EVORCH_BUILD_REV`・OS/arch）を入れ、lesson 由来候補は抽出元 run を `run_id` に持つ。IdenticalToolCalls はツール名と入力の SHA-256 先頭 16 桁、LearningPipelineFailed は失敗原因を detail に含める。observer は bus lag で止まらず、lag エピソードごとに `ObserverLagged` 候補を残す
- **外部エージェント向けの調査経路（2026-10-08）**: `evorch inspect` が store を read-only（migration なし、スキーマ版一致必須）で開き、`candidates` / `candidate <id>` / `run <run-id> [--full]` / `events (--run <id> | --around <ns>)` を JSON で返す。候補の `next` に元 run・イベントへ辿るコマンドを示す。手順は repo skill `harness-diagnosis`（`.agents/skills/`）にまとめ、evorch 内エージェントは sandbox のため対象外
- **診断コードの棚卸しと crash の手がかり（2026-10-09）**: `DiagnosticEvent` の code は `event_bus::event::diagnostic_codes` の定数だけで発行し、`ALL` に全定数を並べる。self-improvement の分類表は `ALL` の全コードを明示的に分類し（未知コードの `Ignored` に落とさない）、発行側の文字列リテラルと `ALL` の漏れはソース走査テスト（`crates/runtime/tests/diagnostic_code_inventory.rs`）で検出する。crash spool の panic hook は元の hook も呼び（stderr 報告を維持）、std・executor・panic 機構を除いた backtrace（1 フレーム 1 行、最大 24 フレーム）を記録する。シンボルのない stripped release build では空になりうる。`CrashRecovered` の dedup key は発生位置（`crash:{location}`）で、同じ panic 箇所は 1 候補の `occurrences` に畳み込む
- **run の panic を即時に報告（2026-10-09）**: agent run のタスクは `panic_capture::catch_panic` で包まれ、panic すると shell job を止めて workspace を残し（finalize 失敗と同じ扱い）、`AgentRunPanicked`（Error、run_id 付き）を出して run を Error（reason `panicked: <1 行目>`）にする。発生位置と backtrace は `panic_capture::install` の hook（GUI と `evorch` CLI が起動時に設置）が捕捉中の poll に限って記録し、crash spool には書かない。detail の `site=` 行（`event_bus::event::DIAGNOSTIC_SITE_PREFIX`）は dedup key に入り、発生位置ごとに候補を分ける
- **記録のみの障害診断（2026-10-09）**: ストリームで組み立てた tool-call 引数が JSON でないとき、providers が `ToolArgumentsMalformed`（引数本文は含めず長さと SHA-256 先頭 16 桁）を出し、runtime はそのツールを実行せず「引数が JSON でない」とツールエラーで返す（入力は従来どおり `null` で履歴に残る）。compaction の試行が失敗したとき（要約モデル・provider 側 compaction・要約サイズ制限。cooldown 等のガードによるスキップは除く）は `CompactionFailed` を出す。`EscalationAdmissionFailed` はユーザーの停止・取り消しでは出さない。この 3 つはモデル出力・provider・設定に依存するため TransientOrExternal（候補にしない）
- **storage の書き込み停止を spool（2026-10-09）**: storage が全イベントを拒む状態（writer 終了、DB・セッション・日次・WAL のサイズ上限）に入ると、GUI の storage bridge が停止エピソードごとに 1 件、crash spool に `StorageWriterHalted`（site `storage:<cause>`）を書く（`self_improvement::spool_fault`）。書き込みが通ればエピソードは終わる。1 イベントだけの拒否（イベントサイズ、secret guard、stale mutation）は対象外。spool の code 付きエントリは次回起動時に分類表を通り、harness のものだけが候補になる。spool は self-improvement が有効なときだけ
- **委譲直後の取り消し（2026-10-10）**: 親が background で委譲した子を、直後のモデルターンで `cancel` することが同じ target（role/category）で 3 回に達すると、`DelegationRetracted`（Warning、source `retracted_delegates`、site `role=<role> category=<category>`）を run ごと・target ごとに 1 回出す。有効だが意図と違う role/category は target 検証を通るため、この自己修正の繰り返しが唯一の痕跡になる。HarnessImprovement に分類し、`delegate` の説明やスキーマが誤選択を誘っていないかを見る入口にする


## 受け入れ基準

- runtime fault が DiagnosticBus に流れ、診断バンドルが生成されること
- harness bug と分類された診断が GitHub Issue として作成されること
- session-end hook 非実行時も crash spool に記録され、次回起動時に処理されること

## v0.1.1 provider request 観測イベントの下地確定（2026-08-30、PR #32 / issue #31）

診断の入力源となる provider attempt 観測が event-bus schema に landed: request 開始/TTFT/完了/失敗（型付き `ProviderFailureKind` 分類）/fallback 選択が request ID 相関で bus に流れる。失敗 payload の診断情報（レスポンス本文・credential）は意図的に含めない（bus/storage に流れるため）。本 unit は観測の schema と発行境界のみで、DiagnosticBus への接続・Issue 化は本 feature の後続 unit の責務。詳細は provider-routing/overview.md の実装確定セクションを参照。

## Related decisions

- [ADR 0006: Harness 自身の診断と自己改善](../../decisions/0006-self-improvement-and-diagnostics.md)
- [ADR 0005: Headless Agent Kernel と GUI の分離](../../decisions/0005-headless-kernel-and-gui-separation.md)

## Open questions

- 自動 Issue 化の抑制条件（誤検出の multi-fire 防止）
- self-improvement agent の権限範囲（config 変更のみか source 変更まで許可するか）
- ~~diagnostics の event 契約詳細~~ → 2026-09-10 確定。Bundle D の memory ledger と Bundle Browser の evidence を source に使う design が採択された
