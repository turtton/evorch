# v14-h-allowed-directories-lazy-instructions Review Context

## Position and scope

ADR 0032 の 3 段のうち ③。許可ディレクトリのルールと skills の読み込みだけを扱う。受け渡しとマウント（v14-f）、システムメッセージと途中の追記（v14-g）は前提である。プロジェクトルートのルール注入の重複除去はこの unit の範囲外であり、含まれないことを不足として扱わない。

## Intent references

- `intents/evorch/decisions/0032-project-allowed-directories.md`
- `intents/evorch/features/context-engine/overview.md`

## Review focus

### trust と範囲

- 未 trust の許可ディレクトリや、許可ディレクトリの外のパスから、ルールや skills が読み込まれないか。
- シンボリックリンクで許可ディレクトリの外を指すルールファイルや skill を読み込まないか（canonical path で判定しているか）。

### 重複と予算

- 同じルールファイルが run の中で二度注入されないか。復元の後も同様か。
- 予算を超えたときに、切り詰めや省略が明示されるか。

### skills

- 共有の skill catalog とシステムメッセージの skills 節が変わっていないか。ほかの run から許可ディレクトリの skill が見えていないか。
- 子 run に `load_skills` で渡したとき、子 run の registry で解決できるか。
- 名前の衝突で既存の skill が上書きされないか。

### cache

- 追記が末尾への追加だけで、送信済みのメッセージを編集していないか。

## Acceptance-to-evidence map

packet.yaml の acceptance_criteria の各項目に対応するテスト名を PR 本文に列挙する。
