---
name: sandbox-build
description: "evorch の sandbox 内で cargo build / check / test / nextest を実行するときの環境の整え方。libclang (bindgen, btls-sys) ・CEF (gui)・LD_LIBRARY_PATH・cargo fork をまとめて解決し、/nix/store を探し回らずに済ませる。trigger: sandbox ビルド, cargo build, cargo test, nextest, libclang, LIBCLANG_PATH, bindgen, Unable to find libclang, CEF_PATH, btls-sys, cef-dll-sys, LD_LIBRARY_PATH, nix print-dev-env, 環境変数が無い"
---

# sandbox-build — sandbox 内の cargo 環境

sandbox の PATH には dev shell のツール (cargo fork, rustc, cmake, gcc, nextest) が入っているが、
`LIBCLANG_PATH`・`BINDGEN_EXTRA_CLANG_ARGS`・`CEF_PATH`・`LD_LIBRARY_PATH`・`RUSTFLAGS` は引き継がれない。
`nix` も PATH に無い。`/nix/store` を `ls`/`find` して clang や glibc を推測しない
(バージョン違いの clang・glibc を掴み、bindgen が不整合で落ちる)。
環境変数・Cargo 設定の一般説明は AGENTS.md の Development environment 節を参照。

## 手順

ビルドする checkout / worktree のルートで、コマンドを wrapper 経由で実行する。

```sh
.agents/skills/sandbox-build/scripts/with-dev-env.sh cargo nextest run --offline -p providers
```

wrapper は cwd の git ルート (ビルド対象) を対象にし、wrapper 自体の置き場所には依存しない。
そのため、ビルドする worktree の中に wrapper が無くても、別の場所の wrapper を絶対パスで呼べる。

### worktree 内に wrapper が見つからないとき

古い commit から作られた worktree には `scripts/with-dev-env.sh` が存在しない
(`No such file or directory`)。ここで `nix` や `/nix/store` を探し回らない。
main clone (全 worktree が共有する `.git` を持つ元のプロジェクト root) の wrapper を、
ビルドする worktree を cwd にしたまま絶対パスで呼ぶ。

```sh
# worktree 内で実行する。--git-common-dir は main clone の .git を指す。
main_clone="$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")"
"$main_clone/.agents/skills/sandbox-build/scripts/with-dev-env.sh" cargo nextest run --offline -p providers
```

main clone 側にも無い (古いブランチを checkout している) 場合は、`origin/main` から
`$TMPDIR` へ取り出して呼ぶ。

```sh
git show origin/main:.agents/skills/sandbox-build/scripts/with-dev-env.sh > "$TMPDIR/with-dev-env.sh"
bash "$TMPDIR/with-dev-env.sh" cargo nextest run --offline -p providers
```

どちらの場合も cwd はビルドする worktree のままにする (wrapper が `git rev-parse --show-toplevel` で対象を決める)。

wrapper は `nix print-dev-env --offline` で flake の default dev shell 環境を生成して
`$TMPDIR` にキャッシュ (flake.nix / flake.lock / rust-toolchain.toml のハッシュ単位) し、読み込んで `exec` する。
store のパスは flake が決めるので、固定ハッシュを書かない。
初回は約 15 秒かかる。シェル呼び出しごとに環境は消えるので、毎回 wrapper を付ける。

- 失敗する場合: `nix` が store に無い、flake の入力が store に無い (`--offline` のため)。
  その場合は推測せず、エラーをそのまま報告する。
- `Unable to find libclang` はこの環境が無いことの症状。wrapper を付け直す。

## ビルド対象の絞り方

- libclang が要るのは `btls-sys` (bindgen)。`providers` → `wreq` 経由なので、`providers` に依存するほぼ全クレートが対象。
  `workspace-ui` のような葉クレートは libclang も CEF も要らない。
- CEF が要るのは `gui` (`browser-cef`, default feature `cef`) と、`gui` を dev-dependency に持つ
  `core-client-e2e`。`CEF_PATH` は wrapper が設定する。`cef-dll-sys` の C++ ラッパーを初回に
  ビルドするので `cargo check -p gui` は約 4 分かかる。GUI を触らない変更は `-p <crate>` で対象を絞り、
  `--workspace` や `--all-targets` を避ける。
- テストバイナリは `LD_LIBRARY_PATH` に `libcef.so` を要求する。wrapper 経由なら設定済み。
  CEF を実際に起動するテスト (`cef-e2e`) は対象外。docs/browser.md 参照。

## 注意

- sandbox 内の `cargo` はネットワークに出られない。`--offline` を付ける。
- 主チェックアウト (`.git` が `bare = true` + `extensions.worktreeConfig`) では、cargo fork が
  `did not expect repo at .../.git to be bare` で一部クレート (例: `workspace-ui`) のビルドに失敗する。
  `git worktree` で作った作業ツリーでは起きない。Worker は worktree で作業する。
- dev shell の `RUSTC_WRAPPER` は空 (sccache なし)。fork が依存ビルドを `~/.cargo/targets` で共有する。
  `CARGO_TARGET_DIR` は設定しない (fork は無視する)。
