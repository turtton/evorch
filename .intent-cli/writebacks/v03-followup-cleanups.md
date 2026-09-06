# Writeback: v03-followup-cleanups — v0.3: follow-up cleanups (issue #96)

source_issue: #96 / source_execution_unit: v03-followup-cleanups / PR: #97 (merged c56f894)

## 実施内容
- ReasoningDelta production emit: crates/runtime/src/agent_loop.rs が非空 Reasoning ブロックを ReasoningDelta として emit（run_id: Some(run_id)、content order、空ブロック除外。PR #86 規約整合）
- frame.rs legacy mirror 撤去: run_id:None delta は TranscriptRegistry 層で drop + tracing::warn。表示は payload run_id 帰属経路のみで維持。apply_stream_delta API は代替テスト群とともに撤去
- agents グリッド列幅自動フィット: theme tokens AGENTS_COL_MIN=56/AGENTS_COL_MAX=160/CELL_PAD_X=8 単一出所化、fit_columns（token 境界 clamp → slack 比例縮小 → 0 clamp 付き最終スケール）+ 可視 clip 幅基準計測 + truncate 表示

## 判断
- run_id:None の扱いは frame 層削除に留めず registry 層 full drop + warn（ルーティング権威の単一化）
- 列幅測定基準は available_width ではなく min(clip_rect.width())（dock ホスト非有界幅で fit 未発動となる障害を修正）
- MIN floor は希望下限とみなし最終スケールで未満化を許容（極小幅は truncate で代替保持）

## 次の候補
- 約 340px の狭幅では run ID / Open pane ボタンも省略記号化（Reviewer note）。行識別の発見性向上は別 follow-up 候補
- helper の幅計算パターン: 自然幅 = layout_no_wrap ガレー幅 + CELL_PAD_X、button 列は 2*button_padding.x を別途加算
