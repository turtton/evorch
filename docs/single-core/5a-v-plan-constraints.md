# 5a-V 分割計画（改訂 2）の再レビュー

## 判定：承認

M1〜M7 は 7 件とも解消しています。この改訂で新しく入った must はありません。下の follow-up は実装を止めません。

改訂版の全文を読み、スイッチの位置と列車の CI をコードで確かめました。試験とビルドは実行していません。

## M1〜M7 の解消状況

| 指摘 | 状況 | 確認したこと |
|---|---|---|
| M1 | 解消 | 管理の要求と購読は、すべて `authorize_management` から `AdminService` に入ります（`backend.rs:284-300,305-353`、Web は `api.rs:134`、`sse.rs:307`）。先頭で拒否すれば抜け道はありません。 |
| M2 | 解消 | V-admission に切り出され、DG-1 の独立レビューが前提になっています。 |
| M3 | 解消 | DG-2 と DG-3 が入りました。DG-3 の受け入れ条件は V1b-2a の完了条件に入っています。 |
| M4 | 解消 | 往復の試験が V-ops-q2 と T3 に、拒否理由の表示が CLI と Desktop に入りました。「導線を含めない」案は外れています。 |
| M5 | 解消 | V-cache のシナリオ 6 と、列車の merge 条件 9 に入っています。 |
| M6 | 解消 | `current` target、`Restarting` から Ready までと 120 秒の比較、対応表の 3 つが入っています。 |
| M7 | 解消 | 編集者が 1 本に決まり、B2 (a)／(b) が反映されています。 |

公開列車の PR は、base が main でなくても CI が走ります（`ci.yml:4-7` は `pull_request` に絞り込みがありません）。

## 合意した設計上の制約

| ID | 範囲 | 制約 | 根拠 | 検証 |
|---|---|---|---|---|
| V-01 | `AdminService` | 管理操作は `enabled`（既定 off）で fail-closed。off では `authorize*`／`submit`／`query` の先頭で拒否し、facade・監査・gate／tx／owner に触れない。検証済み local でも拒否する。 | M1、親の判断、§2.2 | V0-switch の単体試験（9 操作、facade と監査の呼び出し 0） |
| V-02 | `capability.rs` | 製品で有効にするのは `MANAGEMENT_PUBLISHED` の 1 か所だけ。`true` にするのは T1 だけ。環境変数と設定の key は作らない。試験は in-process の host で `enable_management()` を呼ぶ。 | §2.2、DC-20 | `check-admin-publish-boundary.sh`（CI） |
| V-03 | `capability.rs`、`backend.rs` | 有効化と広告は、Linux で 3 facade と coordinator が全部登録されたときだけ。広告は検証済み local だけに出す。`Capability::IMPLEMENTED` は変えない。非 Linux は gate／tx の前に Unsupported。 | DC-31、§6 の 1・2 | T2（cold と successor の hello、local と Web の差）、非 Linux のビルド |
| V-04 | 公開列車 | T1〜T4 は `integration/5a-v-publish` に積み、1 回で main へ merge する。各 PR の個別レビューと、統合レビュー 1 回を通す。 | J1 | merge 履歴、レビュー記録 |
| V-05 | 公開の前提 | 列車は、公開前の全 PR と B2 (a)・(b)、B3 の merge 後に始める。 | J8、全体計画 §1 | T1 の PR 本文の表 |
| V-06 | 設計メモ | DG-1〜DG-3 が独立レビュー（実装者と Advisor とは別 context）を通るまで、V-admission、V1b-1、V1b-2a、V1b-2b に着手しない。 | M2、M3、J3 | レビュー記録 |
| V-07 | V-admission | `admit` は live collector の lock を保持したまま revision と空集合を照合し、I/O なしで gate を遷移する。読んで比較する実装は不可。`SnapshotClock` は使わない。 | `restart/public/implementation.rs:36-40`、M2 | 競合試験、`check-q2-boundaries.sh` |
| V-08 | V1b-1、V1b-2b | finalization は host を所有する loop だけが実行する。二重実行と再実行をしない。loop で DB の I/O をしない。`host.shutdown()` の順序を変えない。`FinalizationOwner` は tx の作成・readback と既存 `start_finalization` の呼び出しだけを行う。 | DG-2、DC-32 | 所有と依頼の試験、故障試験、既存試験の回帰 |
| V-09 | V1b-2a | core 所有でない run を `CoreOwned` にしない。権限を新設しない。lane・team・goal の run は、再開するか gate の前に拒否されるかを試験で固定する。 | DC-35、M3 | `public_restart_compose::recipes` |
| V-10 | 全 PR | target は `current` かローカル設定の候補 ID だけ。wire・UI・CLI から path・argv・env を受けず、出力にも path を出さない。 | DC-30 | V1b-2a と V-ops の試験 |
| V-11 | 中央ファイル | 同じファイルの編集者は同時に 1 PR。`main.rs`、`planned_entry.rs`、`lifecycle*.rs` は B2 (a) の merge 後の V1b-1 だけ。composition の根と `bootstrap/mod.rs` は V0-switch、追記は V1b-1 だけ。登録は V0-reg だけ。 | M7、J2、§4 | 各 PR の diff |
| V-12 | install の PR | V1a-1、V1a-2、V1b-2b は、off で拒否される負例と、on でも未認証 Web と Agent の変更系が拒否される負例を持つ。 | M1、§2.3 | 各 PR の試験 |
| V-13 | CLI、GUI | 広告が無い core に、CLI の変更系は何も送らず exit 1。GUI は panel を出さない。release は resume を起こさない。 | DC-34、§2.3 | V-ops-q1／q2、V-gui の試験 |
| V-14 | 解除・再開の導線 | 未処分で拒否されたとき、CLI と Desktop は件数と次の操作を示す。「拒否 → 一覧 → release／resume → 通る」を V-ops-q2 と T3 で通す。 | 元の依頼、M4 | V-ops-q2、V-gui、T3 |
| V-15 | V-cache | 入力由来の mock cache と独立した wire 比較を使う。異 build で schema が変わる行と、明示 resume の行は必須。履歴を書き換えない。製品同士の証拠とは呼ばない。 | AGENTS.md の cache gate、全体計画 §7、M5 | `public_restart_cache`、`check-cache-contracts.sh` |
| V-16 | V-deny | 未認証の実 HTTP で 9 操作と管理 SSE を拒否し、状態は不変。Agent／System は host 入口で拒否。sandbox の負例は失敗理由を固定する。bwrap が動かなければ失敗させる。 | 全体計画 §3、DC-62 | `public_restart_deny`、列車の CI で再実行 |
| V-17 | V-safe | 確認・照合・監査を必須にする。監査の失敗で副作用 0、cancel だけ成立。I-31〜I-34 が本番の組み立てで成り立つ。 | 全体計画 §2.3、I-31〜I-34 | `public_restart_safe` |
| V-18 | 試験全般 | 製品の main／bootstrap に試験入口を足さない。F4-P の fixture を使い、拡張は T2 だけ。同期は clock／barrier／channel／join で、sleep と retry を増やさない。 | DC-20、共通 DC | diff、レビュー |
| V-19 | 期限 | probe 5 秒、300 秒、120 秒、30 秒、lease 5 秒＋grace 10 秒、stage 5 の 10 秒を変えない。 | DC-38 | `conditions.md` の基点 commit からの `git diff` |
| V-20 | 性能 gate | 条件は最初の計測の前に commit する。実測は T4。hard 条件は、失敗 sample 0、再試行と延長なし、各区間の最大値が期限未満。失敗時は addendum を先に commit する。soft は報告だけ。 | 全体計画の性能 gate、M6、J5 | `measurements/public-restart/` |
| V-21 | Web | Web 管理は有効にしない。`security.rs` と認証検証器に触れない。認証済み Web の正例は未検証と開示する。 | 全体計画 §1 | diff、T1 の PR 本文 |
| V-22 | 巻き戻し | スイッチを `false` に戻すか、列車の merge commit を revert する。runtime の kill switch は作らない。 | J7、§2.5 | off の build で拒否される試験 |
| V-23 | native client | EOF だけで planned と見なさない。新 epoch で旧 cursor を捨てる。command・data bind・token を再送しない。CLI は core を起動せず exit 1、Ctrl-C は 130。 | DC-36、DC-37 | V-gui の actor 試験、T3 |
| V-24 | B2 (a) の後 | 製品 core と `evorch` CLI を起動する試験は `guard-cargo.sh` 経由にする。 | M7、§7 | CI の job 定義 |

## follow-up（should）

- **V-cache のシナリオ 2**：same-build の行は公開前には作れません。同じ image が両側に要り、製品は off です。列車か F4-E に置くと明記してください。
- **V-exec の「未処分による拒否」**：successor の製品が off なので、別の理由で拒否されます。composed 側で確かめるか、V-safe と T3 に任せてください。
- **V-perf の計測関数**：T2 だけが触れる fixture の拡張に依存します。関数は列車に移し、V-perf は条件と手順だけにするのが整合します。T4 は「データのみ」にならない可能性があります。
- **V-ops-q2 の「CLI × 実 core」の置き場所**：`evorch` と `evorch-core` は互いに依存しません。両方を link できるのは `core-client-e2e` だけなので、V0-reg の登録に target を足してください。
- **V-admission と V1a-2 の順序**：`OwnedGraphsFacade::new` の署名を変えるなら、DG-1 で順序を決めてください。
- **T3 の未処分 graph**：製品 artifact で test hook なしに作る方法を、列車の前に確定してください。作れないときの代替も決めておきます。
- **対応表の「capture → exec」**：期限が 4 つ並んでいます。合否に使う 1 つを `conditions.md` で決めてください。
- **巻き戻し**：1 行で戻すと T2・T3 の試験が赤になります。試験をスイッチに追従させるか、手順を merge commit の revert に絞ってください。
- **境界検査の ②**：「`true` にするのは T1 だけ」は機械的に検査できません。期待値を script に持たせてください。
- **公開済み build から off の build への restart**：successor では解除・再開ができません。扱いを DG-2 か T1 で 1 行決めてください。
- **V1b-2b の大きさ**：helper の `composed_core.rs` 込みで重いです。超えそうなら helper を別 PR にしてください。

## 未解決の確認事項

- B2 の (a)／(b) の境界は、計画書も書いているとおり未 commit の差分からの推測です。B2 の worker に確認してから、§4 と §7 を合わせてください。
