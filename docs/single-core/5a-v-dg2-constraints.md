# DG-2 改訂の再レビュー

## 判定：承認

M1 と M2 は解消しています。新しい must はありません。V1b-1 と V1b-2b に着手できます。

読み取りだけで確認しました。ビルドと試験は実行していません。改訂で足された file:line は、main `794fa1ed` で照合して合っていました。

## M1 と M2 の確認

| 指摘 | 状況 | 確認したこと |
|---|---|---|
| M1 | 解消 | §3.2 の手順 6 が、start の受理直後に `AwaitRestartRecovery` を返す形になりました。完了監視の task も作りません。loop のスコープの guard が、全退出経路で receiver と reply sender を破棄します。§6 に試験が 2 行入りました。 |
| M2 | 解消 | §4.3 の確定表が、型を定義する PR、中央へ入れる PR、起動順の位置を持っています。collector の接続点は自動 recovery の前で、コード上も置けます。DG-1 §6、DG-3 §6、計画 §4 は同じ決定に揃っています。 |

M2 には条件が 1 つあります。V0-switch の branch は今も `686fcc75` のままで、`StorageConfig` と `StorageHandle` の accessor はまだ入っていません。入るまで V0-switch を merge しないでください（下の G2-11）。

中央ファイルの編集者の変更は、V-11 の「同時に 1 PR」を保っています。編集窓が V0-switch、V-admission、V1b-1 の順で直列だからです。編集者の一覧だけが、親の決定で更新された扱いです。

## DG-2 の制約（V1b-1、V1b-2b）

| ID | 制約 | 根拠 | 検証 |
|---|---|---|---|
| G2-01 | finalization を始める製品の呼び出しは、host を排他的に借りた loop だけ。`FinalizationOwner` は host を持たず、tx の作成・readback と配送だけを行う。capture・時計・catalog 比較を再実装しない。 | V-08、§3.1、§3.2、DC-32 | 型の試験（Shared に receiver を渡せない、sender に host が無い） |
| G2-02 | `LifecycleDriver::new(&host)` の署名と、既存の `run`／`run_with_web` を変えない。unsafe、`DerefMut` の偽装、panic の代替を作らない。署名の変更が要るなら PR を分けて親へ戻す。 | §3.1、計画 V1b-1 | diff。19 の呼び出しが不変 |
| G2-03 | loop は DB、ELF の hash と probe、writer や finalizer の待機をしない。start は同期の転送だけで、すぐ select に戻る。 | V-08、§5 の 2 | DB の worker を barrier で止めても、signal と drain を処理できる試験 |
| G2-04 | tx の Create は最大 1 回、start は restart ID ごとに最大 1 回。ACK 不明は同じ ID の readback だけ。失敗や不明で、再 Create・再配送・別経路の start をしない。 | V-08、§3.2、§3.3 | 二重依頼、不明 ACK、reply 消失の試験 |
| G2-05 | start が受理されたら、continuation は直ちに `AwaitRestartRecovery` を返す。`wait_finalization` と後続 stage を待たない。 | §3.2 の手順 6、§5 の 9 | stage 4 を既存の 1 秒以内に通る試験。期限は変えない |
| G2-06 | receiver は loop のスコープの guard が 1 つだけ持つ。signal、accept の失敗、drain 中の signal、早期 return、unwind のどれでも、receiver を閉じて未処理の依頼と reply sender を破棄する。Drop は memory の操作だけ。 | §3.2「全終了経路での解放」 | 3 経路で `host.shutdown()` が完了する試験 |
| G2-07 | 線形化点は既存の `capture_boundary`。独自の capture flag を作らない。signal を観測済みなら start は 0。capture が先なら転送を中断しない。mailbox 以外の分岐の意味を変えない。 | §3.4 | 同時 ready と、capture 直後の signal の試験 |
| G2-08 | `host.shutdown()` の中の順序、終端の `transition_to_shutdown`、backend・host・instance の drop 順を変えない。capture 後の task を abort しない。 | V-08、§5 の 3〜5 | shutdown ordering と watchdog の回帰 |
| G2-09 | instance、signal mask、signal reader、control drain は、起動時に得た同じ所有物を共有する。第二の Instance、lifetime lock、signalfd を作らない。argv は保存した型付きの起動オプションから組む。`--launch-fd` や生の argv を写さない。 | §4.1、V-10 | 両入口の試験、argv の試験 |
| G2-10 | `RestartMetadata` は §4.2 の情報源で 1 回だけ固定する。試験用の固定値を製品に写さない。`old_epoch` は、control drain の検査が比べる hello の現在値にする（`finalization/later.rs:196-201`、`lifecycle/control.rs:30-37`）。 | §4.2、既存コードの契約 | 異 metadata と full builtin の試験 |
| G2-11 | 中央入力は §4.3 の表を正とする。編集窓は V0-switch、V-admission、V1b-1 の順で直列。V0-switch は 2 つの accessor なしで merge しない。未定義の型を先に参照しない。表に無い追加は親へ戻す。試験専用の `composition.storage_writer` に依存しない。 | §4.3、計画 §4、V-11 | 各 PR の diff |
| G2-12 | collector の構築と hook の接続は bootstrap の中で、`recovery.start`（`bootstrap/mod.rs:807-825`）より前。入口の install で初めてつながない。 | §4.3 | successor の起動順の試験 |
| G2-13 | 既存の期限を変えない。非 Linux は gate と tx の前に Unsupported。off と未認証は mailbox と tx に届かない。製品に試験入口を足さない。 | V-19、V-03、V-12、V-18 | 負例、diff |

G2-10 の `old_epoch` は、§4.2 の「hello／host epoch」を既存コードの契約で一意にしたものです。

## DG-1 の制約の更新

前回の G1-10 は、今回の決定と合わなくなったので置き換えます。

| ID | 制約 | 根拠 | 検証 |
|---|---|---|---|
| G1-10（改） | V-admission の後に V1a-2。空の collector の暫定 constructor を作らない。V-admission が composition の根と `bootstrap/mod.rs` を編集できるのは、collector の入力・accessor・構築・hook の接続だけ。時期は V0-switch の merge 後、V1b-1 より前。 | DG-1 §6、DG-2 §4.3、計画 §4 | diff |

## should（follow-up）

- **V-admission の scope**：`bootstrap/mod.rs` は中央ファイルです（`scripts/central-files.txt:36`）。V-admission は integration の scope で出さないと、`check-central-files.sh` が落ちます。規模も計画の「約 12」を超える見込みです。
- **leaf の登録と根**：V0-switch の根は、登録を各 leaf の PR に任せています（`admin_composition/recovery.rs` の install は空）。計画 §4 の「V1a-1・V1a-2・V1b-2a は中央を編集しない」と合いません。merge 前に stub を置くか、根への `mod` 宣言の追加を leaf に許すかを決めてください。
- **管理用 epoch の取得口**：確定表にありません。`AdminService` の epoch は非公開です（`admin/service.rs:91`）。facade と coordinator の構築に要ります。projection の現在値で代用すると、invalidation の後にずれます。V0-switch の追加に含めるのが安く済みます。
- **計画 §3.2 と §3.3**：「前提」の列と依存の図が未更新です。新しい依存は §4 の文章にしかありません。
- **Shared の loop**：receiver を取り出さない実行では、依頼が待ち続けます。試験 helper は owned の loop を使うか、Shared 側で receiver を捨ててください。
- **off の間の hook**：V-admission の merge 後は、公開前の全 core で、自動 recovery が collector の hook を通ります。collector の不具合で自動 recovery の結果が変わらない規則を、V-admission で決めてください。たとえば、hook が effect を断るのは restart の予約があるときだけにする、という規則です。
- **前回の should で未反映のもの**：
  - signal 優先の実現方法
  - successor epoch の `u64::MAX`
  - capture 前の失敗を cancel する主体
  - `Arc<Instance>` の drop 順
