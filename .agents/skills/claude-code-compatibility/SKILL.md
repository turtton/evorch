---
name: claude-code-compatibility
description: "evorch の Claude サブスク互換性を Claude Code の更新に追従させる。native/OMP 解析、隔離した通信採取、TLS・HTTP・checksum の差分調査と、その知見による skill 自身の改善に使う。通常の Anthropic API 利用やログイン設定には使わない。"
---

# Claude Code compatibility

Claude Code の実測を根拠にサブスク専用 adapter を更新し、次の追従でも使える解析手順を残す。
現在の仕様・参照バージョン・意図した差分の正本は
[docs/claude-compatibility.md](../../../docs/claude-compatibility.md) と、そこから辿る実装。
この skill の過去の記述や OMP の定数を新バージョンの仕様とみなさない。

offline 調査だけの依頼や対象 binary が未入手の場合は、既存の実装・fixture の解析と
次の採取準備まで行う。新 version の仕様と採否は未確認として報告し、profile を推測で更新しない。

## 進め方

1. [解析と検証](references/analysis.md) を読み、対象 native binary の実体・hash・version、OMP の full commit、evorch HEAD を記録する。wrapper と実体、調査対象と手元の一時 clone を区別する。
2. 意味のある文字列と呼出経路から静的解析し、各差分を「native 実測」「静的解析」「OMP からの推定」「未確認」に分ける。旧 byte offset や minified symbol を流用しない。
3. 通信の実測が必要なら [隔離採取](references/capture.md) を読む。dummy 認証・隔離設定・外向き通信遮断を用意してから、必要な request 種別だけを採取する。
4. native の証拠と evorch の契約を比較し、採用する差分と意図して残す差分を決める。CLI/SDK/runtime、headers、billing、TLS を整合した profile として更新し、会話の途中で profile を変更しない。
5. 独立した synthetic vector と HTTP/TLS fixture で差分を検証し、現在のドキュメントにある更新 gate を実行する。API-key 経路、schema/引数、署名付き履歴、送信済み prefix の契約を保つ。
6. 下記の改善ループを行い、pin 情報・採否・検証結果・未測定事項・skill の改善内容を報告する。

## 採取と証拠の境界

- ローカル解析に本人の認証情報や通常の Claude 設定を使わない。公式 host を使う採取でも、CONNECT を固定 loopback listener に転送し、証明書・hostname 検証を有効にする。proxy の許可リストだけでは外向き通信の遮断にならない。
- credential、bundled system prompt/tool 定義、raw request body、CLI 全出力を skill や repo の fixture に入れない。必要な raw 資料は repo 外のアクセス制限した一時領域で扱い、保存理由と破棄・保持範囲を明確にする。
- mock の cache token や通信形状の一致から、実サービスの cache hit・サブスク適格性・usage-credit routing を断定しない。実認証・実 endpoint の検証は、対象 profile と消費上限を含むユーザーの実行許可がある場合だけ行う。既存の許可が今回を含むなら再確認しない。
- 採取不能や positive control の失敗は「一致」と扱わない。未測定と理由を残し、証拠を得るために実アカウントや通常設定へ切り替えない。

## 繰り返し使う処理

未暗号化 ClientHello を含む TLS records を調べるときは、依存ライブラリ不要の
[inspect-client-hello.mjs](scripts/inspect-client-hello.mjs) を使う。入力の抽出方法、比較例、PSK の扱いは [隔離採取](references/capture.md) を参照。
採取サーバーの起動、通信、認証の読込みは行わないオフライン処理である。

## 使用ごとの改善ループ

**追従作業のたびに、この skill 自体も見直して改善する。**

- 今回確認できた再利用可能な手順、判断を誤った罠、実行環境の前提を該当 reference や script に反映する。仕様の値は production と互換性ドキュメントへ、解析の方法は skill へ置く。
- 新しい知見には対象 version/hash/commit、観測条件、証拠の場所を添える。単発の推測を一般則にせず、古い手順の修正・削除・重複の統合も行う。
- script を変えたら focused test、判断手順を大きく変えたら独立 agent による offline の試用で確かめる。得た不具合に絞って直す。
- 改善を同じ作業の差分に含め、何を変えたか報告する。変更不要なら理由を報告し、毎回の無意味な追記や履歴の蓄積はしない。
