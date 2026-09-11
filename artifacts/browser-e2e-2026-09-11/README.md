# Browser 実 Chromium/CDP E2E 実測

- 実測日: 2026-09-11
- worktree: `/home/turtton/.ghr/github.com/turtton/evorch` (`main`)
- Chromium: `152.0.7977.75`、headless、独立した一時プロファイル
- 接続先: テストが起動する `127.0.0.1` の動的ポートの HTTP fixture
- 本体コードの修正: 不要。既存 ignored テストを拡充し、ignore は維持。

## 実行結果

1. 既存の最小 E2E: 1 passed / 0 failed、実行 1.63 秒。
2. 証跡保存・click 検証を追加した同じ E2E: 1 passed / 0 failed、実行 1.34 秒、ビルド 20.85 秒。
3. 変更した Rust ファイルの LSP diagnostics: なし。rustfmt と `git diff --check` 成功。
4. workspace テストは最後に一度だけ実行。`crates/arena/tests/variants.rs:52` の E0599（`ArenaReport::promote_config` が存在しない）によりコンパイル停止。実行中に追加された別作業の arena/storage 変更は修正していない。workspace 全体の成功は未確認。

## 証跡

- `navigate-before.png` / `navigate-after.png`: navigate 前後の実 CDP JPEG をデコードして PNG 保存。
- `click-before.png` / `click-after.png`: button が `Change` → `Changed` になることを目視確認。
- `navigate.json` / `click.json`: 各 action の started/completed、前後 screenshot（元 JPEG の base64 を含む）、DOM diff の診断イベント。
- 各 action のイベント ID 一致、エラーなし、前後 JPEG のデコード成功、画像寸法が正、click 画像が白一色でないことをテストで確認。
- click の DOM diff は `removed: ""`, `inserted: "d"`, `truncated: false`。GUI 向け BrowserReport でも同じ差分を確認。
- screencast の最初のフレーム受信と Chromium の正常終了も検証。

これは Browser の実 CDP アダプターを通した検証。egui デスクトップのボタン操作や headful モードの実測ではない。

## 再現コマンド

```sh
EVORCH_BROWSER_EVIDENCE_DIR="$PWD/artifacts/browser-e2e-2026-09-11" \
  cargo test -j 1 -p gui --features browser --lib \
  browser::tests::chromium_screencast_and_action_evidence \
  -- --ignored --exact --nocapture --test-threads=1
```

環境変数を省略すると証跡は一時ディレクトリに保存され、テスト終了時に削除される。実測ごとに別ディレクトリを指定すれば過去の証跡を保持できる。

実行した workspace 検証（再試行なし）:

```sh
cargo test -j 1 --workspace --features gui/browser -- --test-threads=1
```
