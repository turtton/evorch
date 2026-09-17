# Ultrawork Notepad — v07-durable-execution-substrate (issue #118)
Started: 2026-09-18T00:00:00+09:00

## Plan (exhaustive, atomic)
1. Claim issue #118 (intent-cli worker claim)
2. Wiring-confirmation relay → w1X:p1
3. Fetch issue #118 body; explore code-map paths
4. Plan agent → parallel task graph
5. TDD impl: (1) Durable Task/Run continuation, (2) Budget & circuit breaker, (3) Evidence-first review gate
6. Reviewer Gate (ultrabrain/plan reviewer)
7. PR ready-for-review
8. intent-cli worker result-summary + complete
9. Completion relay → w1X:p1

## Scenarios (the contract)
- S1 (resume-after-crash): task_id で中断 goal を resume、成果物・失敗理由・再開地点が SQLite 永続化 → binary: resume 後の run が中断地点から継続し events replay が task state を再構築
- S2 (budget checkpoint): 50-call ごとに checkpoint event 発火 → binary: mock provider e2e で DiagnosticEvent 発行を観測
- S3 (budget exhausted): tool-call 予算超過 → DiagnosticEvent code=BudgetExhausted; 無進捗 → NoProgress
- S4 (provider pre-spawn gate): 未検証 fallback で worker 起動しない → ProviderUnavailable 発行
- S5 (evidence-first verdict): ReviewLoop verdict が typed tool result (criterion+evidence 紐付け)
- S6 (regression): cargo test --workspace 全 green

## Now
Claim issue #118

## Todo (remaining, ordered)
(see Plan)

## Findings
- T1 RED (2026-09-18): `cargo test -p storage --test migration v8_extends` failed with `left: 7 / right: 8` (0 passed, 1 failed). `cargo test -p storage --test task_queue` failed with E0560 for all eight missing durable fields (16 errors after correcting the test's close() return-type mistake). The repo update/list test also failed with E0560 before implementation. Projection RED failed with `Some(Failed)` vs `Some(Cancelled)` before changing the mapping.
- T1 GREEN: `cargo test -p storage` passed 162 unit/integration tests (87 unit + 75 integration), plus 8 doc-tests. Final migration-only rerun passed 8/8. `cargo check -p storage`, `cargo clippy -p storage -- -D warnings`, `cargo fmt --check`, and `git diff --check` passed. Storage LSP scan and targeted task/migration diagnostics were clean.
- T1 design: v3 already allowed pending/blocked; v8 makes all eight statuses bijective and preserves task_links during transactional table recreation. JSON is opaque Option<String> matching run_contexts; attempts is u32; heartbeat is Option<SystemTime>. Public entity::TaskRecord/TaskStatus paths remain stable via re-export. Repo tests cover update/list and NULL clearing; writer integration covers create/get/reopen. The v6 fixture now creates the complete earlier schema. Table/index names are unchanged.
- T1 size review: new task entity 36 pure LOC, task repo 181, migration tests 247, CRUD tests 246 (warning band; split before further additions). Existing oversized entity/projection files were reduced/not expanded; unrelated credential/projection refactoring remains out of scope.
- T2 (2026-09-18): event-bus に TaskProgressed / TaskCheckpoint / TaskRetryScheduled / TaskStaleMarked、CriterionEvidence、CriterionCheck.evidence (serde default)、event::diagnostic_codes の 3 定数を追加。DiagnosticEvent と既存 variant の形状は維持。既存 enum に ns timestamp はなく、elapsed は u64 / count は u32。指定どおり last_heartbeat_ns は u64。
- T2 RED: 実装前の `cargo test -p event-bus` は exit 101。`E0599: no variant named TaskProgressed/TaskCheckpoint/TaskRetryScheduled/TaskStaleMarked`、`E0422/E0425: CriterionEvidence not found`、`E0560: CriterionCheck has no field named evidence` の 11 エラー。診断テスト単独 `cargo test -p event-bus --test diagnostic` も exit 101、3 定数に対する `E0433: cannot find diagnostic_codes in event` を確認。
- T2 GREEN: `cargo test -p event-bus` は 148 passed / 0 failed (unit 120、integration 28、doc 0)。全 variant + Criteria evidence の Event envelope 往復、exit_status/target_sha/artifact_path の wire 値、旧 checklist の evidence 省略、optional evidence refs 省略、u64 最大値、負の exit_status、診断の直接 JSON/EventKind 往復を検証。
- T2 gates: `cargo check -p event-bus` / `cargo clippy -p event-bus -- -D warnings` / `cargo fmt -p event-bus --check` は exit 0。変更 Rust 3 ファイルの LSP error 診断なし。`cargo fmt --check` は並行 T1 の storage/src/repo/task.rs、storage/tests/migration.rs、storage/tests/task_queue.rs の整形差分のみで失敗し、担当外なので変更せず。後続は CriterionCheck の Rust literal に evidence を設定し、新 enum variant の exhaustive consumer を更新する必要あり。
- W1-T3 verification-by-construction: `gate_evidence_criteria_checklist_round_trips_with_full_evidence` は `OrchestratorEvent::EvidenceRecorded` の Criteria checklist に command、exit_status、target_sha、diff_ref、artifact_path、red_evidence を全て設定し、実装変更なしで 1 passed。T2 の `GateEvidence::Criteria` → `Vec<CriterionCheck>` → `CriterionEvidence` serde 配線を direct event 往復の回帰テストとして固定した。
- W1-T3 legacy compatibility: `legacy_criterion_without_evidence_decodes` は旧 Criteria payload の criterion から `evidence` キーを省略し、`#[serde(default)]` により `None` として 1 passed。専用テスト追加前から契約が成立していたため RED にはならず、旧形状互換の明示的な回帰ロックとして保持する。
- contract: /home/turtton/.ghr/github.com/turtton/evorch/.opencode/v07-durable-execution-substrate-contract.md (host repo side)
- review notepad: .opencode/v07-durable-execution-substrate-review-notepad.md
- intent-cli binary: /nix/store/bhn4d645q3f0nc7m7k0iw20phizrsw5i-intent-cli-0.26.0/bin/intent-cli
- lead pane: w1X:p1

## Learnings

## Plan locked (plan agent ses_f4eda67ccffe19EaaiCY26FIaI)
13 tasks / 5 waves. Critical path T2→T4→T5→T6→T13.
W0: T1(storage v8), T2(event-bus variants+codes) — T9 deferred to W1 (needs T2 codes).
W1: T3(criterion wire), T4(ledger replay+durable fields), T7(budgets/checkpoints), T9(provider pre-spawn gate).
W2: T5(resume/retry/cancel), T8(escalation→NoProgress bridge), T10(typed reviewer result).
W3: T6(stale-worker), T11(supervisor typed-first), T12(GUI DurableTasks panel — new pane per v09-b pattern).
W4: T13(workspace green).
Decisions locked by plan: new GUI pane (not Agents extension); tasks-table column-add in v8 with widened CHECK; typed-first with prose fallback for reviewer; restore one-shot handled via fresh run per retry attempt.
