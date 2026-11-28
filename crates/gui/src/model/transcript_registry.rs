//! Transcript 配送の責務を集約し、run ID ごとにイベントを分離する。
//!
//! `MessageDelta` / `ReasoningDelta` は payload の `run_id` が `Some` なら該当 run へ、
//! conversation root の場合だけ thread にも配送する。`run_id` が `None` の delta は
//! 警告して完全に破棄し、Running の run 数にかかわらず配送先を推測しない。
//!
//! Subscriber lag is diagnostic-only: the event bus reports each episode.
//! Transcripts never retain lag residue or insert lag summaries into conversations.

// allow: SIZE_OK - issue #85 keeps routing regressions beside the existing registry tests.
