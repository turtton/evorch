# v02-network-guard Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

## Slice-specific review focus

- NetworkGuard が実際に bwrap 外 main process 層に置かれているか。worker sandbox 内実行への移動や、bwrap 側 NetworkAccess 強制（`--unshare-net` 等）の緩和による「楽な」実現に逃げていないか（q07 の実行位置契約）
- HTTPS 強制が upgrade 後の https 接続失敗時に http へ fallback しない fail-closed になっているか。fallback を許す実装や error を握りつぶす経路がないか
- redirect が最大 10 回で、各 redirect 先 URL に HTTPS 強制と IP range 判定を含む同一ガードを再適用しているか。redirect 先で guard を素通りする実装になっていないか
- DNS pinning が解決 IP への直接接続で実装され、同一リクエスト内のアドレス一貫性が担保されているか。TTL 依存の再解決を挟み、rebinding（TTL の短い悪意ある DNS）を許していないか
- IP range 判定の許可/遮断の方向: link-local 169.254.0.0/16（169.254.169.254 metadata endpoint を含む）/ CGNAT 100.64.0.0/10 / IPv6 link-local fe80::/10 が確実に遮断され、loopback（127/8, ::1, `localhost`）と RFC 1918 private（10/8, 172.16/12, 192.168/16）が許可されるか。判定の取り違えや default allow（fail-open）がないか
- size チェックが Content-Length 事前 + 実読み streaming 累計 + gzip/deflate 解凍後累計の三面で効くか。Content-Length 詐称両方向（大きく宣言→少なく送付 / 宣言なし→大きく送付）で 5MB guard が機能するか。解凍爆弾に対する累計チェックが streaming 中に働くか
- 3 層 AND 判定が各層独立に deny を効かせ、OptIn が ask 承認なしで通過しない fail-closed になっているか。1 層でも通れば許可になる OR 実装に堕落していないか
- 依存クレート feasibility（reqwest redirect Policy / hickory-resolver / ipnet）の検証結果と選定根拠が記録に残る形で実装されているか
- scope widening: browser escalation / headless Chromium、per-domain allowlist、proxy selective egress、RSS / API 限定 fetch、web ツール本体（provider transport / extractor chain / ContentOrigin 型付け）を本 slice に持ち込んでいないか

## Facet context

<!-- BEGIN GENERATED FACET CONTEXT (G530) -->
### vocabulary
- (none overlapping this packet's intent_references)
### invariant
- (none overlapping this packet's intent_references)
### decider
- (none overlapping this packet's intent_references)
### acceptance-property
- (none overlapping this packet's intent_references)
<!-- END GENERATED FACET CONTEXT (G530) -->

注: `intent-cli intent facet-check` は lexical な補助に留まる。上記 Slice-specific review focus が main process 層 guard 境界と q07/q08 確定内容（3 層 AND + 厳格 SSRF ガード）の意味的接続を確認する主たる観点である。

## Knowledge Writeback Expectation (G461)

`closeout_learning.write_back_required` は `true`。closeout で以下が host 側に記録されているか確認する（この PR 内または追跡可能な follow-up packet）。

- `features/tools-sandbox/overview.md`: NetworkGuard が main process 層（bwrap 外）で確定した事実（HTTPS 強制 / redirect 再検証 / DNS pinning / IP range 判定 / size 三面チェック / 3 層 AND 判定インタフェース）の反映
- 依存クレート選定結果（reqwest redirect Policy / hickory-resolver DNS pinning / ipnet range 判定）と feasibility 検証結果
- facet `network.browser` 予約（q09）の明文化が overview 側で担保されていることの確認

記録が未実施の場合は、security boundary の仕様 drift が残るため知識 writeback 不足として review 所見に残す。
