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

## 2. 公開の切替スイッチ（M1）

### 2.1 公開前の状態（誤りの訂正）

計画 改訂 1 は「広告が唯一のスイッチ」と書いたが**誤り**だった（§1.1）。host は広告を見ずに、検証済み local と facade の登録だけで管理操作を受ける。そのため、広告を後回しにしても、install が merge された main では restart・discard・release・resume が実際に動く。

**親の判断（msg-1198）**: V-publish より前の main で、管理操作が実際に動くことを認めない。install の後も、単一の有効化スイッチで有効にするまでは、各操作を fail-closed で拒否する。

### 2.2 スイッチの場所と仕組み

| 要素 | 内容 |
|---|---|
| **スイッチの本体** | `workbench-host` の `AdminService` が持つ `enabled`（既定は off）。`enable_management()` を呼ぶまで off |
| **拒否の位置** | `authorize`、`authorize_request`、`authorize_query`、`submit`、`query` の**先頭**。`UnsupportedCapability`（"management is not published"）を返す。facade の呼び出し、監査の書込み、gate／tx／owner の変更をしない。検証済み local でも拒否する |
| **install との関係** | `install_restart`／`install_quarantine`／`install_owned_graphs` は、off の間も受け付ける（登録と有効化は別） |
| **有効にする箇所（1 か所）** | `evorch-core/src/admin_composition/capability.rs` の `const MANAGEMENT_PUBLISHED: bool = false;`。`true` のとき、かつ 3 つの facade と coordinator が全部登録され、Linux であるときだけ、`enable_management()` と広告（`HostBackend` の runtime flag）を行う。1 つでも欠けると有効にしない |
| **広告** | `server_hello` の `safe_restart.v1` は、`enabled` かつ検証済み local のときだけ。Web には出さない（既存の挙動を維持）。`Capability::IMPLEMENTED` の定数は変えない（DC-31） |
| **境界の検査** | `scripts/check-admin-publish-boundary.sh`（`check-q2-boundaries.sh` と同じ様式）。①製品コードで `enable_management` を呼ぶのは `capability.rs` の 1 か所だけ。②`MANAGEMENT_PUBLISHED` が `true` になる PR は T1 だけ。③`admin_composition::install` の呼び出しは `main.rs` と `planned_entry.rs` に 1 か所ずつ。CI に組み込む（V0-reg）。この script は `capability.rs` を SHA-256 で固定しているため、同ファイルを編集したらハッシュの更新が必要 |
| **試験からの使い方** | 組み立て済みの in-process host（`BootstrappedHost`）の試験は、`AdminService::publish_for_test()` を明示的に呼んでよい（`#[cfg(any(test, feature = "test-support"))]` の下でのみ利用可能）。**feature の無い製品 artifact には有効にする入口が無い**（DC-20 の「製品の main／bootstrap に試験入口を足さない」を守るため）。環境変数や設定の key も作らない |

### 2.3 各 PR の負例（スイッチ off のとき）

| PR | off のときに拒否されることを示す負例 |
|---|---|
| V0-switch | `AdminService` の単体試験: 9 操作（照会を含む）× 検証済み local が全部拒否される。fake facade の呼び出し回数 0。監査 writer の呼び出し 0。hello に `safe_restart.v1` が無い |
| V1a-1、V1a-2、V1b-2b | 組み立て済み host × 実 UDS × 検証済み local: facade を install した状態で、各操作が拒否される。gate／tx／owner／binding が不変。hello に広告が無い。加えて、スイッチを試験で on にした状態でも、**未認証の Web と Agent の変更系が拒否される**（最小の負例。全 matrix は V-deny） |
| V-gui、V-ops-q1／q2 | 広告が無い core では、GUI の panel が出ない。CLI の変更系 subcommand は何も送らず exit 1（`status` ではなく `request`、`discard`、`release`、`resume` で確認する） |
| V-exec、V-cache、V-deny、V-safe | 試験は on にして行う。ただし各 PR に、off のままの構成で 1 操作が拒否される確認を 1 つ入れる |

### 2.4 公開前の製品 artifact で試験できないこと（公開列車）

製品 artifact は feature 無しで、有効にする入口が無い。したがって、**製品→製品の restart と、公開経路の性能の実測は、スイッチが on の状態でしか行えない**。on にするのは T1 だけなので、これらの試験は T1 の後になる。

公開列車（publish train）の案:

- 公開前の PR が全部 main に入った後、main から長命の統合 branch `integration/5a-v-publish` を作る。
- T1〜T4 を、worker 1 人分の PR として、この branch に向けて出す。**各 PR は 1 本ずつ独立レビューと CI を受ける**（J1）。
- すべて承認・緑になった後、**列車全体を対象にした統合レビューを 1 回**行い、通ってから 1 回で main へ merge する（これが V-publish）。**main に T1 だけが入る瞬間は無い。**

| 車両 | 内容 | 規模 | 前提 |
|---|---|---|---|
| T1 V-enable | `MANAGEMENT_PUBLISHED = true`、`safe_restart_remains_unadvertised` の置き換え、README と doc コメントの更新 | S／約8 | 公開前の全 PR、B2 (a)(b)、B3 |
| T2 V-prod | 製品 → 公開要求 → 製品 successor の最小正例（1 周）。cold と successor の**両方で 3 facade** が揃うこと | M／約8 | T1 |
| T3 V-native | 管理 CLI の binary と GUI の actor を、製品 artifact に対して実 restart まで通す。「restart が拒否される → 一覧 → 解除／再開 → 通る」の 1 本（M4） | M／約10 | T1 |
| T4 V-perf-record | T2／T3 の head で、公開経路の性能を実測して記録する（データのみ） | S | T2、B2 (b) |

代替案（§9 の J1）: ①単一の V-publish PR に全部入れる（T1〜T4 で約 30 ファイルになり、worker 1 人分を超える）。②設定の key で opt-in にする（製品に試験でも使う入口が増えるため、DC-20 の解釈を親が決める必要がある）。

### 2.5 巻き戻しの手順

1. **公開前**: 戻すものは無い。install は入っているが、管理操作は全部拒否される（§2.3 の負例が証拠）。
2. **公開後（推奨）**: `MANAGEMENT_PUBLISHED` を `false` に戻す 1 行の PR を出す。または公開列車の merge commit を revert する。どちらでも、install は残り、管理操作は拒否に戻る。
3. **制約**: 実行中の core は、スイッチの状態を起動時に決める。off に戻した build への切替は、core の再起動（idle 終了後の cold 起動、または手動の再起動）で効く。**実行中の core を止める runtime の kill switch は、この計画に含めない。**必要なら別の判断にする。
4. 巻き戻し後も、`check-admin-publish-boundary.sh` が `MANAGEMENT_PUBLISHED = false` の状態を許す。

## 3. PR の分割と依存順序

### 3.1 設計メモ（着手前に独立レビュー。M2、M3）

| ID | 対象 | 決めること | 担当 PR |
|---|---|---|---|
| DG-1 | `UndisposedAdmission` の live collector | collector の所有、revision の発行（CAS または予約）、`commit` の中で I/O なしに lock を保持して gate 遷移できること、lock の順序（public state → admission guard → core coordinator）、Q2 の resume／release との競合、再起動後の再構築 | V-admission |
| DG-2 | host の所有と finalization の構築 | lifecycle の loop で DB の I/O をしないこと、signal と依頼が競合したときの扱い、`LifecycleDriver::new` の署名を変えない方法（19 回の呼び出し・15 ファイル）、`FinalizationEnvironment`／`RestartMetadata` の本番構築（`Arc<Instance>`、signal mask、`OperatorSignals`、`ControlDrainHandle`、successor の argv）、`host.shutdown()` の順序の維持 | V1b-1、V1b-2b |
| DG-3 | `RecipeSource` | 情報源、core 所有でない run を `CoreOwned` にしないこと（DC-35）、lane・team・goal の run の扱い | V1b-2a |

- **3 本のメモは、まとめて 1 回だけ**、実装担当者とは別の Reviewer（Advisor とは別の context）のレビューを受ける（J3）。通過してから、対応する PR の実装を始める。
- DG-3 の**受け入れ条件**（レビューの指摘）: 「全 root を `CoreOwned`、他を `None`」という実装でも通る完了条件にしない。recipe の lane・team・descriptor は再開時に照合される（`recovery_authority.rs:182-185`、`recovery_activation/activate.rs:474-493`）。値が欠けた run は再開されず、未処分に残る見込みなので、**lane・team・goal の run が、公開 restart の後に再開するか、gate の前に拒否されるかを、試験で固定する**。
- 「着手前に確認する」としていた R2、R3 は、この設計メモに置き換える。

### 3.2 一覧

規模: S（〜8 ファイル）／M（〜15）／L（15 前後で試験が重い）。ファイル数は見積もり。

| PR | 内容 | 規模 | 新しい試験の種類 | 前提 |
|---|---|---|---|---|
| V0-switch | 切替スイッチ、`admin_composition` の骨格、広告の仕組み、境界検査の script、既存 fixture 4 つの更新 | M／約16 | 1（スイッチ off の単体） | なし |
| V0-reg | 試験 target の登録、CI の hook、CLI の route | M／約16 | 1（route の単体） | V0-switch |
| V-admission | live collector（Q2 の領域） | M〜L／約12 | 1〜2（競合） | DG-1 |
| V1a-1 | Q1 facade の install、Q1 の属性 | S／約7 | 1 | V0-reg |
| V1a-2 | Q2 facade の install、Q2 の属性、cold の progress の確認 | M／約10 | 1 | V1a-1 |
| V1b-1 | host の所有構造と install の hook（`main.rs`、`planned_entry.rs`、`lifecycle.rs`） | M／約12（署名を変える設計なら L） | 1 | DG-2、V0-reg、**B2 (a) の merge**（J2） |
| V1b-2a | `LocalTargets` の構築と `RecipeSource` | M／約9 | 1（lane・team・goal の固定を含む） | DG-3、V0-reg |
| V1b-2b | `FinalizationOwner`、`FinalizationEnvironment`、`RestartMetadata`、coordinator の install、**試験 helper `composed_core.rs`** | L／約14 | 2（組み立て済み host、finalization の故障） | V-admission、V1a-2、V1b-1、V1b-2a |
| V-gui | GUI の配線と、拒否理由の表示 | L／約14 | 2 | なし（V0 を待たない） |
| V-ops-q1 | quarantine の client と CLI | M／約11 | 2（client 契約、CLI×fake core） | なし |
| V-ops-q2 | owned graphs の client と CLI、restart 拒否の理由表示、実 core の往復 | M／約13 | 2 | V-ops-q1、V1b-2b |
| V-ops-gui（任意） | Q1／Q2 の GUI 画面 | M／約10 | 1 | V-gui、V-ops-q2 |
| V-exec | composed な predecessor → exec → 製品 successor | M／約8 | 1 | V1b-2b |
| V-cache | 公開 wire の cache E2E | M／約8 | 1 | V1b-2b、V1a-2 |
| V-deny | 認可の拒否 matrix | M／約10 | 2 | V1b-2b、V1a-2 |
| V-safe | 確認・CAS・監査障害 matrix | M／約9 | 2 | V1b-2b、V1a-2 |
| V-perf | 性能の条件と計測の手順 | S／約5 | 1（`#[ignore]`） | V-exec |
| T1〜T4 | 公開列車（§2.4） | S〜M | 各 1 | 上記すべて、B2 (a)(b)、B3 |

V-ops-gui は、M4 の 3 条件（§6 の公開条件）が入れば公開後でよい。

### 3.3 依存の図

```text
                 B2(a) ──────────────────────────┐
DG-1 ─ V-admission ──────────────────────────────┤
DG-3 ─ V1b-2a ───────────────────────────────────┤
DG-2 ─────────────── V1b-1 ──────────────────────┼─ V1b-2b ─┬─ V-exec ─ V-perf
V0-switch ─ V0-reg ─ V1a-1 ─ V1a-2 ──────────────┘          ├─ V-cache
                                                             ├─ V-deny
V-gui ──────────────────────────┐                            ├─ V-safe
V-ops-q1 ─────────────── V-ops-q2 ◄──────────────────────────┘
                          │
V-gui ─ V-ops-gui〔任意〕◄─┘

全部 + B2(a)(b) + B3 ─ 公開列車: T1 ─┬─ T2 ─┬─ T4 ─ main へ merge（V-publish）
                                      └─ T3 ─┘
```

### 3.4 並列にできる組み合わせ

| 段階 | 並行できるもの | 備考 |
|---|---|---|
| A（すぐ） | DG-1／DG-2／DG-3 のメモ、V0-switch、V-gui、V-ops-q1 | V-gui と V-ops-q1 は登録も中央ファイルも V0 に依存しない（既存の target を拡張するだけ） |
| B | V0-reg（V0-switch の後）、V-admission（DG-1 の後）、V1b-2a（DG-3 の後） | |
| B' | V1a-1 → V1a-2、V1b-1 | V1b-1 は B2 (a) の merge 後 |
| C | V-exec、V-cache、V-deny、V-safe、V-ops-q2 | 5 本。V1b-2b の後。同時に走らせる worker は 4 本を目安にする |
| D | V-perf（V-exec の後）、V-ops-gui（任意） | |
| E | 公開列車 | |

## 4. 中央ファイルの担当境界（M7）

原則: **同じファイルを編集する PR は、同時に走らせない。** 中央ファイルは、1 つの PR だけが編集者になる。lane A は直列で、lane A の中の重なりは衝突しない。

| ファイル／領域 | 唯一の編集者 | 他の PR の扱い |
|---|---|---|
| `workbench-host/src/admin/service.rs`、`admin/mod.rs`（スイッチ） | V0-switch | |
| `admin_composition.rs`（根）と入力の構造体、`admin_composition/{recovery,restart,capability}.rs`（根）、`bootstrap/mod.rs` の accessor | **V0-switch が、DG-2 の設計に従って必要な入力と accessor を先に全部入れる。** 後から足す必要が出たら、段階 A／B／B' では V1b-1 だけが追記してよい。それ以外の PR は、親に依頼する | 段階 B／B' の並列 PR（V1a-1、V1a-2、V1b-2a、V-admission）は、根と `bootstrap/mod.rs` を**触らない**。消費するだけ |
| `admin_composition/recovery/quarantine.rs` | V1a-1 | |
| `admin_composition/recovery/owned_graphs.rs` | V1a-2 | |
| `admin_composition/restart/{targets,recipes}.rs` | V1b-2a | |
| `admin_composition/restart/finalization.rs` と、根 `restart.rs` の `install` の本体 | V1b-2b（V1b-2a の merge 後） | |
| `admin_composition/capability.rs` の `MANAGEMENT_PUBLISHED` | V0-switch が `false` で作る。T1 だけが `true` にする | |
| `evorch-core/src/main.rs`、`planned_entry.rs`、`lifecycle.rs`、`lifecycle/control.rs` | **V1b-1 だけ**（hook もここで置く。V0 は触らない）。**B2 (a) と同じファイルなので、B2 (a) の merge 後に着手する** | |
| `evorch-core/src/backend.rs`（広告の runtime flag） | V0-switch | |
| `bootstrap/recovery_disposal/*`（collector） | V-admission | V1a-2 は構築だけで編集しない |
| `lifecycle/quarantine.rs`（属性） | V1a-1 | |
| `runtime/src/scratch.rs`、`scratch/usage.rs`、`scratch/conditional_reset.rs`（属性） | V1a-2 | |
| `evorch/src/cli/mod.rs`（route） | V0-reg | |
| `evorch/src/cli/core_admin/*`、`workbench-client/src/restart/*` | V-ops-q1 が最初（構造を作る）。V-ops-q2 はその後（直列） | `cli/core_client.rs` は触らない |
| `gui/src/core_client/{mod.rs,features/mod.rs,features/layout.rs}`、`connection.rs`、`features/restart*` | V-gui | V-ops-gui は V-gui の後（直列） |
| `Cargo.toml` の `[[test]]`、`ci.yml`、`ci-test-groups.json`、`contract-test-filters.sh`、`feature-scopes.txt`、`test_check_central_files.sh`、境界検査の CI hook | **V0-reg だけ**。以降の PR は placeholder を置き換えるだけ。target を足したいときは親へ依頼する | B2 (a) も `ci.yml` と `evorch-core/Cargo.toml` を編集する。hunk が別なので、後に merge する側が rebase する |
| `scripts/check-admin-publish-boundary.sh` と試験 | V0-switch（script 本体）。CI への組み込みは V0-reg | |
| 試験 target の中身 | 1 target を複数 PR が使う場合は、**`mod` を PR ごとに分けた placeholder** を V0-reg が置く。`recovery_admin_install`（`q1`、`q2`）、`public_restart_compose`（`targets`、`recipes`、`finalization`）。各 PR は自分の module だけを編集する | |
| `shared-test-support/composed_core.rs`（組み立て済み core を試験 binary で起動し、公開 wire の client をつなぐ helper） | **V1b-2b**。V-exec、V-cache、V-deny、V-safe、V-ops-q2 は読むだけ。変更が要るときは親へ依頼する | 公開 wire を叩く helper は、ここに置く（V-cache が V-prod の fixture 拡張を待たないため） |
| `shared-test-support/product_exec*`（F4-P の fixture） | **T2 だけ**（拡張が要る場合は新ファイルとして足す）。別 fixture と製品の test hook は作らない（DC-20） | V-exec は fixture の読み取りのみ |
| `workbench-web/src/security.rs`、認証検証器 | 誰も触らない | |
| `measurements/public-restart/*` | V-perf（`conditions.md`）、T4（データ） | |

## 5. 各 PR の詳細

DC-20、DC-30〜DC-40 は全 PR に適用する。特に、製品の main／bootstrap に試験入口を足さない（DC-20）、wire／UI／CLI から path・argv・env を受けない（DC-30）、#710 の状態機械・時計・catalog 比較を二重実装しない（DC-32）、既存の期限（probe 5 秒、capture 前 300 秒、Restarting→Ready 120 秒、Ready→再接続 30 秒、lease 5 秒＋grace 10 秒、stage 5 の 10 秒）を延ばさない。各 PR は、スイッチが off であることを前提に、§2.3 の負例を持つ。

### V0-switch — 切替スイッチと骨格

**範囲**: `AdminService` の `enabled`（既定 off）と拒否の位置、製品コードで `capability.rs` が呼び出す `enable_management()`、および試験専用の `publish_for_test()`（`#[cfg(any(test, feature = "test-support"))]` の下でのみ利用可能）。`HostBackend` の広告 flag（`enabled` かつ検証済み local のときだけ）。`admin_composition.rs`（根、入力の構造体、`install`）と `recovery`、`restart`、`capability` の根と stub（`MANAGEMENT_PUBLISHED = false`）。`bootstrap/mod.rs` の accessor。`scripts/check-admin-publish-boundary.sh` と試験。既存 fixture 4 つ（`public_restart_contracts/support.rs`、`public_restart_coordinator/fixture.rs`、`quarantine/tests.rs`、`admin_contracts.rs`）が `publish_for_test()` を呼ぶように更新する。

**完了条件**: §2.3 の V0-switch の負例。`publish_for_test()` を呼んだ fixture では、既存の契約試験が全部通る。`install` はまだ誰も呼ばない（hook は V1b-1）。`main.rs`／`planned_entry.rs` は触らない。

**試験・計測**: スイッチ off の単体試験（9 操作）。境界検査の script の試験（違反例と正例）。

**前提**: なし。

### V0-reg — 登録と CLI route

**範囲**: placeholder と登録 — evorch-core の `recovery_admin_install`（`q1`、`q2`）、`public_restart_compose`（`targets`、`recipes`、`finalization`）、`recovery_admission`（V-admission が必要なら）、`public_restart_exec`、`public_restart_deny`、`public_restart_safe`、`public_restart_product`、core-client-e2e の `public_restart_native`。`Cargo.toml` の `[[test]]`、`ci.yml`、`ci-test-groups.json`、`verify-test-inventory.sh` の分類、`feature-scopes.txt`、`test_check_central_files.sh`。`check-admin-publish-boundary.sh` の CI hook。`evorch/src/cli/mod.rs` の `core` の route（先頭語が `core` のときだけ。裸の `restart` は今 `Run` の prompt なので routing しない）。product artifact を使う target は、既存の `f4_product_*` と同じ job・同じ `guard-cargo.sh` の起動に載せる。

**完了条件**: 登録した target が inventory 検査と central-files 検査を通る。placeholder は「登録のみ」と明記する。`evorch core restart request` が route され、**広告の無い core には何も送らず exit 1**（`status` は広告を見ずに送るので例にしない）。

**試験・計測**: route の単体試験。`check-central-files.sh`、`verify-test-inventory.sh`。

**前提**: V0-switch。

### V-admission — `UndisposedAdmission` の live collector

**範囲**（DG-1 で確定）: Q2 の領域に live collector を新設し、`UndisposedAdmission` を実装する。`SnapshotClock` は診断用なので使わない。

**完了条件**
- `admit` は、**lock を保持したまま**、snapshot revision と未処分集合の空を照合し、I/O 無しで gate 遷移の closure を実行する。読んで比較する実装は不可。
- 判断した revision と受付時の状態が違えば、再判定する。
- 未処分があるときは gate 変更前に拒否する。
- Q2 の resume／release と admission が競合しても、二重に通らない。
- `scripts/check-q2-boundaries.sh` が通る。

**試験・計測**: 競合試験（clock／barrier／channel／join のみ）。cache 契約には影響しない。

**前提**: DG-1（独立レビュー済み）。

### V1a-1 — Q1 facade の install

**範囲**: `admin_composition/recovery/quarantine.rs`（`StorageQuarantineStore` と `QuarantineFacade` の構築、`install_quarantine`）。`lifecycle/quarantine.rs` の属性 5 行の除去。

**完了条件**: §2.3 の負例（off で拒否、on でも未認証 Web と Agent の変更系が拒否）。on の状態で、実 UDS（検証済み local）から Q1 の一覧・詳細・discard が通る。属性が全部除去され、`cargo clippy --workspace --all-targets -- -D warnings` が通る。

**試験・計測**: `recovery_admin_install::q1`（組み立て済み host × 実 UDS）。

**前提**: V0-reg。

### V1a-2 — Q2 facade の install

**範囲**: `admin_composition/recovery/owned_graphs.rs`（`OwnedGraphsFacade::new(...)` の構築、`install_owned_graphs`）。`runtime/src/scratch*` の属性 4 箇所の除去。cold quarantine の完了条件（O-01）の確認。

**完了条件**
- §2.3 の負例。
- on の状態で、実 UDS から Q2 の `Undisposed` 照会と一覧・詳細が通る。
- cold の graph の discard は、Q2 の progress write が全 thread を記録した後でだけ「完了」になり、それまでは partial／Failed を保つ（#723）。
- 属性: 除去できたものと、残した理由を PR 本文に表にする。未使用が露出したら、抑止せず報告する（削除が大きければ別 PR）。
- `check-q2-boundaries.sh` が通る。

**試験・計測**: `recovery_admin_install::q2`。

**前提**: V1a-1。

### V1b-1 — host の所有構造と install の hook

**範囲**（DG-2 で確定）: `finalization` の依頼を、host を所有する側のループだけが実行する構造にする。`main.rs::serve` と `planned_entry.rs::run` に `admin_composition::install` の呼び出しを 1 か所ずつ置く。`lifecycle.rs`、`lifecycle/control.rs`。`LifecycleDriver::new` の署名は変えない方針（変えるなら本 PR を分ける）。

**完了条件**
- 振る舞いは変わらない。idle、signal、client の drain、restart の状態機械、`host.shutdown()` が planned cleanup を待つ順序（README の "Explicit shutdown ordering"）の既存試験が全部通る。
- 依頼は host の所有側のループだけが実行し、二重に実行できない。失敗・不明でも再実行しない。
- loop の中で DB の I/O をしない。signal と依頼が競合したときの結果が、設計メモの表どおり。
- `check-admin-publish-boundary.sh` の「`install` の呼び出しが両入口に 1 か所ずつ」を満たす。（両入口の効果の確認は T2。公開前は、この script と code review で担保する）

**試験・計測**: 既存の lifecycle／restart／planned_exec 試験の回帰。所有と依頼の新規試験 1 種類。

**前提**: DG-2、V0-reg、**B2 (a) の merge**（J2）。

### V1b-2a — `LocalTargets` と `RecipeSource`

**範囲**: `admin_composition/restart/targets.rs`（`core.restart.targets` と現在の build から `LocalTargets`／`Requirements` を構築）と `restart/recipes.rs`（`RecipeSource`）。

**完了条件**
- DG-3 の受け入れ条件（§3.1）。core 所有でない run を `CoreOwned` にしない。lane・team・goal の run が、公開 restart の後に再開するか、gate の前に拒否されるかが、試験で固定される。
- 権限を新設しない。CLI の所有方針を変えない。
- targets は `current` またはローカル設定の候補 ID だけ。wire から path・argv・env を受けない（DC-30）。

**試験・計測**: `public_restart_compose::{targets,recipes}`。

**前提**: DG-3、V0-reg。

### V1b-2b — `FinalizationOwner` と coordinator の install

**範囲**: `admin_composition/restart/finalization.rs`（`FinalizationOwner`、`FinalizationEnvironment`、`RestartMetadata` の本番構築）、根 `restart.rs` の `install`（`PublicRestartCoordinator::new` と `install_restart`。非 Linux では `unsupported.rs` の実装）、`shared-test-support/composed_core.rs`。

**完了条件**
- §2.3 の負例（off で拒否、on でも未認証 Web と Agent が拒否）。
- on の状態で、実 UDS の公開 wire から targets／status／request／cancel／interrupt が届く。拒否と capture 前の cancel で gate と tx が変わらない。二重 restart が出ない。
- 未処分が空でなければ gate 変更前に拒否する（V-admission を本番の組み立てで確認）。
- `FinalizationOwner` は、Preparing の tx の作成と readback、既存の `start_finalization` の呼び出しだけを行う。第 2 の capture 境界や時計を作らない。失敗・結果不明を再実行しない。
- 非 Linux のビルドで、restart が gate／tx の前に Unsupported を返す。
- `scripts/check-cache-contracts.sh` を実行する。

**試験・計測**: `public_restart_compose::finalization`（組み立て済み host × 実 UDS）と、finalization の故障（失敗、不明 ACK、二重依頼）。

**前提**: V-admission、V1a-2、V1b-1、V1b-2a。

### V-gui — GUI の配線と拒否理由の表示

**範囲**: `FeatureId::Restart` の追加と登録、`features/restart.rs`、`connection.rs`（planned の証拠に応じた reconnect、`on_new_session()`、read-only の `reconcile_queries()`、新 epoch で旧 cursor を捨てる、command／data bind／token を再送しない）。機能の可用性を広告に連動させる。**restart が未処分で拒否されたとき、Desktop の panel が、未処分の件数と次の操作（解除・再開の場所）を示す**（M4）。件数は `OwnedGraphs` の `Undisposed` 照会（読み取り）で得る。

**完了条件**: 確認なしで restart／Interrupt を送れない。対象が変わった確認は破棄される。ACK 喪失、新 epoch、旧 cursor、planned の期限切れで表示が正しく command が再送されない。広告が無い core では panel が出ない。未処分による拒否の表示が試験で固定される。

**試験・計測**: ① actor の再接続（`FakeServer` と paused clock）。② view の描画と GUI の evidence（`docs/gui-verification.md` の規約。要確認）。実 core との結合は T3。

**前提**: なし。

### V-ops-q1 — quarantine の client と CLI

**範囲**: `workbench-client/src/restart/` の隣に quarantine の型付き呼び出しと確認つき intent（一覧、詳細、discard。CAS の binding は view から取る）。`core_admin/` の subcommand（例: `core quarantine list|show|discard`）。試験 harness の拡張。

**完了条件**: **変更系の subcommand（`discard`）は、広告が無ければ何も送らず exit 1。** 確認なしの discard を送らない。古い binding は Conflict と表示する。一覧と詳細は有界で、path・prompt・秘密を含まない。

**試験・計測**: ① client 契約（fake server）。② CLI × fake core（実 UDS、既存の `fake_core`）。

**前提**: なし。

### V-ops-q2 — owned graphs の client と CLI

**範囲**: owned graphs の型付き呼び出し（一覧、詳細、release、resume）。`core graphs list|show|release|resume`。**`core restart request` が未処分で Conflict になったとき、CLI が `Undisposed` を照会し、「N 件の未処分 graph があります。`evorch core graphs list`」と次の操作を示す**（M4）。

**完了条件**
- 変更系の subcommand（`release`、`resume`）は、広告が無ければ何も送らず exit 1。
- release と resume は別操作で、release が resume を暗黙に起こさない（DC-34）。
- 実 core（組み立て済み host、スイッチ on）で、「**restart が拒否される → 一覧に出る → release または resume → restart が通る**」を 1 本通す。

**試験・計測**: ① client 契約。② CLI × 実 core（`composed_core.rs`）。

**前提**: V-ops-q1、V1b-2b。

### V-ops-gui（任意）— Q1／Q2 の画面

V-gui の panel に quarantine／owned graphs の画面を足す。公開後に回してよい（M4 の 3 条件が §6 で担保されているため）。規模 M。前提は V-gui と V-ops-q2。

### V-exec — composed な predecessor から exec する試験

**範囲**: `public_restart_exec`。predecessor は `admin_composition::install` を呼ぶ試験 binary（スイッチを on にする）。successor は実製品 artifact。F4-P の fixture と `composed_core.rs` を使う。**これは製品同士の証拠ではない**（predecessor が試験 binary）。呼び方は公開関数だけで、製品の test hook ではない。

**完了条件**: 公開 wire の要求で、exec して製品 successor が Ready になる。target の分類（`current`、設定した target、無い・非互換）、拒否、capture 前の rollback、successor 失敗の診断が、公開 wire の応答と一致する。未処分 graph による次 restart の拒否（本番の組み立て）。

**試験・計測**: 1 種類。product artifact は PR の head から CI がビルドする。

**前提**: V1b-2b。

### V-cache — 公開 wire の cache E2E

**範囲**: `public_restart_cache.rs`（placeholder の置き換え）。入力から導く mock cache と、独立した wire 比較。predecessor は V-exec と同じ方式（`planned_exec_cache` と同じ考え方）。**製品同士の証拠とは呼ばない。**

**シナリオ**
1. 通常ターンの prefix、tools の schema と順序、affinity key が保たれる。
2. 公開入口の same-build restart: 境界 0。実際に送った effective tools を比較する（DC-22）。
3. 公開入口の異 build で schema が変わる行: tools だけの 1 要求の境界。history、ToolResults、system、model、settings、affinity は不変。次の turn が新しい prefix を再利用する（**必須の 1 行**）。
4. 異 build でも schema が不変なら境界 0。
5. discard／release で履歴を書き換えない。discard の後の通常 Continue は non-handoff の規則。
6. **公開の明示 resume の後、次の provider 要求が planned の規則どおり**（M5）。#734 から引き継いだ、manual Resume 全経路の cache E2E の製品結線。
7. 認証情報、確認操作、監査、再接続の状態が provider 入力に入らない。

**試験・計測**: 1 種類。`scripts/check-cache-contracts.sh`。

**前提**: V1b-2b、V1a-2。

### V-deny — 認可の拒否 matrix

**範囲**: `public_restart_deny`。組み立て済み host ＋ 実 `WebServer` ＋ 実 UDS ＋ 実 facade。

**完了条件**
- 未認証の実 HTTP で、**9 操作すべて**（照会を含む。一覧と詳細が別 method なら両方）を拒否する。`Provenance::human(Web, …)` だけの fixture、偽の identity header、任意の session ID、許可された Host／Origin、loopback でも拒否する。
- 拒否の前後で、gate、tx、owner、binding、runtime の呼び出しが不変。監査以外の書込みが無い。管理 SSE の snapshot、replay、live から管理情報が取れない。
- Agent／System は host 入口で、payload の偽装を含めて拒否される。
- sandbox の負例は、**失敗の理由を固定する**。control socket への到達は、CLI の exit code ではなく、`connect` の失敗（ENOENT／EACCES 等）を直接確認する。管理 CLI の binary を sandbox で実行する試験は、B2 (a) の後に CLI が guard の bind で落ちる別の理由で通ってしまわないよう、guard の bundle 環境を与えたうえで、stderr の理由（endpoint が無い、権限が無い）を確認する。network 許可つき sandbox から loopback Web に要求できても、未認証として 9 操作を拒否する。ユーザーが明示的に許可した unsandboxed shell までは、防御を主張しない。
- sandbox の本体欠陥が見つかれば、sandbox 担当の修正 PR に戻し、公開列車を作らない。

**試験・計測**: ① HTTP／UDS の拒否 matrix。② sandbox の到達性（bwrap が CI で動くこと。動かなければ**失敗させる**。skip しない）。

**前提**: V1b-2b、V1a-2。

### V-safe — 確認・CAS・監査障害 matrix

**範囲**: `public_restart_safe`。監査の障害は、既存の storage の fault feature（`admin-audit-fault`）を使う（試験 build 限定。製品 artifact には入れない）。

**完了条件**: 確認なしの restart と Interrupt の拒否。古い確認、二重クリック、ACK 喪失、同じ ID で内容が違う要求の挙動。Interrupt は、restart ID、epoch、run incarnation、blocker の各不一致で signal の送信が 0。監査の保存失敗で、restart、Interrupt、discard、release、resume の副作用が 0（cancel だけは成立し、失敗が表示される）。5a-I の制約（I-31〜I-34）が本番の組み立てでも成り立つ。未処分が空でなければ拒否され、状態が変わったら再判定される。拒否の洪水と高 cardinality でも、queue、件数、bytes が上限内。

**試験・計測**: ① 確認と CAS の matrix。② 監査障害の matrix。clock／barrier／channel／join のみ。

**前提**: V1b-2b、V1a-2。

### V-perf — 性能の条件と手順

§8 で詳述する。`measurements/public-restart/conditions.md`（**最初の計測の前に commit する**）と、`public_restart_product` の `#[ignore]` の計測関数。公開前は、製品 artifact で管理操作を有効にできないため、**この PR の時点では実測できない**（コンパイルの確認のみ）。実測は T4。

**前提**: V-exec。

## 6. 公開列車と V-publish の完了条件

**前提の確認**（列車を作る条件。PR 本文に表で示す）: V0-switch、V0-reg、V-admission、V1a-1、V1a-2、V1b-1、V1b-2a、V1b-2b、V-gui、V-ops-q1、V-ops-q2、V-exec、V-cache、V-deny、V-safe、V-perf が merge 済み。B2 (a) と (b)、B3 が merge 済み（または親が J8 の選択肢 B を明示承認）。V-ops-gui は任意。Web 認証 PR は未 merge でよい。

**列車の merge 条件（V-publish）**
1. **切替**: `MANAGEMENT_PUBLISHED` を `true` にするのは T1 だけ。`check-admin-publish-boundary.sh` が通る。Linux で、3 つの facade と coordinator が全部登録された core だけが有効になる。Web の接続には広告しない。非 Linux は有効にならず、要求は gate／tx の変更前に Unsupported。
2. **hello**: successor の hello にも `safe_restart.v1` が出る。**同じ core で、検証済み local には出て、Web には出ない。**
3. **Desktop**（T3）: 広告があるときだけ panel が出る。確認 → 要求 → 製品の実 restart を経て、planned の証拠つきで再接続し、新 epoch の snapshot と read-only の照会で状態が戻る。EOF だけで planned と見なさない。
4. **管理 CLI**（T3）: 実 core（製品 artifact）に対して動く。core が未稼働なら起動せず exit 1。広告が無い core には何も送らず exit 1。待機中の Ctrl-C は 130 で、restart と run は止めない。製品に対して実際の restart を 1 回通す。
5. **解除・再開の導線（M4）**
   - 実 core（製品 artifact）で、「restart が拒否される → 一覧に出る → release または resume → restart が通る」を 1 本通す（T3）。
   - restart が未処分で拒否されたとき、CLI と Desktop の panel が、**未処分の件数と次の操作**を示す（V-ops-q2、V-gui の試験で固定済み）。
   - 旧改訂の「導線を公開に含めない」案は採らない。元の依頼は、解除・再開の導線を 5a に含めている。
6. **製品→製品**（T2）: 製品 → 公開要求 → 製品 successor が成立する。同 PID、新 epoch、同 build identity。cold と successor の**両方で** 3 facade が揃う。連続 2 回、CEF、旧世代の残存なしは F4-E。
7. **未認証 Web**: 9 操作と管理 SSE が、公開後の組み立てでも拒否される（V-deny を列車の CI で再実行して緑）。
8. **Agent／System／sandbox**: V-deny の項目が成立する。
9. **cache**: `public_restart_cache` が緑（異 build で schema が変わる行と、明示 resume の行を含む）。`scripts/check-cache-contracts.sh`、`check-q2-boundaries.sh`、`check-central-files.sh` が通る。
10. **性能**: §8 の hard 条件を満たしている。soft の数値はユーザーに報告済みで、採否はその後に決める（gate にはしない。J5。T4）。
11. **残骸**: `grep -rn "5a-V"` の結果が、歴史的な説明を除いて 0。属性の理由に "5a-V" が残らない。`safe_restart_remains_unadvertised` は置き換えられている。
12. **変更しないもの**: 期限、test hook、`workbench-web/src/security.rs`。
13. **開示（PR 本文）**: 認証済み Web の正例は**未検証**で、Web 有効化 PR の担当。#727 の Web 側 UI 2 件も Web 有効化 PR の残件。V-ops-gui が未了なら、その旨（CLI の導線と、Desktop の拒否理由の表示はある）。性能 SLO の採否。B3 の状態。
14. **巻き戻し**: §2.5 の手順が、実際にスイッチを `false` に戻した build で確認されている（組み立て済み host の試験で、`false` のとき拒否される）。
15. **レビュー**: T1〜T4 は各 PR で独立レビューを通す。main への最後の merge の前に、列車全体の統合レビューを 1 回行う。Reviewer は Advisor とは別 context で、上記を実行ログで確認する。

## 7. D-3a2 の B2／B3 との関係（M7）

B2 は (a)（実入口の Installed binding。`main.rs`、`planned_entry.rs`、`evorch/src/main.rs`、`evorch-gui.rs`、`ci.yml` を触る）と (b)（残りの非 PTY owner の移行）に分かれた、と親から聞いている。(a)／(b) の境界は、作業ツリーの差分からの**推測**なので、B2 の worker に確認する。

| D-3a2 | 前提になる PR | 理由 |
|---|---|---|
| **B2 (a)** | **V1b-1 の着手前**（`main.rs`、`planned_entry.rs` を取り合うため）。B2 (a) の後は、製品 core と `evorch` CLI を起動する試験を `guard-cargo.sh` 経由にする（V0-reg が登録する job の起動方法を、B2 (a) の merge 後の main に合わせる）。V-deny の sandbox 試験の失敗理由の固定（§5） | 作業ツリーは `evorch-core/src/{main,planned_entry}.rs`、`evorch/src/main.rs`、`ci.yml`、`evorch-core/Cargo.toml` を変更中 |
| **B2 (b)** | **T4（性能の実測）**、T1 | B1 の README は、guard の保護外に "probes/other commands" を挙げる。restart の preflight と再 probe は `--describe-handoff` の probe 実行（5 秒 deadline）で、guard 経由になると `describe_probe_run`、`preflight`、`reprobe_before_capture` の所要時間が変わる |
| **B3** | **T1**（公開）、F4-E | PTY／Chromium の保護、release-core の expiry、実 Chromium／CEF の E2E。restart の通常経路の時間には効かない見込みだが**未確認** |
| どれも不要 | V0-switch、V0-reg、V-admission、V1a-1、V1a-2、V1b-2a、V1b-2b、V-gui、V-ops-*、V-exec、V-cache、V-deny、V-safe、V-perf | child の spawn 経路を変えない |

**B3 を待たずに進められる範囲**: 公開列車の直前まで全て。つまり、B3 に依存するのは T1（と、それに続く T2〜T4）だけ。B2 (a) に依存するのは V1b-1 だけ。B2 (b) に依存するのは T4 と T1 だけ。

**V0 と B2 (a) の衝突の解消**: V0 を 2 本（V0-switch、V0-reg）に分け、V0-switch は `main.rs`／`planned_entry.rs` を触らない（hook は V1b-1）。V0-reg が B2 (a) と重なるのは `ci.yml` と `evorch-core/Cargo.toml` の別 hunk だけで、後に merge する側が rebase する。

**B2／B3 の merge 後の再実行**: sandbox の起動経路（V-deny）と製品の起動経路（V-exec、V-cache）の挙動が変わりうる。試験のコードは変えず、merge 後の main で再実行して緑を確認する。

## 8. 性能 gate（M6）

**どこで測るか**: 条件の固定は V-perf。実測は **T4**（公開列車の T2／T3 の head、スイッチ on、B2 (a)(b) の後）。公開前は製品 artifact で管理操作を有効にできないため、公開経路の実測は列車の中でしかできない。

**測るもの（F4-P が「5a まで測れない」とした 4 区間を含める）**

1. **公開の要求経路**: 要求の送信から `Accepted` まで（監査の ACK を含む）。**既定の `current` target での計測**を含める（`current` は実行中の ELF の fingerprint を読むため、設定 target と別に測る）。
2. **capture から exec まで**（製品の旧 core、公開経路）。
3. **`Restarting` から Ready まで**（製品同士。**120 秒の期限は `Restarting` から始まる**ので、F4-P の `exec_to_ready` とは別の区間として、`Restarting` 開始時刻から測る）。
4. **exec から owner の claim まで**（製品同士）。
5. F4-P 由来の区間（`preflight`、`reprobe_before_capture`、`before_exec_check`、`describe_probe_run`、`hash_pass`、`cold_spawn_to_ready`）を、同じ環境で再測定した基準線として出す。

**区間と期限の対応表（`conditions.md` に書く下書き）**: 期限の値は F4-P の `conditions.md` と README から取った。コードでの再確認は T4 の前に行う。

| 区間 | 対応する期限 |
|---|---|
| 要求 → `Accepted` | 監査の ACK 10 秒（accept の ACK の上限）。`current` と設定 target を別に出す |
| `preflight`、`reprobe_before_capture`（各 probe 実行を含む） | probe 5 秒（1 回ごと）。capture 前の取消期限 300 秒の内側 |
| capture → exec | stage 5 の drain 10 秒。planned の cohort 10 秒、reap 1 秒、worker join 1 秒 |
| `Restarting` → Ready（製品同士） | boot deadline 120 秒 |
| Ready → 再接続 | 30 秒（クライアント側。ここでは Ready の時刻まで） |
| exec → owner claim | lease 5 秒＋grace 10 秒＝15 秒 |

**条件（F4-P を踏襲）**: release（配布相当）と debug。feature 無しで別ビルド。`limited`（4 CPU、8 GiB、4096 tasks）と `loaded`（同じ制限に niced の CPU stressor 3 つ）。`scripts/reproduce-under-limits.sh` 経由（承認つきの unsandboxed 実行）。`first` は 5、`warm` は 20。sample は捨てない。分位数は nearest rank。**基準線は同じ環境・同じ toolchain で再測定する**（F4-P の数値とは直接比べない）。B2 (b) の bind の計測（cold と warm を各 5 回。最大値、起動区間への増分）を B2 の報告から転記し、probe の 5 秒への余裕と併記する。

**合格の定義**
- **必須（hard）**
  - 全 cell で、失敗・無効な sample が 0。**再試行も deadline の延長もしない。**
  - 既存の期限が変わっていない。**`git diff` の基点は、`conditions.md` を commit した main の commit**（その SHA を `conditions.md` に書く）。期限の定数に差分が無いことを PR 本文に示す。
  - 各区間の最大値が、対応する期限未満。
- **失敗した sample が出たとき**: 条件を変えず、**原因と対処を addendum として先に commit してから**再計測する。addendum の前の失敗した生データも残す。
- **報告（soft）**: 各区間の最大値と、期限に対する余裕（期限 ÷ 最大値）を、`loaded` と `debug` の最悪値で表にして親へ報告する。**数値の SLO を採るかどうかは、ユーザーに報告してから決める。soft の値は gate にしない（J5）。**報告の表では、`loaded` で余裕が 2 倍未満の区間に注記を付ける（合否には使わない）。
- 遅い sample も生データに残す。遅さは CI の正しさの assertion にしない。

## 9. 親の判断（決定済み）

親（msg-1203）が J1〜J8 を次のとおり決定した。本文（§2〜§8）はこの決定に合わせてある。

| ID | 決定 | 計画への反映 |
|---|---|---|
| J1 | **(A) 公開列車を採用。** 統合 branch は `integration/5a-v-publish`。T1〜T4 を積んでから、1 回で main へ merge する | 列車に積む PR（T1〜T4）も、**1 本ずつ独立レビュー**を通す。main への最後の merge の前に、**列車全体を対象にした統合レビューを 1 回**行う（§2.4、§6） |
| J2 | **(A)**。B2 (a) を先に merge する | V1b-1 は B2 (a) の merge 後に着手する（§3.2、§4、§7） |
| J3 | 設計メモ DG-1〜DG-3 は、**まとめて 1 回だけ**独立レビューを受ける。レビュアーは実装担当者とは別の人 | §3.1。3 本を 1 つのレビュー依頼にまとめる。DG-1 に依存する V-admission、DG-2 に依存する V1b-1／V1b-2b、DG-3 に依存する V1b-2a は、この 1 回のレビューの通過後に着手する |
| J4 | **(A)**。V-ops-gui は公開の後に回す | 必須の PR は 20 本、V-ops-gui は公開後（§3.2、§6 の 13） |
| J5 | 余裕 2 倍の soft 基準は **gate にしない**。数値を報告するだけ。採否はユーザーに報告してから決める。**hard 基準は必須**（失敗する sample が 0、期限を変更しない、最大値が期限未満） | §8 の soft の項は「報告のみ」。「要判断として明示する」は、報告の表の注記にとどめ、合否に使わない。§6 の 10 は、hard 条件の充足と、soft の報告をユーザーに出すことを条件にする |
| J6 | **(A)**。T3 で、GUI の actor を製品 artifact に対して実際の restart まで通す | §6 の 3（T3） |
| J7 | **(A)**。kill switch は作らない | §2.5 の 3 |
| J8 | **(A)**。公開は B2 の (a) と (b)、および B3 の後 | §6 の前提、§7 |

**確認待ち**: B2 の (a)／(b) の境界は、作業ツリー（`.worktrees/d3a2-b2`）の差分からの**推測**である。B2 を担当する worker の PR 本文で確定する。確定したら、§4 と §7 の記述を合わせる。

V-cache の predecessor の方式は、`planned_exec_cache` と同じ考え方で妥当（レビュー）。製品同士の証拠とは呼ばない、とした。判断は不要。

## 10. 計画レビューへの対応

| 指摘 | 対応 | 節 |
|---|---|---|
| M1 「広告が唯一のスイッチ」は誤り | 誤りを訂正。`AdminService` の `enabled`（既定 off）を、`capability.rs` の `MANAGEMENT_PUBLISHED` の 1 か所だけが有効にする仕組みに変更。各 PR に、off で拒否する負例と、on でも未認証 Web と Agent の変更系を拒否する最小の負例を追加。V-ops は広告が無ければ何も送らない。巻き戻しを書き直した | §2、§5 |
| M2 `UndisposedAdmission` の前提（R2）が成り立たない | V1a から切り出して V-admission にした。DG-1 の設計メモと独立レビューを前提にした | §3.1、§5 |
| M3 V1b の設計を独立レビューに | DG-2（host の所有、`FinalizationEnvironment`／`RestartMetadata`）と DG-3（`RecipeSource` の受け入れ条件）を追加。`LifecycleDriver::new` の 19 呼び出しに触れない方針と、署名を変える場合の分割を明記 | §3.1 |
| M4 解除・再開の導線の完了条件 | 実 core の往復（V-ops-q2 と T3）、CLI と Desktop の拒否理由の表示（V-ops-q2、V-gui）を完了条件に追加。「導線を公開に含めない」案を外した | §5、§6 の 5 |
| M5 V-cache に明示 resume の行 | シナリオ 6 を追加 | §5 |
| M6 性能 gate が公開経路を測っていない | `current` target、`Restarting` → Ready と 120 秒、区間と期限の対応表を追加。要求から `Accepted` まで（監査の ACK 10 秒）も測ります。失敗時は addendum を先に commit してから再計測します。`git diff` の基点も決めました | §8 |
| M7 中央ファイルの矛盾と B2 の分割 | V0 を 2 本に分け、`main.rs`／`planned_entry.rs` は V1b-1 だけにした。根と `bootstrap/mod.rs` は V0-switch が先に入れ、段階 B／B' の並列 PR は触らない。B2 (a)(b) を前提に反映。B2 (a) 後は guard-cargo 経由 | §4、§7 |
| should: V1b-2 の分割 | V1b-2a と V1b-2b に分けた | §3.2 |
| should: V-ops の分割 | V-ops-q1 と V-ops-q2 に分けた | §3.2 |
| should: V0 の完了条件 | 例を `request` にした。「両入口で 1 回」の単体試験は削り、script と T2 で担保 | §5 |
| should: V-publish の hello | successor と、local／Web の差を条件に追加 | §6 の 2 |
| should: V-deny の失敗理由 | `connect` の失敗の直接確認と、guard の bundle 環境での stderr の理由に固定 | §5 |
| should: helper の置き場所 | `composed_core.rs` を V1b-2b に置く。製品の fixture 拡張は T2 だけ | §4 |
| should: 性能 | addendum を先に commit、要求→`Accepted` を追加、`git diff` の基点を決めた | §8 |
| should: #727 の Web UI 2 件 | Web 有効化 PR の残件として明記 | §1.3、§6 の 13 |

## 付録: 触らないもの

- `workbench-web/src/security.rs` と認証検証器。
- `crates/evorch/src/cli/core_client.rs`。
- 既存の期限と定数。
- 5b-R、F4-E、O-V3、Web 有効化 PR の範囲。
