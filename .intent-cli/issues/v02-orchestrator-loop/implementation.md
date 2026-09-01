# v02-orchestrator-loop Implementation Packet

## Goal

GUIから投入されたgoal+packet contractをdurableにrunへ固定し、worker実装、commit/push、PR、CI、Reviewer request-update/repair/rereview、人間merge承認、intent-cli closeoutまでをevorch内で継続するorchestrator loopを実装する。`finish`はPR+CI+diff成功基準+Reviewer承認のcomposite gateを満たすまで拒否し、idle時にcontinuationを自動dispatchする。人間承認はmergeだけに限定し、外部OpenCode/omo/herdrへ依存しない。

## Why

v0.1 runtimeはdelegate/send/wait/list/inspect/finishを持つが、`finish`は即時受理され、goal durability、idle continuation、review修復、merge approvalは存在しない。現在の実装loopはomo/herdrの外部pane、relay、手作業closeoutに依存する。grill Q6と人間承認点の確定により、GUI起点、durable goal、composite+Reviewer gate、omo型idle continuation、mergeのみ人間承認、shell経由GitHub/intent-cliがv0.2 capstoneになった。本sliceは先行7 packetを統合して「止まらず、勝手にmergeせず、証拠なしにfinishしない」loopをsystem invariantにする。

## Scope

- GUIのgoal submission commandからgoal、packet/issue参照、成功基準、制約、project/threadを正規化し、run/sessionへbindする
- `GoalState { active, paused, complete }`と遷移reason/timestampをEvent Bus + SQLiteへ永続化する。再起動時はevent/logから再構成する
- pause/resume/complete/stop/cancelの意味を分離する。pausedは自動continuation停止、completeはgate成功後のみ、cancel/crashはgoal未完了として再開/blocked判断可能にする
- `finish` meta-opをgate-awareにする。PR実在、最新head/base、CI green、packet acceptance/success criteriaに対するdiff evidence、最新Reviewer approvalを収集し、全てfreshな場合のみfinishをcommitする
- diff criteria照合は単なる「diff非空」ではなく、packet acceptance criteriaとverification evidenceのstructured checklistをReviewer結果と結びつける。判定不能はpassにしない
- gate拒否時は欠落/古いevidenceを型付きreasonで返し、goalをactiveのまま保持する。runtimeがidle/completion eventを観測した時、goal、未充足gate、累積review/nudge、次actionを含むcontinuation promptを一度だけdispatchする
- idle continuationはsession/idle epochでdedupeし、progress eventでepochを更新する。自然言語「完了」やtimerだけでは発火/停止しない
- Reviewer runを起動し、approvalまたはrequest-updateをstructured resultとして記録する。request-updateは対象workerへsteeringし、repair後に新head/diffでrereviewする
- review roundsをboundedにし、上限到達/同一finding反復/repair無進展をblockedへ遷移させる。approvalのないblocked状態をmerge approvalへ進めない
- workerのheartbeat/progress/message/tool/commit eventからstalledを検出し、bounded nudgeを送る。親orchestratorはworker worktreeを直接編集せず、repair workerの再起動/追加委譲で収束する
- run crash時は厳密snapshot reviveをせず、durable goal、transcript、gate/review stateから新規run用contextを組み直す
- GitHub操作はapproved shell toolで`gh`を呼び、intent-cli closeoutもshell toolで正規commandを呼ぶ。専用bridgeや新規CLI binaryは作らない
- merge approvalはPR number/repo/head SHA/gate snapshotにbindし、approve後にhead/CI/reviewerが変われば失効する。rejectは理由をcontinuation contextへ戻す
- merge以外の実装/PR/CI/review/closeoutは自律。`gh pr merge`相当は有効なGUI approval tokenがある場合だけ実行可能
- closeoutの順序とfailure handlingを明示し、intent-cli結果/artifactをevent/transcriptへ記録する。queue polling/seed/publish/issue automationは呼ばない
- gui headlessでqueued packet相当fixtureをend-to-end実行し、review request-updateを1回含むhappy pathと各gate欠落/approval失効/blocked pathを検証する
- `--demo`はfake GitHub/CI/reviewer/intent-cli adapterで同じstate machineを決定的に再現する。実self-dogfoodはqueued v0.2 unit 1本以上でevidenceを残す

## Out of scope

- intent-cli queue polling、queue seed、issue publish、GitHub issue automation（operations layer）
- 新規`evorch` CLI binary/command。起点はGUIのみ
- GitHub/intent-cli専用native bridge。v0.2はapproved shell tool
- parked runのin-flight tool/model snapshotを復元する厳密revive（v0.3）
- 人間による実装開始、PR作成、CI確認、review修復、closeout承認
- 人間承認なしのmerge、approvalの別PR/SHAへの再利用
- lead/orchestratorがworker worktreeを直接編集するfallback
- 固定workflow DSL。Goal/gate invariantの下でdelegation topologyは動的

## Verification

- state-machine unit: active/paused/complete、pause/resume/cancel/crash、invalid transition、event replay/restart
- finish gate table: missing PR、wrong repo/base、stale head、CI pending/fail、criteria未照合、Reviewer missing/request-update/stale、全pass
- continuation unit: idle epoch dedupe、progress後再dispatch、pause/complete/blocked非dispatch、自然言語非依存
- review integration: Reviewer request-update→worker repair→new head→rereview approval、round上限、同一finding反復
- stalled integration: heartbeat/progress timeout→nudge、進展でreset、bounded nudge後blocked、親直接editなし
- merge approval security: PR/repo/head/gate snapshot binding、SHA/CI/review変化で失効、reject→continuation、二重merge防止
- shell adapter contract: allowed `gh`/intent-cli closeout commandとresult capture、queue/publish command拒否、新規CLIなし
- crash recovery: transcript+durable stateから新規run context再構成、in-flight snapshot非復元
- gui headless E2E: packet goal→worker→PR/CI→request-update→repair→Reviewer approval→merge approval→closeout、gate拒否continuation
- `--demo`決定的再現と実queued unit 1本以上のself-dogfood evidence
- workspace全体の`cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: `features/orchestration/overview.md` primary。runtime/gui/roadmapをsupportingとして実装確定を書き戻す。新規intent不要
- ADR candidate: decline — no-fixed-workflow、role boundary、headless、event sourcing、tree addressingは既存ADRで決定済み。approval token形式が再利用可能なsecurity primitiveになる場合のみ新ADR候補
- Diagram candidate: required — orchestration overviewにGoalState/gate/review/approval/closeoutのstate-machine図を追加する
- Docs update: `evorch-gui --help`に`--demo` loop確認手順を追加する
- Closeout learning: event schema、dedupe、gate evidence鮮度、round/nudge上限、approval binding、shell command境界、crash再構成、headless/self-dogfood結果を必須writeback。`write_back_required: true`

- Guide reachability (G645): operatorがGUI goal実装loopの開始・監視・merge判断からgoal state、gate/review status、continuation、merge approval、closeout resultへ到達するrouteを宣言する。`no_role_facing_surface: false`

`improve` (G456 / G460) is the later safety net; packet-time maintenance is the normal path.
