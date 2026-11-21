# 5a-V 引き継ぎ事項（対象PR本文の調査）

指定された16件について `gh pr view <番号> --json title,body,mergeCommit` で本文を確認し、V/5a-V に関係する配線・残件を抽出した。引用はPR本文からの短い抜粋。後続PRの本文で実施済みと分かるものは、その旨を併記した。5a-Vとは別領域の残件も、指定PR本文に明記されているため区別して記載する。

## PRごとの記述

- **#719 — public restart 共通契約**
  - 本番leaf facadeは未登録でUnavailableのまま。「`Production leaf facades stay unregistered/Unavailable`」。V bootstrap側の本番登録・有効化が必要。

- **#723 — Q1 quarantine inspection/discard**
  - Q1 facadeの本番設置。「`The facade is not installed`」「`constructing QuarantineFacade + StorageQuarantineStore and AdminService::install_quarantine belongs to the 5a-V bootstrap (central)`」。
  - facade接続後のdead-code属性除去。「`The leaf modules carry #[allow(dead_code)] for that reason; 5a-V removes them.`」
  - cold graphの処分は未完了になり得る。「`stays partial ... until Q2's disposal records every thread through the production progress write`」。Q2の本番progress writeが各threadの処分記録を残す必要がある。

- **#724 — 接続認証 seam**
  - SafeRestartの公開は対象外。「`SafeRestart の公開 ... は含めません`」。V側でのcapability公開が残る（#731/#732にも明記）。

- **#725 — Q2 integration seam**
  - facadeと共通continuation/auditの接続、scratch L-5/L-6 E2E、leafのfault/race試験は後続扱い。「`Q2 の公開 facade・共通 continuation/audit 接続、scratch L-5/L-6 E2E、leaf の fault/race 試験は後続です。`」後続の#734はQ2 leafの物理E2Eを実施したが、Vの本番RPC/capability配線は#734本文でも残件。
  - `expect(dead_code)` は接続後に整理する前提。「`leaf 接続後の整理は後続 integration として管理します。`」

- **#726 — 5a-CN seam**
  - 本文に5a-V向けの具体的な残件はなし。CN leafがこのseamのmergeに依存する旨のみ。「`5a-CN の leaf PR がこの PR の merge に依存します。`」

- **#727 — Web管理UI/SSE**
  - Web管理は本番無効。Web認証が未実装のため、UI操作とmanagement streamを開始しない。「`Web の管理操作は本番では無効のまま（Web 認証は未実装）`」。認証済みWebの正例もfake principalでの契約確認のみ。
  - UIの未接続・DTO待ち。「`shell Interrupt の UI は ... DTO 拡張待ち`」および「`quarantine の discard は ... 現状の DTO では押せない`」。

- **#728 — 5b-D continuity storage**
  - 5a-V向けの直接の配線依頼はなし。別領域の未確認事項として、5b-Rと未接続のためcontinuity E2E未検証。「`5b-R ... と接続していないため、end-to-end の continuity は未検証。`」Vの作業と混同しないこと。

- **#729 — Q2 manual readback seams**
  - dead-code属性の暫定変更がある。「`expect(dead_code) → allow(dead_code)`」。Q2のpublic facade、disposal/Resume接続、E2Eは別担当の後続。「`leaf の公開 facade・処分/Resume 接続・E2E は別担当の後続作業です。`」

- **#730 — R shell seam**
  - 本番restart facade、shared dispatch、Q2 recovery_disposal、SafeRestart製品注入は未実施。「`R の公開 facade、共有 dispatch、Q2 recovery_disposal、SafeRestart の製品注入は含めません。`」本番execとの結合も後続R/V担当。

- **#731 — R public restart adapter**
  - 必須portの本番注入がV/Q2に残る。
    - `UndisposedAdmission`（Q2/V）: live collector lock内でsnapshot revision・所有/未処分集合を照合し、lockを保持してgate transitionする。
    - `FinalizationOwner`（V）: BootstrappedHost/exec integrationがPreparing transactionの作成/readbackと既存start_finalizationに接続する。
    - 引用:「`本番の実配線は5a-VとQ2 leafに残ります。SafeRestartはこのPRでは公開しません。`」
  - 製品exec、port注入、native公開、公開wire cache E2Eは未実施。「`製品exec、5a-V/Q2の本番port注入、native公開、公開wire cache E2Eは本PRでは未実施`」。公開cacheシナリオはVへ引き継ぎ。

- **#732 — native reconnect、管理CLI、GUI panel**
  - CLI routeをVで接続。「`crates/evorch/src/cli/mod.rs に core の route を足し、core_admin::main(argv) に渡します。`」
  - GUI actorでRestartPanelをfeaturesに接続し、planned restartの証拠に応じたreconnect、再接続後の`on_new_session()`とread-only `reconcile_queries()`を配線する。
  - SafeRestartはVだけが広告する。「`SafeRestart を広告するのは V だけです。`」未広告coreにはCLIから何も送らずexit 1。
  - Q1 discard/Q2 owner release・resumeのUIはDTO未確定で対象外。「`Q1 と Q2 ... の UI は、この PR に含みません。DTO が確定していないためです。`」
  - 実core結合、GUI actor/windowへの配線・描画は未確認/未実施。「`実際の core（製品）との結合は、V と F4-E で行います。`」「`ウィンドウへの配線と描画は V の担当`」。GUIはmodel単体試験のみ、CEF/Chromium未使用。

- **#733 — physical recovery fixture**
  - test-only fixture準備のみで、Q2 leaf E2Eは含まない。「`leafのE2EはこのPRには含めません。`」後続#734ではleafの物理E2Eを実施済み。Vの本番接続とは別。

- **#734 — Q2 owned graph resume/disposal**
  - Q2 leafは実装済みだが、製品接続はV。「`ホストRPCからの実際のcapability公開とmanual Resume全経路、独立wire-prefix/cache E2Eの製品結線は5a-Vで実施。`」
  - 属性はこのleafでは除去していない。「`expect / allow 属性削除なし`」。

- **#736 — Q2 central seam補完**
  - Vへの新規具体配線依頼はなし。属性・cache契約にも変更なし。「`属性expect/allow、cache契約は変更しません。`」leaf側は#734、製品capability/RPC/cache接続は#734のV引き継ぎとして扱う。

- **#720 — process-guard B1**
  - 5a-Vとは別件。caller移行なしで、guardの保護外にある起動経路が残る。「`呼び出し側は移行しません`」「`今の全ての起動経路は、guard の保護の外`」。B2（PTY以外の移行）・B3（PTY/Chromium/結合）は別PR。
  - 未確認: Nix `checks.tests`の失敗/timeoutとmain比較・再実行、CI待ち時間、aarch64-darwin build、kernel 5.11実機、PTYの実exec/portable-pty結合、複数箇所のmutation未実施など。「`checks.tests ... main の checks.tests との比較をしていない`」「`実 portable-pty との結合は B3`」。これらはprocess-guard側のfollow-upでありV項目ではない。

- **#715 — process-guard B1**
  - 本文もB1の範囲を「`呼び出し側は移行しません`」とし、B2/B3を別PRとしている。PR本文に5a-V向けの直接の引継ぎはない。
  - B1の未確認/後続（Nix test derivation、Darwin/kernel実機、PTY/B3結合等）は#720と同じprocess-guard領域。5a-Vの残件には算入しない。

## 性質別の分類

件数は以下の各分類に並べた「残件の箇条書き」数。後続PRで実施済みと確認できるものは、V残件として重複計上していない。

### 本番 install と配線 — 9件
1. **Q1 facadeのbootstrap install**（#719/#723）: leaf facadeをUnavailableのままにせず、`QuarantineFacade` + `StorageQuarantineStore` を `AdminService::install_quarantine` で登録する。
2. **SafeRestart capability**（#724/#731/#732/#734）: Vのcoreだけで広告し、未広告coreにはCLIから命令しない。
3. **Q2 public facade/continuation/audit接続**（#725/#729/#734）: leafの公開機能をhost側serviceに接続する。
4. **UndisposedAdmission port注入**（#731）: live collectorのatomic照合とgate transitionを実配線する。
5. **FinalizationOwner port注入**（#731）: BootstrappedHost/execのtransaction作成/readbackと既存finalizationへ接続する。
6. **R製品接続**（#730/#731）: public facade/shared dispatch、Q2 recovery_disposal、SafeRestartの製品注入と製品execを接続する。
7. **CLI route**（#732）: `cli/mod.rs`から`core_admin::main(argv)`を呼ぶ。
8. **GUI restart配線**（#732）: actorにpanel features、planned-restart reconnect、new-session後のread-only reconciliationを接続する。
9. **Web管理の本番有効化条件**（#727）: 実Web認証がない間は無効のまま。実認証導入後にのみ認証済み操作を接続する。

### 属性の除去 — 2件
1. **Q1 leafの`#[allow(dead_code)]`除去**（#723）: facade install後に不要な属性を除去する。
2. **Q2/runtime seamの`expect/allow(dead_code)`整理**（#725/#729/#734）: 接続を理由に保持/置換した属性を、配線後に見直して除去する。#729では指定2箇所を`expect`から`allow`へ変更し、#734時点でも削除なし。

### 試験と計測 — 5件
1. **公開wire cache E2E**（#731/#734）: provider/history/tools/schema/affinity prefixを独立に検証する製品結線E2EをVで実施。
2. **実core/製品exec結合**（#730/#731/#732）: fake coreや契約試験に留めず、製品exec・native reconnectとの結合を確認。
3. **Web認証の正例**（#727）: fake principalだけでなく実Web verifierを通る認証済み操作を検証。
4. **連続性E2E**（#728）: 5b-D本文上の残件。5b-R担当であり5a-V件数には含めない。
5. **process-guard B1/B2/B3の未確認項目**（#720/#715）: Nix test derivation比較/再実行、CI待ち時間、Darwin/kernel実機、PTY/portable-pty結合など。process-guard側の残件であり5a-V件数には含めない。

### その他 — 4件
1. **cold quarantineの完了条件**（#723）: cold graphのowners/scratch処分記録が揃うまでpartial/Failedを維持し、Q2のproduction progress writeで全threadを記録する。
2. **Q1/Q2 UI**（#727/#732）: discard/owner release/resume UIはDTO確定待ち。既存本文では未搭載。
3. **Q2物理leaf E2E**（#725/#733）: 当時のfollow-upだったが、後続#734で物理E2Eを実施済み。Vの製品結線E2Eとは別。
4. **process-guard caller migration**（#720/#715）: B2/B3の別PR。全起動経路がguard外という記載は5a-VではなくD-3a2側で追跡。
