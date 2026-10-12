# v13-a-provider-failure-cooldown Review Context

## Position and scope

ADR 0030 の 4 段のうち ①。失敗の詳細・分類・クールダウンだけを扱う。ロールのチェーンと一周するフォールバックは v13-b であり、この PR に含まれないことを不足として扱わない。

## Intent references

- `intents/evorch/decisions/0030-role-model-chains.md`
- `intents/evorch/decisions/0004-provider-routing-separation.md`
- `intents/evorch/features/provider-routing/overview.md`

## Review focus

### 秘匿化

- エラー詳細に API key やトークンが残らないか。secret-guard と、プロファイルの api key の置換の両方を通っているか。
- 1 KiB の切り詰めが UTF-8 の境界で壊れないか。

### クールダウン

- 単位が正しいか（Quota / Auth はプロファイル、RateLimited は（プロファイル, モデル））。
- 全メンバーがクールダウン中のとき、無限に待ったり空の成功を返したりせず、候補なしのエラーになるか。
- runtime の再構成で Auth のクールダウンが解除されるか。テスト用の時計を注入できるか（sleep に依存しないか）。

### 観測

- 新しいイベントと `detail` の追加で、GUI の通知・テレメトリ・usage ledger・storage projection が壊れないか。

## Acceptance-to-evidence map

packet.yaml の acceptance_criteria の各項目に対応するテスト名を PR 本文に列挙する。
