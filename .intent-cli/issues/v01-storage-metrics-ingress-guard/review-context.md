# v01-storage-metrics-ingress-guard Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

## Slice-specific review focus

- `StorageHandle::append_event` と `repo::event::append_event` の両方でraw `EventKind::Usage`を拒否し、片側だけのguardになっていないこと。
- 拒否がserialize/INSERT/accounting更新より前に起き、events行・session bytes・日次bytesを変えないこと。
- errorが呼出側に識別可能で、raw payloadやcredential候補を診断ログへ出さないこと。
- `UsageSink::submit` → pending `UsageBucket` → `downsampled_metrics` batch flushを壊していないこと。
- Lifecycle / Message / Tool / Provider / Fault eventを誤って拒否せず、projectionの回帰がないこと。
- raw usageを書ける新しいescape hatchや任意SQL APIを追加していないこと。
- 後続 `v01-storage-writer-boundary` のvisibility変更時にもこのguardが保持される構造であること。

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

## Knowledge Writeback Expectation (G461)

`closeout_learning.write_back_required` は `false`。ADR 0012とstorage-memory overviewが既にraw非永続化を明記しているためintent文書更新は要求しない。closeout evidenceにはguardを置いた公開handle/repoの境界と、raw拒否・DB不変・downsampled成功のテスト結果を記録する。
