> 復元状況: 会話記録の最終レビュアー read から、改訂 2 の冒頭〜§1.3 までを復元。§2 以降（特に後続の J1〜J8 反映を含む §9）および §10 は欠落しているため、推測で補っていない。

# 5a-V 実装分割計画（公開入口の同時公開 integration）— 改訂 2

作成: Planner の代役 Worker（読み取り調査のみ。コード変更・commit・PR なし）
調査したツリー: `/home/turtton/.ghr/github.com/turtton/evorch` の HEAD `794fa1ed`（`f794cd36` #734 の上に docs commit が 1 つ）。B2 の状況は `.worktrees/d3a2-b2` の未 commit の差分を読んだもので、今後変わりうる。

改訂 2 は、計画レビュー（M1〜M7 と should）と親の判断（msg-1198）を反映した。対応の一覧は §10。

## 0. 要約

- **公開の切替を 1 か所にまとめた**（§2）。install が main に入っても、管理操作は全部 fail-closed で拒否される。有効にするのは「公開列車」の T1 だけ。
- 5a-V は **設計メモ 3 本（独立レビュー）＋ PR 21 本（必須 20、任意 1）** になった。1 PR は変更ファイル 15 個前後、新しい試験の種類 1〜2 個を上限にした。
- **公開前の製品 artifact では、管理操作を有効にできない**（有効にする入口が製品に無いため）。製品→製品の試験と性能の実測は、公開列車の中で行う（§2.4）。公開列車を使うかどうかは親の判断（§9 の J1）。
- 調査で見つけた、handoff にも計画本文にも無い項目: host の所有構造（N-03）、`FinalizationEnvironment`／`RestartMetadata` の本番構築（N-16）、`RecipeSource` の本番実装、Q1／Q2 の client（N-13）、`UndisposedAdmission` の live collector（N-17）。
- 並列の山は 2 つ。段階 A（設計メモ、V0 の 2 本、V-gui、V-ops-q1）と、段階 C（V1b-2b の後の試験 PR 5 本）。
- B2 は (a)（実入口の bind）と (b)（残りの移行）に分かれる。**B2 (a) と V1b-1 が `main.rs`／`planned_entry.rs` を取り合う**ので、先に B2 (a) を merge する案を推奨する（§9 の J2）。

## 1. 作業項目の一覧と突き合わせ

### 1.1 事実確認（HEAD `794fa1ed` で確認した範囲）

| 確認した事実 | 場所 |
|---|---|
| 3 つの facade を登録する関数（`install_restart` / `install_quarantine` / `install_owned_graphs`）は定義だけで、製品に呼び出しが無い。`install_operation_reader` だけが呼ばれる | `workbench-host/src/admin/service.rs:116-128`、`bootstrap/mod.rs:656` |
| **host は管理操作を広告で止めない。必要なのは検証済み local と facade の登録だけ**（広告を見るのは CLI の `request` と `interrupt` だけ。`status` は広告を見ずに送る） | `backend.rs:402-408`、`admin/service.rs:152-225`、`flow.rs:146,159,195,291`（レビュー指摘。`backend.rs` と `service.rs` は私も確認） |
| `SafeRestart` は `Capability::IMPLEMENTED` に無い。`server_hello` は、検証済み local 以外から `safe_restart.v1` を取り除く処理だけを持つ | `handshake.rs:48-72`、`backend.rs:106,404` |
| 製品の入口は 2 つ。cold の `main.rs::serve` と planned successor の `planned_entry.rs::run`。どちらも `LifecycleDriver::new(&host)` → `run_with_web(&host, …)` | `evorch-core/src/main.rs`、`planned_entry.rs` |
| `start_finalization(&self, id, host: &mut BootstrappedHost, tx, target)`。参照実装は F4-P の harness | `restart/finalization.rs:21`、`product_exec/old_core.rs:132,238` |
| `FinalizationOwner::finalize(coordinator, id, current, target) -> AdminContinuation` は `host` を受け取らない | `restart/public/implementation.rs:52-60` |
| `FinalizationEnvironment` と `RestartMetadata` の本番構築が要る（`Arc<Instance>`、signal mask、`OperatorSignals`、`ControlDrainHandle`、successor の argv）。`main.rs` の `instance` は `Arc` ではない | `restart/finalization/later.rs:108-130`（レビュー指摘） |
| `SnapshotClock` は診断用の cache で、CAS でも予約でもない。revision は DB を読み直して作る。一方 `admit` は I/O の無い `commit` の中で呼ばれる。**live の collector は存在しない** | `recovery_disposal/snapshot.rs:5`、`facade.rs:219-233`、`implementation.rs:185-215` |
| `UndisposedAdmission` は trait だけで実装が無い。admin service は restart の accept 時に owned_graphs facade の `Undisposed` を使う | `implementation.rs:41`、`admin/service.rs:371-378` |
| `LocalTargets` は実装があるが、本番での構築は無い。`RecipeSource` は本番実装が無い。`RecoveryRecipe` を作る本番コードも `request_planned` の本番呼び出しも無い | `restart/public/local.rs`、grep（レビュー指摘） |
| `OwnedGraphsFacade::new(runtime, owner, storage, reader, diagnostics)` と `QuarantineFacade::new(store, reader, epoch)` は構築可能。製品での構築は無い | `recovery_disposal/facade.rs:76`、`quarantine/facade.rs:32` |
| Q1 の属性が 5 行（`dead_code` 3、`unused_imports` 2）。Q2 側は `cfg_attr(not(test), allow(dead_code, …))` が 4 箇所。Q2 側が今も本当に未使用かは**未確認** | `lifecycle/quarantine.rs:10-22`、`runtime/src/scratch.rs:163`、`scratch/usage.rs:19,30`、`scratch/conditional_reset.rs:41` |
| 管理 CLI の `core_admin::main` は存在するが、`Route` に入口が無い。コマンドは restart の 5 種のみ | `cli/mod.rs:23-60`、`core_admin/args.rs:6-19` |
| `RestartAdmin::send/targets/status` は capability を検査しない。広告前でも、試験は公開 wire に要求を送れる | `workbench-client/src/restart/session.rs:34-39,76` |
| Q1／Q2 の client は `workbench-client`、`evorch`、`gui` のどれにも無い（Web の `assets/js/restart.js` だけ） | grep（レビュー指摘と一致） |
| restart の拒否理由のうち、未処分は区別されない Conflict で返り、表示は "conflict (the target changed)" | `admin/service.rs:371-379`、`restart/view.rs:182`（レビュー指摘） |
| GUI は `RestartPanel` のモデルだけで、actor・`FeatureId`・window に未配線 | `gui/src/core_client/restart.rs:1-17`、`features/mod.rs:110-146` |
| `AdminService::new` の呼び出しは 5 か所（bootstrap と、試験 fixture 4 つ） | grep |
| `LifecycleDriver::new` の呼び出しは 19 回・15 ファイル | grep |
| `public_restart_cache` ほか 6 target は placeholder。`public_restart_coordinator.rs` に `safe_restart_remains_unadvertised` がある | `evorch-core/tests/` |
| F4-P の `conditions.md` は、5a まで測れない区間として 4 つを挙げる（公開の要求経路、`current` target、製品の旧 core の capture から exec まで、製品同士の Ready） | `measurements/f4-product/conditions.md` の末尾（レビュー指摘） |
| B2 の作業ツリーは `evorch-core/{main.rs,planned_entry.rs,args.rs,Cargo.toml}`、`evorch/src/main.rs`、`gui/src/bin/evorch-gui.rs`、`ci.yml`、`restart/README.md` ほか sandbox・process-guard を変更中（未 commit） | `.worktrees/d3a2-b2` の `git status` |

### 1.2 handoff 文書の分類と計画本文 brief の突き合わせ

記号: W=本番 install と配線、A=属性、T=試験・計測、O=その他、N=handoff に無い項目。

| ID | 内容 | 担当 |
|---|---|---|
| W-01 | Q1 facade の install | V1a-1 |
| W-02 | SafeRestart の広告 | **T1（公開列車）**。広告の仕組みは V0-switch が先に入れ、T1 まで効かない |
| W-03 | Q2 facade・continuation・audit の接続 | V1a-2 |
| W-04 | `UndisposedAdmission` | **V-admission**（live collector の新設。install ではない） |
| W-05 | `FinalizationOwner` | V1b-1（host の所有）＋ V1b-2b |
| W-06 | R の製品接続（umbrella） | 分解済み（W-02〜W-05、N-01、N-02、N-16、T2）。重複 |
| W-07 | CLI の route | V0-reg |
| W-08 | GUI の restart 配線 | V-gui |
| W-09 | Web 管理の本番有効化 | **5a-V の対象外**（認証 PR 後の Web 有効化 PR） |
| A-01 | Q1 の属性 5 行 | V1a-1 |
| A-02 | Q2／runtime seam の属性 4 箇所 | V1a-2（除去して `clippy -D warnings` が通るかで判断。残るなら抑止せず報告） |
| T-01 | 公開 wire cache E2E | V-cache（明示 resume の行を含む。M5） |
| T-02 | 実 core／製品 exec 結合 | V-exec（公開前）、T2・T3（公開列車）。連続 2 回、CEF、旧世代残存なしは F4-E |
| T-03 | Web 認証の正例 | 対象外 |
| T-04 | 連続性 E2E | 対象外（5b-R） |
| T-05 | process-guard の未確認項目 | 対象外（D-3a2） |
| O-01 | cold quarantine の完了条件 | V1a-2 |
| O-02 | Q1／Q2 の UI | CLI は V-ops-q1／q2、GUI は V-gui（拒否理由の表示）と V-ops-gui（画面。任意） |
| O-03 | Q2 物理 leaf E2E | 実施済み（#734） |
| O-04 | process-guard の caller 移行 | 対象外（D-3a2） |
| N-01 | `LocalTargets` の本番構築 | V1b-2a |
| N-02 | `RecipeSource` の本番実装 | V1b-2a（DG-3 の条件つき） |
| N-03 | host の所有構造（`&mut BootstrappedHost`） | V1b-1（DG-2） |
| N-04 | 非 Linux は gate／tx の変更前に Unsupported | V1b-2b |
| N-05 | cold と successor の両入口で同じ install | V1b-1 が hook を置く。確認は T2（両方で 3 facade） |
| N-06 | 未認証 Web の 9 操作と管理 SSE の拒否 | V-deny（最小の負例は各 install PR にも入れる。M1） |
| N-07 | Agent／System の拒否、sandbox から control socket に届かないこと | V-deny |
| N-08 | 確認・CAS・監査障害（本番経路） | V-safe |
| N-09 | 未処分 graph による次 restart の拒否 | V-admission（実装）、V-exec と V-safe（本番確認） |
| N-10 | 性能 gate | V-perf（条件）、T4（実測） |
| N-11 | 試験 target の登録 | V0-reg |
| N-12 | README／コメント／`safe_restart_remains_unadvertised` の更新 | T1 |
| N-13 | Q1／Q2 の client（workbench-client、管理 CLI） | V-ops-q1／q2 |
| N-14 | 公開の切替スイッチ（単一の有効化） | V0-switch（M1） |
| N-15 | restart 拒否の理由（未処分の件数と次の操作）の表示 | V-ops-q2（CLI）、V-gui（Desktop）（M4） |
| N-16 | `FinalizationEnvironment`／`RestartMetadata` の本番構築 | V1b-2b（DG-2） |
| N-17 | `UndisposedAdmission` の live collector | V-admission（DG-1） |

### 1.3 漏れと重複の整理

- 重複: W-06 は umbrella。handoff の 9 件は、重複を除くと配線 7 件。
- V の対象外: W-09、T-03、T-04、T-05、O-03、O-04。
- handoff の 20 件のうち、5a-V の担当は 13 件（配線 7、属性 2、試験 2、その他 1、判断待ち 1）。
- 新しく足した項目は N-01〜N-17（うち handoff に無く本文 brief にあるのは N-04、N-06〜N-10、N-12）。
- #727 の Web 側 UI 2 件（shell Interrupt の UI が DTO 拡張待ち、quarantine discard が現在の DTO では押せない）は、**Web 有効化 PR の残件**として扱う。5a-V では対応しない。
- 属性の除去は配線と同じ PR で行う（別 PR にすると配線 PR が `dead_code` で落ちる）。
