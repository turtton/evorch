# v01-storage-metrics-ingress-guard Implementation Packet

## Goal

`crates/storage/` の ingress で raw `UsageEvent` の直接永続化を拒否し、usage は `UsageAggregator` が生成する `UsageBucket` を `UsageSink` / single-writer が `downsampled_metrics` へ batch flush する経路だけに限定する。公開 handle と低水準 repo の双方に迂回不能な境界を置き、非 usage event の event log 契約は維持する。

## Why

issue #3 (`v01-session-storage`) の v0.1 inspect で、ADR 0012 の「raw は永続化しない」が実装境界になっていないことが判明した。`crates/storage/src/writer.rs:81-97` は任意の `Event` を受け、`crates/storage/src/repo/event.rs:34-45,194-202` は `EventKind::Usage` も通常イベントとして serialize / INSERT できる。これは Codex の高頻度raw保存問題を避けるための ADR 0012 (`intents/evorch/decisions/0012-metrics-architecture.md:21-29`) と、overview の保存方針 (`intents/evorch/features/storage-memory/overview.md:26-28`) に反するため、v0.1.1で入口を閉じる。

## Scope

- `StorageHandle::append_event` (`crates/storage/src/writer.rs:81-97`) で `EventKind::Usage` をDB commandへ送る前に拒否する。専用 error variant等、呼出側が原因を識別できる診断を返す。
- `repo::event::append_event` (`crates/storage/src/repo/event.rs:34-120`) にも同じ invariant を置き、crate内の将来の呼出しが公開handleを迂回してもraw usageをINSERTできないようにする。
- 正規経路 `UsageSink for StorageHandle` (`writer.rs:118-132`) → pending `UsageBucket` → `flush_usage` (`writer.rs:241-246`) → `downsampled_metrics` を維持する。
- 拒否時は events table、event accounting、session total bytesを一切変更しない。
- 非usageの `Lifecycle / Message / Tool / Provider / Fault` は既存のappend/read/projection動作を保持する。
- integrationまたはmodule testで、公開入口拒否、repo入口拒否、DB非変更、downsampled flush成功を固定する。

## Out of scope

- UsageAggregator / ring buffer /1分bucketの集計アルゴリズム変更。
- `downsampled_metrics` schema、flush閾値、retention、WAL運用の変更。
- 一般eventの保存可否やevent語彙の再設計。
- backlog 5 `v01-storage-writer-boundary` が扱うrepo公開範囲の変更。両packetは同じ `writer.rs` / `repo/event.rs` 周辺に触れ得るため、本packetを先に適用し、後続でguardを失わないようmerge時に再確認する。

## Verification

- `cargo test -p storage` で raw usage のhandle/repo拒否、拒否後のevents件数・accounting不変、非usage event回帰、UsageBucket flush成功を検証する。
- 必要に応じ `cargo test -p storage --test usage_flush` と追加focused testを個別実行する。
- `cargo clippy -p storage --all-targets -- -D warnings` / `cargo fmt --all --check` / `git diff --check`。

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: `intents/evorch/features/storage-memory/overview.md` の既存「計測の保存」を実装で満たす。新規node不要。
- ADR candidate: なし。ADR 0012 / 0018 の既決事項の修正。
- Diagram candidate: なし。
- Docs update: なし。overviewとADR 0012が既に正しい境界を記述する。
- Closeout learning: guardを置いた二つの境界と回帰テスト結果をcloseout evidenceに残す。`write_back_required: false`。
- Guide reachability (G645): 内部storage APIの修正のみでrole-facing surfaceを追加しない。`no_role_facing_surface: true`。

`improve` (G456 / G460) is the later safety net; packet-time maintenance is the normal path.
