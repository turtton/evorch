# ローカルコアの発見・起動調停（ADR 0045 / P10a）

## 境界

- `core-instance`: config identity、private socket path、endpoint record、ロック。runtime / storage / GUI / protocol に依存しない。
- `core-launcher`: `workbench-client` のhandshakeとプロセス起動を合成する。GUI / CLIにはまだ組み込まない。
- ローカルFS・Linux対象。継承FDのロック所有検証に Linux `/proc/self/fdinfo` を使う。ネットワークFSの排他は保証しない。

## APIと順序

1. `ConfigIdentity::new` でconfigをcanonicalizeする。
2. `CoreBinary::sibling` または `CoreBinary::explicit` で起動対象を検証する。PATH / CWD探索はしない。
3. `Launcher::new(...).connect_or_start().await` が、保存endpointと標準endpointに接続し、config identity・version・必要capability・epoch・Ready状態を検証する。
4. 接続不能・hello前のtransport切断・保存epoch不一致の場合は `.evorch-core.launch` を取得し、recordを最新から読み直して再接続する。identity / version / capability拒否や不正なwire応答は恒久エラーとして返す。launch保持中のみlifetimeをprobeし、切断した旧ownerが後から解放する場合も再probeする。probe FDはspawn前にcloseする。
5. 親はlaunch FDをCLOEXECのまま保持し、fork後の子だけCLOEXECを解除する。子は `--launch-fd` をentryで採用し、CLOEXECを戻す。同じファイルをopen/flockし直さない。
6. 子は `Instance::acquire(&launch, ...)` でlifetime取得後、検証済みstale socketを掃除する。listenerをbindし、handshakeと同じepochのrecordをpublishしてlaunchをcloseする。親も実際のreadiness検証後にcloseする。LOCK_UNやlock file unlinkは禁止。
7. exit 2は再spawnせず既存コアへの再接続へ移る。

endpoint recordはconfig内のatomicな発見ヒントであり生存証明ではない。PIDを記録・参照しない。異なるXDG_RUNTIME_DIRの旧socketもlifetime取得後にのみ掃除する。廃止した `.evorch-core.start` には触れない。

既定30秒のdeadlineは診断だけで、子のkillやlock強奪はしない。起動役が死んでも、spawn前ならlaunch解放後に別クライアントが選出され、spawn後なら子が同一OFDを保持する。子は親のstdout pipeを使わないため親の死によるSIGPIPEにも依存しない。

## 後続PR

- **07**: planned exec / adopt_handoffは別経路。lifetime FD移譲APIはまだ公開しない。cold launchの採用と混同しない。
- **08 / 17**: GUI / CLIはこのLauncherを使い、embedded runtimeへのfallbackを作らない。client configは再接続時も同じrequest counterを再利用する。
- **18b**: config単位の永続restart budgetを起動決定箇所へ追加する。現在は単一cold-start試行だけでクラッシュ再試行をしない。AlreadyRunningは予算消費対象にしない（`LaunchStage::AlreadyRunning`）。単なるpollingや接続を試行回数として数えない。

## 検証

`harness/main.rs` はstorageなしの実プロセスハーネス。control socketのackを各同期点とし、kill後はpidfdの終端イベントで待つ。テスト内のsleepやwall-clock完了判定はない。ハング診断は仮想時間で検証する。実evorch-coreにも継承launch起動の回帰テストがある。
