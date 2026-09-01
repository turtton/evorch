# v02-orchestrator-loop Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

## Slice-specific review focus

- GUI command以外に新規CLI起点を追加していないか。headlessは同じapplication service/state machineを駆動し、別実装loopになっていないか
- GoalStateがdurable event/stateとしてsession/runにbindされ、process memoryだけに置かれていないか。active/paused/complete/cancel/crashの意味とinvalid transitionが明確か
- `finish`がPR+CI+criteria diff+最新Reviewer approvalの全ANDを強制し、missing/unknown/staleをpass扱いしないか。modelの自己申告や文字列「done」でbypassできないか
- PR/head/base/CI/diff/reviewer evidenceに取得時点と対象SHAがあり、head更新後の古いReviewer approvalやCIを再利用しないか
- acceptance criteria照合が「diffがある」「testがpassしたとagentが言った」だけで済まず、packet contractと実verification evidenceをReviewerが確認する構造か
- idle continuationがruntime idle/completion event起点で、session/epoch dedupeされるか。timer loop、自然言語検出、同時hookによるprompt stormを起こさないか。paused/complete/blockedで停止するか
- review request-update→repair→rereviewがstructured message/resultで回り、round上限があるか。上限到達をReviewer approvalとして扱わずblockedにするか
- stalled detectionがprogress/heartbeat/event時間に基づき、長い正当tool実行を即stalledにしないか。nudgeがboundedで、親orchestratorの直接edit bypassがないか
- crash recoveryがdurable transcript/stateから新規runを起動する方式に留まり、不完全なin-flight snapshot reviveを導入していないか
- human approvalがmergeだけか。実装/PR/CI/review/closeoutで不要なapprovalを増やしていないか。逆にmerge approvalなしで`gh pr merge`へ到達する経路がないか
- merge approvalがrepo/PR/head SHA/gate snapshotにcryptographicまたはunguessable identityでbindされ、head/CI/review変化、reject、使用済みで失効するか
- GitHub/intent-cli操作がapproved shell tool経由で、credential/command outputを適切にsanitizeするか。専用bridge、新規CLI、intent-cli queue/publish/automation呼出しを追加していないか
- closeoutが定義済み順序で行われ、失敗をDoneに隠さずgoal/GUI/eventへ反映するか。merge前後どちらの処理かがtestとdocsで一致するか
- headless E2Eがhappy pathだけでなくfinish拒否、continuation、approval失効、review上限、stalled blockedを検証するか。`--demo` adapterとproduction shell adapterが同一interfaceを使うか
- self-dogfood evidenceが単なるfixtureではなくqueued v0.2 unit 1本以上の実loop実行を示すか

## Facet context

<!-- BEGIN GENERATED FACET CONTEXT (G530) -->
### vocabulary
- (none overlapping this packet's intent_references)
### invariant
- (none overlapping this packet's intent_references)
### decider
- (none overlapping this packet's intent_references)
### acceptance-property
- (none overlapping this packet's intent_references)
<!-- END GENERATED FACET CONTEXT (G530) -->

注: facet-checkはdurability、evidence鮮度、continuation dedupe、approval bindingの安全性を証明しない。state-machine/table-driven/E2E testと上記review focusを主たる判定にする。

## Knowledge Writeback Expectation (G461)

`closeout_learning.write_back_required`は`true`。以下を確認する。

- `features/orchestration/overview.md`: state-machine図、GoalState、finish evidence/gate、continuation epoch、review/nudge bound、merge approval、closeout順序
- `features/agent-runtime-kernel/overview.md`: event/schema、idle/progress、crash再構成、meta finish変更
- `features/gui-workbench/overview.md`: goal/gate/review/approval/closeoutの実接続
- `technology/mvp-roadmap.md`: headless fixtureと実queued unit self-dogfoodによるv0.2成功基準の充足状況

fixtureだけでroadmap成功を「完了」としている、または実self-dogfoodの失敗/制約を記録していない場合はwriteback不足として指摘する。
