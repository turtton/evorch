# Q9 再測定の事前固定計画

レビュー修正後、計測開始前に固定する。既存の受け入れ条件は変更しない。

- before: main `1d2cc55c` の archived source、after: 今回の最終 Rust コード。
  実装変更を追加した場合は after を再ビルドし、全20ケースを測定し直す。
- 既存 `before.rs` / `after.rs` を使用する ownership マイクロベンチ。
  after は合成 receipt（events DB の snapshot 書込み・hash/redaction は含めない）。
- 5,000,000 B、N=M=20、各15秒、writer 完了後0 ms / 100 ms の各5反復。
  reader は guard 取得・即解放後1 ms休止。追加 guard 保持0 µs。
- 全20ケースを seed=3703 で順序を混ぜ、1プロセスずつ直列実行。
  自分の検証/ビルド完了後に実行し、開始前と各ケース直前に process list、loadavg を記録。
  他の cargo/rustc/nextest/ベンチを発見したら開始しない（共有ホストの他サービスは停止しない）。
- 新規操作受付15秒後に停止し、in-flight 完了を待つ。SQLite DELETE/FULL、
  本番の deadline/attempt timeout・generation guard は変更しない。
- 各反復の writer/reader 失敗件数・総操作数・失敗率、成功 throughput
  （成功数 / elapsed_seconds）、reader p50/p99 を記録。
  既存 Rust 計測の elapsed_seconds は barrier前の起動時間も含むため、その値を分母に使用し
  比較対象双方で同じ定義にする（warmup は含まない）。
- p50/p99 は反復内の成功/失敗を含む全 reader 操作 latency の既存 quantile 定義を使用。
  合成 p99 を反復p99の平均で代用しない。
- 主要比較は同一 pause・version の5反復を合算した失敗数/総数。
  reader は after の合算失敗率 ≤ before、after writer 最終失敗率 ≤2% を要求。
  各反復と同じ rep の対応比較も公開し、合算で隠れる悪化やばらつきを説明する。
- 稀な失敗の不確実性として合算および反復の失敗件数を示し、0件なら
  二項独立仮定の片側95%上限 `1 - 0.05^(1/n)` を示す。
  競合失敗の相関があるため真の独立試行・将来率の保証とはしない。
- quick_check=ok、other_errors空を検証。失敗ケースを恣意的に除外しない。
- 未達なら条件を変えず、writer 到着増による reader starvation 等の仮説と
  fairness / writer rate 制御等の別途評価案を報告する。
