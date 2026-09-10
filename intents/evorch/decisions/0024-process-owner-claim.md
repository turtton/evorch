# ADR 0024: thread process owner / claim モデル（single host 複数 GUI 連携）

## Status

Accepted

## Context

GUI を複数起動したり、GUI を閉じても作業を継続させたいという要望がある。現在の evorch は 1 プロセス内で runtime と GUI を持つ構成であり、複数 GUI 間での thread 所有権・実行の連携ルールが未定義だった。

外部調査（tmux / Zellij / VS Code Remote / VS Code agent host）の結論:
- Raft 等の分散合意は不要（single host 範囲で十分）。
- 必要なのは「owner」と「registry」と「lease/heartbeat」であり、attach と claim/handoff は分ける。
- tmux の attach/detach、Zellij の registry/resurrection、VS Code の reconnect grace / active-client lifecycle を合成する。

## Decision

1. **Thread owner モデル**:
   - 各 thread は実行時に必ず 1 つの owner process を持つ。
   - owner は `owner_id` / `generation` / `lease_expiry` を持つ。
   - GUI / CLI は thread に attach して表示・操作できるが、turn 実行権は owner のみが持つ。

2. **Registry**:
   - thread ごとの registry entry を `~/.local/state/evorch/threads/` 配下の metadata + lease 情報とし、SQLite または atomic JSON で保持する。
   - Unix domain socket を使い、status 通知・claim/handoff 要求・heartbeat を交換する。

3. **Lifecycle**:
   - `Running(owner=A)` → heartbeat timeout → `Suspect` → grace 経過後 `Stale` → `Claimable`。
   - 旧 owner が応答しない場合のみ `Claimable` とし、active turn 中は claim を拒否する。
   - claim は CAS（`expected_generation` / `expected_owner_id`）で実行し、成功時に `generation` を increment する。

4. **Safe shutdown**:
   - 終了要求は `Quiescing` 状態に遷移させ、新規 turn 受付を止め、現行 tool を完了・checkpoint してから lease を解放する。
   - GUI 終了時に active な thread がある場合は警告し、必要に応じて handoff を促す。

5. **Mutation の generation 付与**:
   - turn / tool / transcript の mutation には generation を付与し、旧 generation の request を thread process 側で拒否する。
   - これにより split-brain を防ぐ（外部 API までの強制はできないため）。

6. **Attach semantics**:
   - running thread への attach は可能。
   - absent thread の attach は start しない。`start-or-attach` は明示的な別操作とする。
   - 既定は read-only attach とし、write / claim / handoff は別途明示する。

## Consequences

- GUI と CLI の複数起動・複数接続が可能になる。
- owner が死んでも work 状態が残り、claim によって引き継げる。
- generation 検証により、旧 owner からの遅延書き込みを拒否できる。
- Raft を入れずに single host の整合性を確保できる。

## Non-goals

- 複数ホスト間での分散合意。
- 常駐 daemon による中央管理への全面移行（既存の単一プロセス構成を置き換えない）。

## Related

- tmux: session / client の分離（attach/detach）
- Zellij: registry / resurrection
- VS Code remote: reconnect grace / active client lifecycle
- intents/evorch/features/gui-workbench/overview.md
- intents/evorch/features/storage-memory/overview.md（event ledger / checkpoint 保存）
- intents/evorch/technology/mvp-roadmap.md（Bundle C'）
