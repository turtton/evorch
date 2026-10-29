## 1. モデル × 課題

### req（run_js あり ON / なし OFF。平均 [最小–最大]、3 rep。比 = ON / OFF。◆ = 範囲が重ならない）

| model | task | ON | OFF | 比 | |
|---|---|---|---|---|---|
| claude-opus | t1-survey-v3 | 14 [13–16] | 13 [12–14] | 1.08 |  |
| claude-opus | t2-aggregate-v3 | 13 [10–15] | 14 [13–14] | 0.95 |  |
| claude-opus | t3-rename-v3 | 40 [37–44] | 26 [25–29] | 1.51 | ◆ ON多 |
| claude-opus | t4-build-test-v3 | 30 [24–36] | 33 [32–34] | 0.92 |  |
| claude-opus | t5-single-edit-v3 | 6 [5–7] | 6 [5–7] | 1.00 |  |
| claude-opus | t6-debug-fix-v3 | 10 [9–11] | 10 [10–11] | 0.97 |  |
| claude-sonnet | t1-survey-v3 | 11 [9–13] | 11 [10–12] | 1.00 |  |
| claude-sonnet | t2-aggregate-v3 | 9 [6–11] | 9 [7–11] | 0.96 |  |
| claude-sonnet | t3-rename-v3 | 22 [19–23] | 21 [17–25] | 1.03 |  |
| claude-sonnet | t4-build-test-v3 | 16 [16–17] | 15 [12–17] | 1.07 |  |
| claude-sonnet | t5-single-edit-v3 | 3 | 3 | 1.00 |  |
| claude-sonnet | t6-debug-fix-v3 | 6 [6–7] | 7 [6–9] | 0.90 |  |
| gpt-astra | t1-survey-v3 | 11 [9–13] | 9 [8–10] | 1.19 |  |
| gpt-astra | t2-aggregate-v3 | 11 [8–15] | 12 [11–13] | 0.92 |  |
| gpt-astra | t3-rename-v3 | 20 [19–21] | 19 [16–24] | 1.03 |  |
| gpt-astra | t4-build-test-v3 | 30 [23–36] | 21 [20–23] | 1.41 |  |
| gpt-astra | t5-single-edit-v3 | 4 | 4 | 1.00 |  |
| gpt-astra | t6-debug-fix-v3 | 17 [12–22] | 14 [14–15] | 1.16 |  |
| gpt-luna-fast | t1-survey-v3 | 21 [20–23] | 12 [9–17] | 1.78 | ◆ ON多 |
| gpt-luna-fast | t2-aggregate-v3 | 14 [8–20] | 6 [5–9] | 2.16 |  |
| gpt-luna-fast | t3-rename-v3 | 35 [31–42] | 38 [32–45] | 0.90 |  |
| gpt-luna-fast | t4-build-test-v3 | 39 [31–50] | 41 [31–51] | 0.94 |  |
| gpt-luna-fast | t5-single-edit-v3 | 5 [4–6] | 6 [4–7] | 0.78 |  |
| gpt-luna-fast | t6-debug-fix-v3 | 13 [11–15] | 16 [13–18] | 0.85 |  |

範囲が重ならない組: ON が小さい 0 / ON が大きい 2 / 重なる 22（24 組）

### cost（run_js あり ON / なし OFF。平均 [最小–最大]、3 rep。比 = ON / OFF。◆ = 範囲が重ならない）

| model | task | ON | OFF | 比 | |
|---|---|---|---|---|---|
| claude-opus | t1-survey-v3 | 0.440 [0.420–0.458] | 0.377 [0.346–0.410] | 1.17 | ◆ ON多 |
| claude-opus | t2-aggregate-v3 | 0.467 [0.436–0.507] | 0.495 [0.463–0.518] | 0.94 |  |
| claude-opus | t3-rename-v3 | 0.984 [0.929–1.041] | 0.776 [0.676–0.935] | 1.27 |  |
| claude-opus | t4-build-test-v3 | 1.092 [0.950–1.269] | 1.136 [1.108–1.157] | 0.96 |  |
| claude-opus | t5-single-edit-v3 | 0.065 [0.048–0.082] | 0.072 [0.049–0.094] | 0.90 |  |
| claude-opus | t6-debug-fix-v3 | 0.242 [0.227–0.257] | 0.247 [0.224–0.269] | 0.98 |  |
| claude-sonnet | t1-survey-v3 | 0.228 [0.179–0.296] | 0.203 [0.184–0.220] | 1.12 |  |
| claude-sonnet | t2-aggregate-v3 | 0.152 [0.114–0.178] | 0.154 [0.121–0.192] | 0.98 |  |
| claude-sonnet | t3-rename-v3 | 0.301 [0.256–0.329] | 0.359 [0.326–0.422] | 0.84 |  |
| claude-sonnet | t4-build-test-v3 | 0.358 [0.306–0.434] | 0.293 [0.256–0.326] | 1.22 |  |
| claude-sonnet | t5-single-edit-v3 | 0.021 [0.020–0.021] | 0.019 [0.019–0.020] | 1.07 | ◆ ON多 |
| claude-sonnet | t6-debug-fix-v3 | 0.082 [0.074–0.092] | 0.092 [0.077–0.119] | 0.89 |  |
| gpt-astra | t1-survey-v3 | 0.667 [0.538–0.772] | 0.542 [0.462–0.620] | 1.23 |  |
| gpt-astra | t2-aggregate-v3 | 0.689 [0.514–0.911] | 0.678 [0.618–0.738] | 1.02 |  |
| gpt-astra | t3-rename-v3 | 1.128 [1.037–1.236] | 1.050 [0.904–1.270] | 1.07 |  |
| gpt-astra | t4-build-test-v3 | 1.579 [1.472–1.682] | 0.990 [0.890–1.107] | 1.59 | ◆ ON多 |
| gpt-astra | t5-single-edit-v3 | 0.114 [0.082–0.131] | 0.117 [0.063–0.175] | 0.97 |  |
| gpt-astra | t6-debug-fix-v3 | 0.705 [0.547–0.883] | 0.523 [0.486–0.583] | 1.35 |  |
| gpt-luna-fast | t1-survey-v3 | 0.022 [0.021–0.023] | 0.015 [0.010–0.024] | 1.44 |  |
| gpt-luna-fast | t2-aggregate-v3 | 0.018 [0.015–0.024] | 0.009 [0.007–0.011] | 1.97 | ◆ ON多 |
| gpt-luna-fast | t3-rename-v3 | 0.049 [0.039–0.055] | 0.053 [0.048–0.061] | 0.93 |  |
| gpt-luna-fast | t4-build-test-v3 | 0.053 [0.049–0.058] | 0.046 [0.044–0.049] | 1.16 |  |
| gpt-luna-fast | t5-single-edit-v3 | 0.003 [0.002–0.004] | 0.004 [0.004–0.005] | 0.73 |  |
| gpt-luna-fast | t6-debug-fix-v3 | 0.011 [0.009–0.013] | 0.016 [0.012–0.019] | 0.74 |  |

範囲が重ならない組: ON が小さい 0 / ON が大きい 4 / 重なる 20（24 組）

### dur（run_js あり ON / なし OFF。平均 [最小–最大]、3 rep。比 = ON / OFF。◆ = 範囲が重ならない）

| model | task | ON | OFF | 比 | |
|---|---|---|---|---|---|
| claude-opus | t1-survey-v3 | 101 [91–111] | 91 [81–101] | 1.11 |  |
| claude-opus | t2-aggregate-v3 | 143 [136–151] | 140 [126–151] | 1.02 |  |
| claude-opus | t3-rename-v3 | 269 [202–318] | 226 [187–268] | 1.19 |  |
| claude-opus | t4-build-test-v3 | 224 [207–242] | 242 [222–253] | 0.92 |  |
| claude-opus | t5-single-edit-v3 | 30 [25–35] | 32 [25–35] | 0.95 |  |
| claude-opus | t6-debug-fix-v3 | 79 [76–81] | 81 [76–86] | 0.98 |  |
| claude-sonnet | t1-survey-v3 | 87 [66–116] | 96 [66–151] | 0.91 |  |
| claude-sonnet | t2-aggregate-v3 | 91 [66–116] | 74 [66–86] | 1.23 |  |
| claude-sonnet | t3-rename-v3 | 193 [156–212] | 138 [121–152] | 1.40 | ◆ ON多 |
| claude-sonnet | t4-build-test-v3 | 153 [116–197] | 116 [101–131] | 1.32 |  |
| claude-sonnet | t5-single-edit-v3 | 18 [15–25] | 15 [15–15] | 1.22 |  |
| claude-sonnet | t6-debug-fix-v3 | 62 [55–66] | 54 [45–66] | 1.16 |  |
| gpt-astra | t1-survey-v3 | 158 [141–187] | 129 [121–146] | 1.22 |  |
| gpt-astra | t2-aggregate-v3 | 227 [187–293] | 246 [212–268] | 0.93 |  |
| gpt-astra | t3-rename-v3 | 182 [166–197] | 199 [167–227] | 0.91 |  |
| gpt-astra | t4-build-test-v3 | 277 [277–277] | 232 [203–252] | 1.19 | ◆ ON多 |
| gpt-astra | t5-single-edit-v3 | 27 [25–30] | 27 [25–30] | 1.00 |  |
| gpt-astra | t6-debug-fix-v3 | 133 [126–146] | 114 [81–136] | 1.16 |  |
| gpt-luna-fast | t1-survey-v3 | 178 [146–242] | 87 [60–121] | 2.04 | ◆ ON多 |
| gpt-luna-fast | t2-aggregate-v3 | 116 [96–151] | 69 [55–76] | 1.68 | ◆ ON多 |
| gpt-luna-fast | t3-rename-v3 | 222 [172–252] | 242 [212–277] | 0.92 |  |
| gpt-luna-fast | t4-build-test-v3 | 208 [187–227] | 234 [197–282] | 0.89 |  |
| gpt-luna-fast | t5-single-edit-v3 | 25 [20–30] | 37 [25–45] | 0.68 |  |
| gpt-luna-fast | t6-debug-fix-v3 | 74 [66–81] | 89 [81–101] | 0.83 | ◆ ON少 |

範囲が重ならない組: ON が小さい 1 / ON が大きい 4 / 重なる 19（24 組）

## 2. モデル別・全体の合計（各 18 run、全体 72 run）


### claude-opus

| 指標 | ON | OFF | 比 ON/OFF |
|---|---|---|---|
| req | 338 | 306 | 1.10 |
| input | 14,304,145 | 12,258,782 | 1.17 |
| cache_read | 13,649,335 | 11,580,306 | 1.18 |
| cache_write | 654,088 | 677,828 | 0.96 |
| output | 193,150 | 179,925 | 1.07 |
| input−cache_read | 654,810 | 678,476 | 0.97 |
| 費用($) | 9.87 | 9.31 | 1.06 |
| 時間(s) | 2,537 | 2,432 | 1.04 |
| top-level ツール呼び出し | 381 | 360 | 1.06 |
| shell コマンド | 204 | 223 | 0.91 |
| prog | 55 | 76 | 0.72 |
| └ script | 22 | 35 | 0.63 |
| └ loop | 29 | 38 | 0.76 |
| └ textproc | 25 | 27 | 0.93 |
| └ sed_multi | 0 | 2 | 0.00 |
| build chain | 20 | 16 | 1.25 |
| write_only ヒアドキュメント | 0 | 0 | — |
| segments≥4 のコマンド | 94 | 105 | 0.90 |
| verify 合格 | 18/18 | 18/18 | |

### claude-sonnet

| 指標 | ON | OFF | 比 ON/OFF |
|---|---|---|---|
| req | 202 | 200 | 1.01 |
| input | 6,048,923 | 6,070,805 | 1.00 |
| cache_read | 5,639,304 | 5,631,769 | 1.00 |
| cache_write | 409,181 | 438,596 | 0.93 |
| output | 127,086 | 113,795 | 1.12 |
| input−cache_read | 409,619 | 439,036 | 0.93 |
| 費用($) | 3.42 | 3.36 | 1.02 |
| 時間(s) | 1,816 | 1,479 | 1.23 |
| top-level ツール呼び出し | 215 | 203 | 1.06 |
| shell コマンド | 158 | 186 | 0.85 |
| prog | 61 | 84 | 0.73 |
| └ script | 35 | 62 | 0.56 |
| └ loop | 30 | 29 | 1.03 |
| └ textproc | 13 | 27 | 0.48 |
| └ sed_multi | 4 | 5 | 0.80 |
| build chain | 18 | 17 | 1.06 |
| write_only ヒアドキュメント | 12 | 16 | 0.75 |
| segments≥4 のコマンド | 93 | 111 | 0.84 |
| verify 合格 | 18/18 | 18/18 | |

### gpt-astra

| 指標 | ON | OFF | 比 ON/OFF |
|---|---|---|---|
| req | 276 | 239 | 1.15 |
| input | 6,131,429 | 4,381,190 | 1.40 |
| cache_read | 5,662,976 | 3,984,768 | 1.42 |
| cache_write | 0 | 0 | — |
| output | 85,984 | 75,098 | 1.14 |
| input−cache_read | 468,453 | 396,422 | 1.18 |
| 費用($) | 14.65 | 11.70 | 1.25 |
| 時間(s) | 3,012 | 2,841 | 1.06 |
| top-level ツール呼び出し | 271 | 232 | 1.17 |
| shell コマンド | 109 | 137 | 0.80 |
| prog | 3 | 57 | 0.05 |
| └ script | 0 | 52 | 0.00 |
| └ loop | 3 | 6 | 0.50 |
| └ textproc | 0 | 1 | 0.00 |
| └ sed_multi | 0 | 0 | — |
| build chain | 15 | 13 | 1.15 |
| write_only ヒアドキュメント | 0 | 0 | — |
| segments≥4 のコマンド | 19 | 50 | 0.38 |
| verify 合格 | 18/18 | 18/18 | |

### gpt-luna-fast

| 指標 | ON | OFF | 比 ON/OFF |
|---|---|---|---|
| req | 380 | 359 | 1.06 |
| input | 11,206,769 | 10,156,255 | 1.10 |
| cache_read | 10,448,000 | 9,391,872 | 1.11 |
| cache_write | 0 | 0 | — |
| output | 111,329 | 88,012 | 1.26 |
| input−cache_read | 758,769 | 764,383 | 0.99 |
| 費用($) | 0.47 | 0.43 | 1.10 |
| 時間(s) | 2,472 | 2,276 | 1.09 |
| top-level ツール呼び出し | 448 | 447 | 1.00 |
| shell コマンド | 186 | 245 | 0.76 |
| prog | 11 | 38 | 0.29 |
| └ script | 9 | 34 | 0.26 |
| └ loop | 3 | 7 | 0.43 |
| └ textproc | 1 | 0 | — |
| └ sed_multi | 0 | 0 | — |
| build chain | 7 | 6 | 1.17 |
| write_only ヒアドキュメント | 0 | 3 | 0.00 |
| segments≥4 のコマンド | 9 | 18 | 0.50 |
| verify 合格 | 13/18 | 15/18 | |

### 全体

| 指標 | ON | OFF | 比 ON/OFF |
|---|---|---|---|
| req | 1,196 | 1,104 | 1.08 |
| input | 37,691,266 | 32,867,032 | 1.15 |
| cache_read | 35,399,615 | 30,588,715 | 1.16 |
| cache_write | 1,063,269 | 1,116,424 | 0.95 |
| output | 517,549 | 456,830 | 1.13 |
| input−cache_read | 2,291,651 | 2,278,317 | 1.01 |
| 費用($) | 28.41 | 24.80 | 1.15 |
| 時間(s) | 9,836 | 9,028 | 1.09 |
| top-level ツール呼び出し | 1,315 | 1,242 | 1.06 |
| shell コマンド | 657 | 791 | 0.83 |
| prog | 130 | 255 | 0.51 |
| └ script | 66 | 183 | 0.36 |
| └ loop | 65 | 80 | 0.81 |
| └ textproc | 39 | 55 | 0.71 |
| └ sed_multi | 4 | 7 | 0.57 |
| build chain | 60 | 52 | 1.15 |
| write_only ヒアドキュメント | 12 | 19 | 0.63 |
| segments≥4 のコマンド | 215 | 284 | 0.76 |
| verify 合格 | 67/72 | 69/72 | |

## 3. 課題別（4 モデル × 3 rep の合計。比 = ON / OFF）

| task | req ON | req OFF | 比 | 費用 ON($) | 費用 OFF($) | 比 | 時間 ON(s) | 時間 OFF(s) | 比 | prog ON | prog OFF | tools ON | tools OFF |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| t1-survey-v3 | 171 | 135 | 1.27 | 4.07 | 3.41 | 1.19 | 1574 | 1210 | 1.30 | 23 | 59 | 172 | 141 |
| t2-aggregate-v3 | 140 | 124 | 1.13 | 3.98 | 4.01 | 0.99 | 1730 | 1584 | 1.09 | 43 | 67 | 131 | 120 |
| t3-rename-v3 | 348 | 315 | 1.10 | 7.38 | 6.71 | 1.10 | 2599 | 2414 | 1.08 | 35 | 76 | 377 | 367 |
| t4-build-test-v3 | 345 | 331 | 1.04 | 9.25 | 7.40 | 1.25 | 2588 | 2474 | 1.05 | 23 | 44 | 434 | 389 |
| t5-single-edit-v3 | 53 | 57 | 0.93 | 0.61 | 0.64 | 0.95 | 302 | 332 | 0.91 | 0 | 0 | 45 | 54 |
| t6-debug-fix-v3 | 139 | 142 | 0.98 | 3.12 | 2.63 | 1.19 | 1044 | 1014 | 1.03 | 6 | 9 | 156 | 171 |

課題 × モデルの req 比（ON/OFF、3 rep 合計）

| task | claude-opus | claude-sonnet | gpt-astra | gpt-luna-fast |
|---|---|---|---|---|
| t1-survey-v3 | 1.08 (42/39) | 1.00 (33/33) | 1.19 (32/27) | 1.78 (64/36) |
| t2-aggregate-v3 | 0.95 (39/41) | 0.96 (27/28) | 0.92 (33/36) | 2.16 (41/19) |
| t3-rename-v3 | 1.51 (119/79) | 1.03 (65/63) | 1.03 (60/58) | 0.90 (104/115) |
| t4-build-test-v3 | 0.92 (90/98) | 1.07 (49/46) | 1.41 (89/63) | 0.94 (117/124) |
| t5-single-edit-v3 | 1.00 (18/18) | 1.00 (9/9) | 1.00 (12/12) | 0.78 (14/18) |
| t6-debug-fix-v3 | 0.97 (30/31) | 0.90 (19/21) | 1.16 (50/43) | 0.85 (40/47) |

課題 × モデルの費用比（ON/OFF）

| task | claude-opus | claude-sonnet | gpt-astra | gpt-luna-fast |
|---|---|---|---|---|
| t1-survey-v3 | 1.17 | 1.12 | 1.23 | 1.44 |
| t2-aggregate-v3 | 0.94 | 0.98 | 1.02 | 1.97 |
| t3-rename-v3 | 1.27 | 0.84 | 1.07 | 0.93 |
| t4-build-test-v3 | 0.96 | 1.22 | 1.59 | 1.16 |
| t5-single-edit-v3 | 0.90 | 1.07 | 0.97 | 0.73 |
| t6-debug-fix-v3 | 0.98 | 0.89 | 1.35 | 0.74 |

## 4. shell のプログラム的構造 prog（モデル × 課題。平均 [範囲]）

| model | task | ON | OFF |
|---|---|---|---|
| claude-opus | t1-survey-v3 | 2.3 [1–4] | 6.0 [5–7] |
| claude-opus | t2-aggregate-v3 | 6.0 [5–8] | 7.3 [6–10] |
| claude-opus | t3-rename-v3 | 6.7 [5–9] | 7.7 [7–8] |
| claude-opus | t4-build-test-v3 | 2.7 [2–4] | 3.3 [2–4] |
| claude-opus | t5-single-edit-v3 | 0.0 | 0.0 |
| claude-opus | t6-debug-fix-v3 | 0.7 [0–1] | 1.0 |
| claude-sonnet | t1-survey-v3 | 4.7 [4–6] | 5.0 [4–6] |
| claude-sonnet | t2-aggregate-v3 | 4.7 [2–6] | 5.7 [4–7] |
| claude-sonnet | t3-rename-v3 | 4.7 [3–7] | 10.3 [8–12] |
| claude-sonnet | t4-build-test-v3 | 5.0 [3–7] | 6.0 [5–7] |
| claude-sonnet | t5-single-edit-v3 | 0.0 | 0.0 |
| claude-sonnet | t6-debug-fix-v3 | 1.3 [1–2] | 1.0 |
| gpt-astra | t1-survey-v3 | 0.0 | 3.7 [3–5] |
| gpt-astra | t2-aggregate-v3 | 0.7 [0–2] | 6.3 [5–8] |
| gpt-astra | t3-rename-v3 | 0.3 [0–1] | 5.0 |
| gpt-astra | t4-build-test-v3 | 0.0 | 3.0 |
| gpt-astra | t5-single-edit-v3 | 0.0 | 0.0 |
| gpt-astra | t6-debug-fix-v3 | 0.0 | 1.0 |
| gpt-luna-fast | t1-survey-v3 | 0.7 [0–2] | 5.0 [3–7] |
| gpt-luna-fast | t2-aggregate-v3 | 3.0 [1–7] | 3.0 [2–4] |
| gpt-luna-fast | t3-rename-v3 | 0.0 | 2.3 [1–5] |
| gpt-luna-fast | t4-build-test-v3 | 0.0 | 2.3 [0–6] |
| gpt-luna-fast | t5-single-edit-v3 | 0.0 | 0.0 |
| gpt-luna-fast | t6-debug-fix-v3 | 0.0 | 0.0 |

## 5. shell コマンドの文字数分布（top-level の shell 開始コマンド）

| model | 条件 | 件数 | 中央値 | p90 | 最大 | 300 字以上 | 500 字以上 |
|---|---|---|---|---|---|---|---|
| claude-opus | ON | 205 | 250 | 433 | 819 | 60 | 14 |
| claude-opus | OFF | 223 | 265 | 507 | 4406 | 77 | 23 |
| claude-sonnet | ON | 158 | 278 | 1308 | 5232 | 69 | 32 |
| claude-sonnet | OFF | 186 | 303 | 1761 | 7735 | 95 | 53 |
| gpt-astra | ON | 109 | 160 | 380 | 530 | 17 | 1 |
| gpt-astra | OFF | 137 | 298 | 1836 | 6674 | 67 | 38 |
| gpt-luna-fast | ON | 186 | 72 | 184 | 773 | 10 | 2 |
| gpt-luna-fast | OFF | 245 | 97 | 439 | 6958 | 35 | 21 |
| 全体 | ON | 658 | 180 | 460 | 5232 | 156 | 49 |
| 全体 | OFF | 791 | 227 | 920 | 7735 | 274 | 135 |

## 6. rep 間のばらつき（req の最大/最小）

ON: 中央値 1.40 / 最大 2.50、OFF: 中央値 1.38 / 最大 1.89
rep 別の費用: ON 9.76 / 9.31 / 9.33、OFF 8.43 / 8.37 / 7.99
