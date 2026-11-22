出典: planner run-01m7ypc53z68p1xzpktc5dwrat
# D-3a2 B3 分割計画

**推奨は9 PRです。PTY、agent Chromium、実プロセス結合試験を分離し、最後に性能・監査を閉じます。** ファイル数は見積もりであり、分割上限にはしません。

読み取りだけで調査しました。ファイル編集、ビルド、テスト、GitHubの最新状態の再取得はしていません。#750は既存の本文・レビュー・最終報告を参照しています。

## 1. 位置づけと、先に解消する論点

### 全体計画との関係

指定された全体計画の最終assistantメッセージと、`docs/single-core/5a-v-plan.md` を確認しました。

- 5a-Vの旧公開列車は単一のPR Aに統合する。B3-09の縮小した性能確認はPR Aの前提にしない。
- V1b-2bなど、公開前の実装はB3と並行可能です。
- B3の個別PRがmergeされただけではD-7完了としません。Aを含む証拠を揃え、独立Reviewerと親の受け入れが必要です。
- B3を通すために管理機能を先行公開することは、現計画の範囲外です。

### expiry の証拠と製品 ELF の確認

現行の `planned_shutdown_watchdog_composition` は、**テスト実行ファイルのコピー**を子として使います。これを、そのまま製品ELF自身のexpiry証拠とは扱えません。

親の判断 A により、B3-07 は製品と同じ部品で組んだ **release 版の試験 binary** で、不可逆 capture 後の実時間10秒、exit 74、所有する子の回収を確認します。これは製品 ELF での expiry の確認ではありません。
製品 ELF での同じ確認は、5a-VのPR A（実restart）で行います。試験時だけ管理機能を公開する特別なbuildは採用しません（出典は末尾の親の判断 A）。

---

## 2. 全PR共通の実装契約

根拠は `docs/single-core/d3a2-B-v3-approval-review.md`、両README、B2の実装です。

### 所有と終了

- Linuxではtyped owner APIを使用し、raw `Command`、任意`pre_exec`、bare PID/PGIDを所有入口として再公開しない。
- 数値PGIDへのsignalは、guardがrootを未reapで保持している期間に限る。
- `exited`は観測だけ。sticky wait、raw exit status、キャンセル安全性を維持する。
- `FinalStatus`は**root reap＋group kill送信済み**を意味する。全子孫のreap完了とは称さない。
- `GuardLost`は未確認のまま扱い、成功・ledger解放・restart続行に変換しない。
- owner死亡・half-close・force/dropと、通常TERMの意味を混同しない。helperへの`kill_on_drop`で代用しない。
- 保証対象はrootと元の同一PGIDの子孫。`setsid`等で逃げた子孫まで保証を広げない。

### 起動と互換性

- PID/UID認証、Commit前fork禁止、Readyまでの絶対5秒期限を維持。メッセージ受信ごとに期限を延長しない。
- helperの環境はclear。targetのargv/env/cwdは認証済みlaunch spec経由。
- installed sibling ELF、compat検査、image pinningを維持。検索・再open・旧方式へのfallbackを追加しない。
- PTYは承認済みの `/proc/<core-pid>/fd/N` locatorを使う。同一PID namespace・proc可視性等が成立しなければfail closed。
- Linux ≥5.11を前提とする。非Linuxには既存保証を超える主張をしない。
- feature依存のwire形式を作らない。protocol変更時はhelperのcompat/build入力・bundleを一緒に更新する。

### 試験

- READY／ExecOk／root終了／FinalStatus／pidfd等で同期する。sleepや`kill(pid, 0)`だけを死亡証拠にしない。
- mutationは必須にしない。実施する場合は、効果のある代表的な境界だけをテスト用変更で確認し、製品用fault環境変数・設定を増やさない。
- mutationを行う場合は「起動できなかった」だけでなく、**対象が起動した後、期待した所有保証が破れて失敗すること**を確認する。
- 外側controllerが観測済み子孫をpidfd等で管理し、失敗・mutation・escaped-PGIDを含め必ず回収する。必要なsubreaperはcontroller側だけ。
- 性能gate以外の観測timeoutは、環境とともに試験前に固定する。死亡の観測timeoutを製品の終了時間保証と混同しない。

各PR節に記載したmutationは候補であり、実施を受け入れ条件にしない。mustは製品の誤動作、データ破損、セキュリティ、hang／flaky、事実と異なる記載に限る。レビューは2往復までとし、それを超えた場合は親が裁定する。出典：`question-41eda9b4579b24b09e86a96a7e751928`。

---

## 3. 着手前作業：契約・対象表の固定

これは実装PRではなく、親と各担当への入力です。

1. **baseline固定**
   - #750 mergeを含むmain、V1b-2b、並行harness修正の各SHAを記録する。
   - workerは編集前に更新し、実際の差分から作業範囲を確認する。ファイル数でPRを分割しない。
2. **spawn台帳を作成**
   - 行ごとに「実caller／owner process／ChildKind／ledger有無／起動API／子孫／正常・異常時の終了証拠」を記載。
   - 次の三つを別表にする。
     - B3承認条件の11 root。
     - B2(b)-iiiの13 synthetic ownership経路。
     - 現在の実evorch-coreから到達する実caller。
   - **13 synthetic経路を、実core・実callerの証明で代用しない。**
   - `Probe`には現在の製品callerがないため、数合わせの製品経路を作らない。旧11枠との対応・欠落は親に提示する。
3. **expiry試験の実現性確認と製品 ELF での後続確認**
   - B3-07 は、製品と同じ部品で組んだ release 版の試験 binary で、capture、signal、cleanup停止、実時間10秒、exit 74、所有する子の回収を確認する（末尾の親判断 A）。
   - 製品 ELF での同じ確認は5a-VのPR A（実restart）で行う。ここで確認できなければ検証完了としない。製品hook・時計短縮・試験時だけ管理機能を公開する特別なbuildは使わない。
4. **測定条件固定**
   - 性能baseline、製品artifact、環境、観測期限を先に決定する。B3-09は負荷なし、1 round、50組とする。

---

## 4. PR単位の計画

ファイル数はmanifest・テスト・fixtureを含む概算です。新設ファイル名は提案です。

### B3-01：#750 follow-up――closed-fd境界とdoc link

**範囲・担当境界：5〜9ファイル**

- `crates/process-guard` の低FD処理と関連テスト。
- `crates/sandbox` のowned spawn境界テスト。
- `crates/sandbox/src/ledger.rs:267` の存在しないdoc link修正。
- 問題が再現した場合のみ、対象のFD確保・複製処理を最小修正。

**依存：** #750。PTY backend、Chromium担当より先に共有低レベル部分を確定する。

**完了条件**

- Rustプロセス起動**後**に0/1/2を閉じ、API呼出し直前に`EBADF`を確認する試験がある。
- inherited／明示pipe・null・file／混在で、control・image・exec-error FDとの衝突、誤close、リークがない。
- 既存の「Rust起動時に閉じたstdioが正規化される」試験と区別する。
- targetで必ず`EBADF`を維持する、という新契約は導入しない。

**代表的な負例・mutation候補（mutationは任意）**

- 0/1/2の単独・複数close、早期起動失敗、繰り返し起動。
- 非stdio FDを3以上に退避する処理の無効化など、関連するFD衝突mutationを検出する。
- no-op修正なら「既存APIで通った」ことを明記し、架空の不具合修正にしない。

**期限・性能：** 既存5秒起動期限を維持。性能の独立gateは設けず、B3-09で統合測定。

---

### B3-02：process-guardのtyped PTY backend

**範囲・担当境界：10〜18ファイル**

- `crates/process-guard/src/lib.rs`
- `guard.rs`、`image.rs`、protocol定義、専用PTY backend。
- 同crateのmanifest、build/compat入力、テスト。
- このPRではshell callerを移行しない。

**依存：** B3-01。

**実装方針**

- launch specに型付きのdirect／PTY区別を導入する。
- 現在の`GuardChild`の`std::process::Child`依存を内部抽象化し、PTY helperにも同じ認証・decoder・wait・reaperを使う。
- PTY helperはsession leader・controlling terminal保持側。target shellは**同SID・別PGID**とする。
- shellのPGID設定とforegroundへの設定を終えてからexecを許す。現在のtarget側の無条件`setsid`はPTYでは使わない。
- signal mask/dispositionを復元し、foreground設定時の`SIGTTOU`等を扱う。
- `portable-pty`を通常依存にする場合はdependency receiptも更新する。任意raw launcher callbackの公開による回避はしない。

**完了条件**

- direct経路の保証を保ったまま、typed PTY childから既存の所有操作ができる。
- helperはtarget PGID外に残り、Ctrl-C等でhelper自身を殺さない。
- image pinはexec/auth完了まで保持する。

**代表的な負例・mutation候補（mutationは任意）**

- 認証不一致、Ready遅延、Commit前後の切断、exec失敗、owner死亡、guard喪失。
- `/proc`アクセス不可、image取り違えでfail closed。
- targetへの余分なFD漏出がない。
- foreground設定省略、誤った`setsid`、core-death監視除去、pinしたimageでなくpathnameを再openするmutation。

**期限・性能：** Ready絶対5秒。PTYの機能試験期限は事前固定。計測コードは製品経路に入れない。

---

### B3-03：sandbox／shellのPTY caller移行

**範囲・担当境界：14〜22ファイル**

- `crates/sandbox/src/owned/*`
- `crates/sandbox/src/exec*` のPTY用変換。
- `ledger.rs`、`ledger/children.rs` のtyped PTY登録。
- `crates/tools/src/tools/shell/jobs.rs`
- `jobs/process.rs`、`process_tests.rs`。
- evorch-coreのrestartテストはまだ変更しない。

**依存：** B3-02。

**実装方針**

- 例：`OwnedPtyCommand`／`OwnedPtyChild`相当を用意する。API名より、raw所有入口を露出しないことを優先。
- admission取得からopenpty・起動・登録までを一つの所有操作にする。
- blocking起動をawait中にキャンセルしても、処理完了後のRAIIで回収される構造を維持する。
- `JobGroup::Pty(ProcessGroup)`をtyped groupへ変更する。
- resize、readerのEIO→EOF、EOTによる入力close、終了コード変換を維持する。

**完了条件**

- shell PTY製品経路からraw `ProcessGroup`所有が消える。
- 通常の`sh -c` rootと同一groupの孫を所有できる。
- tty、foreground、resize、Ctrl-C、EOF、cancel、timeoutが既存と同等。
- ledger fence、登録失敗、起動キャンセルのどれでも孤児・偽のreap確認を作らない。

**代表的な負例・mutation候補（mutationは任意）**

- admission拒否、PTY生成失敗、exec失敗、起動中cancel、reader/writer失敗、root先行終了＋残存孫。
- 明示的にPGIDから逃げた子を観測し、保証対象外と記録した上でcontrollerが回収。
- rootだけkill、group kill省略、登録前キャンセルcleanup省略、foreground設定を無効にするmutation。

**期限・性能：** 既存job timeoutの意味を変更しない。PTY操作の観測期限は試験前固定。性能はB3-09。

---

### B3-04：agent Chromiumをowned launch＋connectへ移行

**範囲・担当境界：10〜18ファイル**

- `crates/browser/src/agent/chromium.rs`
- browser内のlaunch adapter、endpoint parser、fixture。
- `crates/browser/tests/agent_chromium.rs`、関連observationテスト。
- browser manifestと必要なlock差分。
- sandbox/process-guardの共有実装は変更しない。

**依存：** B3-01と既存non-PTY API。B3-02／03と並行可能。

**実装方針**

- `Browser::launch`をやめ、owned childでChromium自身を起動し、`Browser::connect_with_config`する。
- guardをChromium executableのprefixとしてupstream launchへ渡さない。
- Chromiumは`ChildKind::Other`。ledgerなしでもguardを使う。
- stdin inherit、stdout null、stderr pipeを維持。
- Chromiumの既存default引数を含むadapterを固定し、chromiumoxide 0.9.1との差をgoldenで検出する。
- detection、canonicalization、proxy、profile、cwd、headless、window設定を維持する。
- HandlerConfigも固定する：既存30秒request timeout、viewportなし、cache有効など、現在の実効値と一致させる。
- endpointはExecOk後の**絶対20秒、総量256 KiB、1行16 KiB**。EOF・不正UTF-8・不正endpoint・上限超過は失敗。
- 成功後は既存同様にstderr readerを閉じる。無制限drain/log taskを増やさない。
- **browser-wide closeは追加しない。** childを持たないBrowser側のkill/waitに所有を委ねない。

**完了条件**

- 旧launchとのargv/env/cwd/stdio/config比較と実Chromium起動が通る。
- argvの比較は異なるkeyの不定順序だけ正規化する。同一keyの重複・順序・値変更を隠さない。
- envは順序ではなく全key/valueで比較する。fixtureは秘密を含まない制御環境で動かす。
- init途中、CDP接続失敗、cancel、drop、resetでchild・task・profile・proxyが残らない。

**代表的な負例・mutation候補（mutationは任意）**

- endpointの遅延・洪水・長行・EOF・不正値、CDP失敗、handler終了、reset反復。
- 所有を旧`Browser::launch`へ戻す、RAII cleanup省略、parser上限・絶対期限除去、default引数変更のmutation。

**期限・性能：** 上記20秒／容量制限は既定。起動時間・reset反復の追加性能閾値が必要なら、親が測定前に決める。

---

### B3-05：Linux raw所有APIの撤去と残存spawn監査

**範囲・担当境界：8〜16ファイル**

- `crates/sandbox/src/process*`、ledgerのraw variant。
- `crates/evorch-core/src/restart/tests/finalization/m2.rs`
- `crates/evorch-core/shared-test-support/product_exec/process.rs`
- 対応するテスト・READMEの残件表。

**依存：** B3-03。core側ファイルはV1b-2b担当から編集権を引き継いだ後。

**範囲**

- Linuxの公開所有入口から、次を撤去する：
  `spawn_pty`、`retired()`、`ProcessGroupGuard`、`wait_for_exit`、`has_exited`、`register_pty_group`。
- `m2.rs`のsynthetic retired receiptを、typed所有を使うfixtureへ置換する。**未確認reapがfinalizationを止める**という試験目的は維持する。
- `close_inherited_fds_on_exec`は所有APIではなく、現在product-execテスト用。必要な処理をtest-support内へ移し、公開raw API存続の理由にしない。
- 非Linux private backendまで無理に同時撤去しない。

**完了条件**

- Linux製品経路と公開APIにraw所有の抜け道がない。
- 残る`std`／tokio spawnを台帳で全分類する。
- GUIの短命git／`fc-match`、launcherのcore起動、private preflight supervisorを「未移行」と「意図的対象外」に分ける。

**代表的な負例・mutation候補（mutationは任意）**

- raw所有APIへアクセスするcompile-failまたは同等の境界検査。
- `GuardLost`／未確認reapでworker join・storage drainへ進まない。
- 未確認を成功扱いするmutationをrestart試験が検出する。
- descriptor hook移設後もproduct-execのFD漏出検査が有効。

**期限・性能：** 新しい待機期限を導入しない。性能gateなし。

---

### B3-06：実evorch-core＋実callerのSIGKILL matrix

**範囲・担当境界：16〜26ファイル**

- evorch-coreの新規専用integration test群。
- `shared-test-support` の専用observer／fixture。
- 必要な既存callerの外部fixture。
- core-launcherのpreflight回帰テスト。
- test登録・CI配線はこの担当が取りまとめる。
- runtime本体や並行harness修正対象は変更しない。

**依存：** 基盤作成は先行可能。完了にはB3-03／04が必要。

**実装方針・完了条件**

- 実coreから実callerを起動し、READY後に**coreだけ**をSIGKILLする。
- API単体13経路は残しつつ、実caller matrixを別に提出する。
- 各行でrootと同一PGIDの孫の死亡を観測し、owner PIDがcoreであることを記録する。
- js-workerは実evorchのworker入口を使う。READY後は未解決Promise等で滞留し、coreとのEOFだけで終了する偽陽性を避ける。
- js-workerのbind-before-logging除外と、初期exec後に新しいprocessを作らない性質も確認する。
- preflight supervisorは二重guard化せず、core死亡・EOF・TERM・exec失敗の既存保証を回帰確認する。

**代表的な負例・mutation候補（mutationは任意）**

- 実MinimalSandbox/bwrap経路と、テスト注入DirectSandbox経路の両方。
- guard除去時にもbwrapの`--die-with-parent`が成功を装わないこと。
- root-only kill、owner pidfd監視除去、早すぎるreap、raw spawnへ戻すmutation。
- 各実callerのguard bypassで期待通り失敗すること。コード経路を通らないmutationは証拠にしない。

**期限・性能：** core死亡後の観測期限は親が実行前に固定。速度のCI合否判定は設けない。

---

### B3-07：実10秒expiryとexit 74の結合

**範囲・担当境界：8〜16ファイル**

- B3-06の専用test-supportを利用するexpiryテスト。
- 既存watchdog compositionテストの必要最小限の再利用・整理。
- restart製品ロジック、finalizationの実装は原則変更しない。

**依存：** B3-06、V1b-2bとの編集調整。試験 artifact は末尾の親の判断 A で決定済み。

**完了条件**

- 製品と同じ部品で組んだ release 版の試験 binary を使う。製品 ELF 自身の expiry 確認とは区別する。
- このincarnationでの不可逆captureとexplicit shutdown signalを成立させ、cleanupを停止させたまま**実時間10秒**を経過させる。
- **exit 74**の後、所有する子の回収を確認する。
- 製品 ELF での同じ確認は5a-VのPR Aの実restartで行い、検証完了条件に含める。
- Aの短い終了tailにcleanup、logger、allocation、lock等を足さない。
- 11 rootとの対応表、実caller表、未対応事項を提出する。

**代表的な負例・mutation候補（mutationは任意）**

- capture前、別incarnation、captureのみ、signalのみでは誤発火しない。
- capture→signal／signal→captureの順序を確認する。
- guard除去で子が残り、試験が失敗する。
- watcher無効化・条件緩和・deadline更新で、74／非発火条件の試験が失敗する。

**期限・性能：** watchdogは実10秒固定。scheduler等を含む**観測許容幅は測定前に親が決定**。時計短縮で代替しない。

---

### B3-08：実Chromium／renderer＋CEFの結合回帰

**範囲・担当境界：10〜20ファイル**

- browser E2Eの専用ケース。
- renderer／CEFのintegration fixture。
- B3-06のobserver利用側。
- GUIの既存fixture/eventsやthemeは変更しない。

**依存：** B3-04、B3-06。SIGKILL部分はB3-07と並行可能。expiry部分の完了にはB3-07が必要。

**完了条件**

- agent Chromiumの実起動、navigation、reset、drop、init失敗が通る。
- 実rendererとCEFを初期化し、navigation完了後にcore SIGKILL／74を発生させる。
- ChromiumとCEFを別経路として証明する。片方の成功で他方を代替しない。
- O-V1／#714 browser-e2eを回帰実行する。O-V2は19a後の後続作業とし、B3-08の完了条件に含めない。
- 実process tree、SID／PGIDを記録し、同一groupとescaped groupを区別する。

**代表的な負例・mutation候補（mutationは任意）**

- renderer起動失敗、CEF初期化失敗、navigation中のcore死亡。
- agent Chromium、rendererそれぞれのguard bypass。
- CEF fixtureを起動しただけで終了し、実navigationを省いたケースを成功扱いしない。
- 保証対象外のprocessもcontrollerが回収する。

**期限・性能：** Chromium endpointは20秒。CEF初期化・navigation・終了観測のtimeoutと実行環境は親が事前固定。環境不足はskip成功で閉じない。

---

### B3-09：性能gate、監査完了、証拠集約

**範囲・担当境界：6〜14ファイル**

- 性能runner、入力fixture、結果schema。
- `crates/sandbox/src/owned/README.md`
- `crates/evorch-core/src/restart/README.md`
- 必要なCI最終配線。
- `docs/`の運用記録はコードPRに混ぜず、規約どおりmain上の別変更。

**依存：** B3-05／07／08。測定仕様の準備だけは最初から行う。

**完了条件**

- 条件・source SHA・artifact digest・生データ・集計方法を揃える。
- 下記性能gateを満たす。超過時は測定条件を後付けで緩めず再レビューへ戻す。
- 残存spawnと保証対象外を説明し、B3の未完項目を隠さない。
- `scripts/check-cache-contracts.sh`を含む関連回帰が通る。cache契約を弱めない。

**任意のmutation候補**

- 実施する場合は、集計器が遅延増大・PSS超過・欠損sample・timeoutを正しく拒否する代表セルを選ぶ。
- mutationを実施した場合は、対象commitと結果を記録する。mutation一覧の網羅・集約を必須にしない。

---

## 5. 性能gate

測定方針は `question-41eda9b4579b24b09e86a96a7e751928` に基づく。負荷を加えず、1 round・50組で測る。閾値は実測前に固定し、結果を見て変更しない。

| 項目 | 基準 |
|---|---|
| A/B*比較 | AはA実装済み・B3未導入、B*はB3最終版。同一toolchain・flags・features |
| shell workload | `grep -F -- D-7 fixture.txt`、`git diff --quiet -- tracked.txt`、`:` |
| 測定 | 実shell入口のrequest→response RTT |
| 条件 | 外部負荷なし。各workload 50 paired samples、1 round |
| 順序・集計 | ABBA、seed固定、nearest-rank。恣意的な外れ値除去なし |
| latency | p50(B*) ≤ 1.5×p50(A) |
| ForceKill | request→root exit：p99 <1,000 ms |
| guard PSS | 100 guardsを180秒保持：median ≤4 MiB、p95 ≤8 MiB |

補足：

- historical Aと現在mainの無関係な変更が大きい場合、比較の分離方法を**先に**親が判断する。製品にunguarded切替flagを追加してbaselineを作らない。
- rendererの同期handshake、Chromium cold start、CI所要時間は別途記録し、B3-09の合否閾値に追加しない。
- ForceKillとguard PSSの閾値は従来どおり維持する。性能gateは測定結果で判定し、通常CIの揺れる時間assertionにはしない。

---

## 6. 並列化と編集競合

### 依存順序

```text
#750
 └─01 closed-fd
     ├─02 PTY guard ─03 PTY caller ─05 raw撤去
     └─04 Chromium

03＋04 ─06 実core SIGKILL ─07 expiry
                   └─────08 Chromium/CEF
                          （expiry部分は07に依存）

05＋07＋08 ─09 性能・監査 ─ 独立レビュー／親受け入れ
```

B3-09は5a-V PR Aと独立し、PR Aの前提にしない。

- 事前の対象表・expiry実現性調査・測定条件作成は並行可能。
- 02と04は並行可能。06のfixture準備も先行可能。
- 07と08は専用test leafに分けて並行。ただし共通observerのinterfaceは06で固定する。
- 05のcoreテスト変更は、V1b-2bとの引き継ぎ後に直列化する。

### 編集権の境界

| 領域 | 方針 |
|---|---|
| `process-guard`本体 | 01→02の直列。browser担当は編集しない |
| sandbox owned／ledger | 03担当。その後05に引き渡す |
| `admin_composition/restart`、製品finalization | V1b-2b担当。B3から直接変更しない |
| `composed_core`等の共有fixture | V1b担当とinterfaceを固定して再利用。二重実装しない |
| coreの新規B3テスト | 06〜08で専用leafを分割 |
| Cargo.lock、共通manifest、CI | merge窓を一人が管理。並行一括編集を避ける |
| 両README | 各段階の必要差分は直列、最終残件整理は09担当 |

ユーザーのharness修正対象は**原則編集禁止領域**とします：

- runtimeの`meta/runs/waiting*`、`agent_loop*`、`rules`、`compaction`
- `event-bus/src/event.rs`
- `storage/src/projection.rs`
- `evorch/src/cli/inspect.rs`
- `gui/src/fixture/events.rs`
- `gui/src/theme/tokens.rs`

B3試験は既存公開入口と外側fixtureから組みます。変更が不可避なら、その時点で必要差分を親へ提示し、引き渡しまで止めます。

---

## 7. 親の判断が必要な事項

### A. expiryを何のartifactで証明するか

※ 以下は判断前に比較した選択肢です。採用する内容は末尾の「親の判断」Aで確定しました。

判断前に比較した選択肢：

| 選択肢 | 証拠の強さ | PR Aへの影響 | 規模 |
|---|---|---|---|
| release production-composition **test binary** | production部品・実10秒・74は証明。製品ELF入口は未証明 | PR A内に製品ELFの確認が必要 | 主に07内 |
| sealed製品ELFを必須 | 実製品入口からの最も強い証拠 | PR Aの公開・検証に含める | 追加の統合・計画変更が発生し得る |

この比較は親判断Aで決定済み。採用結果は末尾の親判断Aを参照。

### B. 「11 root」と現在の実callerの対応

- 選択肢：旧枠を維持する／実callerに合わせて対応表を改訂する。
- **推奨：API網羅表と実caller表を分離し、意味のある網羅性を維持して親が対応表を承認する。**
- callerのない`Probe`を架空の製品経路で埋めたり、13 syntheticの数だけで達成扱いしない。

### C. 監査対象外のstd spawn

- 選択肢：GUI短命git・`fc-match`等を明記して対象外／B3に追加移行。
- **推奨：既存のcore ownership境界を維持し、GUI等は台帳と別follow-upへ。**
- ただし新たに見つかった**core所有の長寿命spawn**は、理由なく除外しない。
- 全アプリのspawn統一を求める場合は、9 PRへ押し込まず追加PRを設ける。

### D. 測定環境・期限

親が測定前に確定するもの：

- A/B source SHAと無関係差分の扱い。
- kernel／Chromium／CEF／bwrap／display環境。
- SIGKILL後の観測期限、10秒expiryの観測許容幅、CEF各段階のtimeout。
- 追加起動時間gateの要否。
- 最古対応kernelの実測範囲と、未検証環境の扱い。

**更新済みの測定方針：** B3-09は負荷なし、1 round、50組、p50(B*) ≤ 1.5×p50(A)とする。ForceKillとPSSの既存閾値は維持する。出典：`question-41eda9b4579b24b09e86a96a7e751928`。

---

## 8. 主なリスクと最終確認

1. **PTYのforeground race**  
   targetのexec前にPGID／foreground設定を確定する。TTYの見かけだけで合格にしない。

2. **偽陽性の死亡試験**  
   EOF、bwrap、通常cleanupで子が死ぬとguard欠落を見逃す。DirectSandboxとguard-removal mutationを必須にする。

3. **Chromiumの実効設定差**  
   upstream private defaultの追従漏れ、重複flag、stdio変更をgoldenで検出する。依存更新時も再検証する。

4. **PTY pin locatorの既知制約**  
   procアクセス・PID namespace要件と、承認済みのPID再利用残余リスクを隠さない。B3で別方式へ無断変更しない。

5. **テストだけのraw入口が残ること**  
   `retired()`とFD hookを分けて整理する。publicな偽receipt生成APIを新設しない。

6. **古い基準と実装の差**  
   例としてDB27原文と現在READMEの`--describe-handoff` bind記述に差がある。B2の承認差分を照合し、B3で起動配置を巻き戻さない。

7. **試験環境不足**  
   CEF等が実行できなければ「未実施」として残す。fake rendererやskipだけで実CEF要件を閉じない。

最終提出物は、**PR一覧、spawn対応表、実artifact識別子、性能の条件と生データ、対象外一覧**です。mutationを行った場合のみ、その対象と結果を記録します。独立Reviewerと親がB3／D-7を確認し、5a-VのPR Aとは独立に扱います。
---

## 親の判断

計画レビュー（`d3a2-B3-plan-review.md`。承認、must なし）を受けた親の判断です。§7 の A〜D に対応します。
着手前作業の成果は `d3a2-B3-prework.md`（契約・対象表、spawn 台帳）と
`d3a2-B-design-v3-excerpt.md`（設計 v3 の §6.1、§8.2 などの復元）にあります。

### A. expiry に使う artifact：**決定**

- B3-07 では、製品と同じ部品で組んだ **release 版の試験 binary** で確認します。不可逆 capture 後に実時間 10 秒を経過させ、exit 74 と所有する子の回収を確認します。
- これは製品 ELF での expiry 確認ではありません。製品 ELF でも同じ確認を、5a-Vの**PR A（実restart）**で行い、検証完了の条件に加えます。
- 試験の時だけ管理機能を公開する特別な build（案 3）は採用しません。
- 製品フックの追加や時計の短縮で証拠を代替しません（計画 §1、C-6）。
- 出典：ユーザーの回答 `question-fa99dc79f403e3adadcd0e946dd5c838`。

### B. 11 root と実 caller の対応：推奨案を採る

- **API 網羅表と実 caller 表を分けます**（`d3a2-B3-prework.md` の表 2 と表 3）。13 の合成経路を、実 core・実 caller の
  証明として数えません。11 root の定義は、8 種の `ChildKind`、StdioSession、PTY、js-worker の 11 個です。
  実 Chrome と CEF は別の追加セルです（レビュー S1）。
- **spawn 台帳は、親が承認します。** 台帳の各行に「artifact 種別」と「到達する入口」を書きます（レビュー S2）。
  製品 ELF で既存の入口から起動できない実 caller は、expiry と同じ扱いで、親に返します。
- **B3-06 は、spawn 台帳の承認が済むまで着手しません。**
- caller のない `Probe` に、架空の製品経路を作りません。11 root との対応と欠落は、親が台帳の承認の時に決めます。

### C. 監査対象外の std spawn：推奨案を採る

- **既存の core の ownership 境界を保ちます。** GUI の短命な git、`fc-match`、GUI の埋め込み terminal の PTY などは、
  台帳に載せ（`d3a2-B3-prework.md` の 6-C）、**別の follow-up** とします。
- ただし、新たに見つかった **core 所有の長寿命の spawn** は、理由なく除外しません。
- 全アプリの spawn の統一を求める場合は、9 本の PR に押し込まず、追加の PR を設けます。

### D. 測定環境・期限：推奨案を採る

- **測定方針（計画 §5）は、出典 `question-41eda9b4579b24b09e86a96a7e751928` に基づく縮小条件に更新します。** ForceKill／PSSの既存閾値は維持します。
- 測定条件の細部（A/B の source SHA と無関係差分の扱い、
  kernel・Chromium・CEF・bwrap・display の環境、SIGKILL 後の観測期限、10 秒満了の観測許容幅、CEF 各段階の timeout、
  追加の起動時間 gate の要否）は、測定の前に親が固定します（§7D）。
