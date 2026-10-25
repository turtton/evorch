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

## Addendum（2026-10-21、ADR 0037）

Decision 4 の checkpoint は、所有権側の active 状態と小さな snapshot 参照を同一トランザクションで
確定するドレイン・バリアである。owner checkpoint は独立した会話履歴の復旧源ではない。
履歴は先に `run_contexts` へ保存し、任意の run store の不在またはその snapshot の保存失敗では
参照を none にして境界を確定する。副作用の安全上必要な保存・回収の失敗は致命のままである。
詳細、旧形式との共存、offline の生履歴掃除は [ADR 0037](0037-owner-checkpoint-metadata.md) に従う。
Decision 5 のうち `run_contexts` 書き込みへの generation fence は既存の未履行範囲として残り、
ADR 0037 では追加しない。owner の参照 commit は従来どおり generation を検証する。

## Addendum（2026-10-23、registry 競合で run を止めない）

### Context

高負荷時に `ownership validate failed at read (attempt 1): database is locked` などで run が Error に
なり続けた。owners.db は rollback journal（DELETE）で、待機中の writer が新しい reader を止める。
run の確認（`validate_mutation`）は 2 秒の単一試行、turn 開始・checkpoint・終了処理は 10 秒の
deadline で打ち切っていた。さらに event bus の run fence はイベントごとに registry の読み取り
トランザクションを張り、競合を拒否と同じに扱ってイベントを捨てていた。終端の
`AgentRunStateChanged` も捨てられ、GUI のスレッドが実行中のまま残った。

### Decision

1. **競合は権限の喪失ではない。** 他プロセスが claim できるのは owner のソケットが応答しない
   ときだけ（Decision 3）で、lease の期限切れだけでは generation を失わない。run の registry
   操作（turn 開始、mutation 前の確認、checkpoint、終了処理）は競合を期限なしで再試行する
   （`RetryPolicy::unbounded`）。各試行はトランザクション内で generation を再検証し、
   終了させるのは fence（owner/generation の不一致または Released）、Stop/Cancel、
   その他のエラーだけとする。待機は blocking pool で行い、数秒続いたら run の activity を
   `Ownership` にし、`OwnershipRegistryWait` 診断を 1 回記録する。
2. **run イベントの fence はプロセス内の記録で判定する。** generation は一度失えば戻らない
   （claim/handoff は必ず generation を進め、release は最終）。registry へのアクセスが fence を
   観測したら、その generation をプロセス内の記録に残す。run 許可の各操作、heartbeat 走査
   （前回保持していた generation が消えた場合）、release、handoff、claim が記録する。
   event bus の run fence はこの記録だけを読み、registry を開かない。競合でイベントが
   ブロックされることも捨てられることもない。
3. ADR 0037 の「既存 generation guard を弱めない」制約は、run イベントの fence については
   本 addendum で置き換える。GUI のイベントバッチも run イベントについては同じ記録で判定し、
   registry のトランザクションを保持しない。永続書き込みの guard（todo/artifact のツール、
   GUI コマンドの `mutation_guard`）は従来どおり registry の読み取りトランザクションを保持する。
4. GUI は、このプロセスが claim したスレッドで、ローカルの runtime にない
   Running/Pending/Waiting の run を Stopped と表示する。履歴の照合時に前の owner が
   生きていて保護された run も、claim の時点で解消する。

### Consequences

- 高負荷だけを理由に run が止まらない。registry が長く使えない間、run は待ち続ける
  （Stop/Cancel で中断できる）。
- 応答しなくなったプロセスが他に claim された後、そのプロセスが自分の fence を観測する
  まで（heartbeat の次の走査、または run の次の registry 確認まで）は、旧 generation の
  イベントが bus に流れうる。永続書き込みと owner の commit は従来どおり generation を検証する。
- 負荷そのもの（heartbeat のスレッドごとの書き込み、操作ごとの接続オープン）は別途削減する。
- 2026-10-24 追記: heartbeat は 1 tick に 1 トランザクションでまとめて書き、lease が次の tick 後も
  半分以上残るスレッドは書かない（既定 5 s / 1 s で約 2 tick に 1 回）。25 スレッドの tick は
  個別トランザクションで約 30 ms、まとめると約 1 ms（DELETE journal、ローカル計測）。
  接続の使い回しは見送った。新規接続の読み取りは約 47 µs と 6 µs の差で、run のイベント fence が
  registry を開かなくなった後は、操作ごとの接続オープンは負荷の主因でない。

## Related

- tmux: session / client の分離（attach/detach）
- Zellij: registry / resurrection
- VS Code remote: reconnect grace / active client lifecycle
- intents/evorch/features/gui-workbench/overview.md
- intents/evorch/features/storage-memory/overview.md（event ledger / checkpoint 保存）
- intents/evorch/technology/mvp-roadmap.md（Bundle C'）
