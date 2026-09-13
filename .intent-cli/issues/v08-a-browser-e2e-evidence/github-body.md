## Goal

browser-e2e ジョブの証跡 artifact が空になる問題を修正し、証跡（PNG 等）が必ず
EVORCH_BROWSER_EVIDENCE_DIR 配下に出力され CI artifact に含まれるようにする。

## Why This Slice Exists Now

CI run 34605848371 (2026-09-11) で browser-e2e は成功したのに artifact が未アップロード
（target/browser-evidence not found）だった。検証 two-layer 設計（ADR 0015）の証跡契約が
実運用で破れており、E2E の証拠性が失われている。

## Current Observed State

- chromium_screencast_and_action_evidence は EVORCH_BROWSER_EVIDENCE_DIR を解釈する契約。
- browser-e2e ジョブ成功時に証跡が出力されない条件が存在する（経路のずれ）。
- upload-artifact の if-no-files-found 挙動が未明示で、空 artifact が黙って成功し得る。

## Accepted Baseline You May Assume

- browser-e2e ゲート自体は v08 で導入済みで動作している。
- ADR 0015（verification two-layer）の証跡契約に従う。

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/gui/src/browser/tests.rs, .github/workflows/ci.yml`

Target part: browser-e2e ジョブの証跡出力経路

## In Scope

- 証跡の書き込み経路と出力されない条件の特定・記録
- browser-e2e ジョブ成功時に証跡が必ず EVORCH_BROWSER_EVIDENCE_DIR 配下に生成される修正
- CI の browser-e2e artifact（browser-e2e-evidence）に証跡ファイルが含まれるようにする
- upload-artifact の if-no-files-found 挙動の明示（空 artifact が黙って成功しない）

## Out Of Scope

- browser-e2e テスト内容自体の拡張
- 他ジョブ（ci / offscreen-gate）の証跡経路の変更

## Standalone Child Issue Contract

evorch の browser-e2e ジョブについて、証跡が出力されない原因を特定・記録し、ジョブ成功時に
証跡（PNG 等）が必ず EVORCH_BROWSER_EVIDENCE_DIR 配下に生成され CI artifact
（browser-e2e-evidence）に含まれるよう修正し、upload-artifact の if-no-files-found 挙動を
明示して空 artifact が黙って成功しないようにする変更を PR として提出する。

## Acceptance Criteria

- crates/gui/src/browser/tests.rs を読み、証跡の書き込み経路と出力されない条件を特定して記録する
- browser-e2e ジョブ成功時に証跡（PNG 等）が必ず EVORCH_BROWSER_EVIDENCE_DIR 配下に生成される
- CI の browser-e2e artifact（browser-e2e-evidence）に証跡ファイルが含まれる
- upload-artifact の if-no-files-found 挙動を明示し、空 artifact が黙って成功しない

## Verification

- browser-e2e ジョブを実 CI で実行し、artifact に証跡ファイルが含まれることを確認
- `git diff --check`

## Related Links

- ADR 0015: intents/evorch/decisions/0015-verification-two-layer.md
- 参照 CI run: 34605848371（2026-09-11、artifact 未アップロードの事例）

## Knowledge Maintenance

- Intent placement: architecture overview（既存 intent、新規不要）
- ADR candidate: none
- Diagram candidate: none
- Docs update: none
- Closeout writeback expected: no

## Guide Reachability (G645)

no_role_facing_surface: true（CI/テスト基盤の修正で role-facing surface の追加なし）

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
