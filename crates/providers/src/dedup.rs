#![cfg_attr(
    not(test),
    expect(dead_code, reason = "T4 で send_streaming に接続するための先行実装")
)]

/// 論理リクエスト内の部分表示リプレイをチャネルごとに除去する。
///
/// 再生成が表示済み内容と分岐した場合、表示済み長に達してから保留内容を
/// 全量転送し、その試行の残りは無加工で転送する。部分表示には矛盾した文章が
/// 並び得るが、確定履歴には影響しない（T10）。表示の訂正再送は行わない（T17）。
pub(crate) struct ReplayDeduper {
    text: ChannelDedup,
    reasoning: ChannelDedup,
}

struct ChannelDedup {
    emitted: String,
    replay: String,
    passthrough: bool,
}

impl ReplayDeduper {
    pub(crate) const fn new() -> Self {
        Self {
            text: ChannelDedup::new(),
            reasoning: ChannelDedup::new(),
        }
    }

    pub(crate) fn on_new_attempt(&mut self) {
        self.text.on_new_attempt();
        self.reasoning.on_new_attempt();
    }

    pub(crate) fn filter_text(&mut self, delta: &str) -> Option<String> {
        self.text.filter(delta)
    }

    pub(crate) fn filter_reasoning(&mut self, delta: &str) -> Option<String> {
        self.reasoning.filter(delta)
    }
}

impl ChannelDedup {
    const fn new() -> Self {
        Self {
            emitted: String::new(),
            replay: String::new(),
            passthrough: false,
        }
    }

    fn on_new_attempt(&mut self) {
        self.replay.clear();
        self.passthrough = false;
    }

    fn filter(&mut self, delta: &str) -> Option<String> {
        if delta.is_empty() {
            return None;
        }
        if self.passthrough {
            self.emitted.push_str(delta);
            return Some(delta.to_owned());
        }

        self.replay.push_str(delta);
        if self.emitted.starts_with(&self.replay) || self.replay.len() < self.emitted.len() {
            return None;
        }

        let forwarded = match self.replay.strip_prefix(self.emitted.as_str()) {
            Some(excess) => excess.to_owned(),
            None => std::mem::take(&mut self.replay),
        };
        self.replay.clear();
        self.emitted.push_str(&forwarded);
        self.passthrough = true;
        Some(forwarded)
    }
}

#[cfg(test)]
mod tests {
    use super::ReplayDeduper;

    #[test]
    fn exact_replay_is_absorbed() {
        // Given: 初回に Hello を表示済み。
        let mut dedup = ReplayDeduper::new();
        assert_eq!(dedup.filter_text("Hello").as_deref(), Some("Hello"));
        dedup.on_new_attempt();
        // When: 同じ接頭辞から再生成する。
        let replay = dedup.filter_text("Hello");
        let excess = dedup.filter_text(" world");
        // Then: 表示済み部分は吸収し、続きだけ返す。
        assert_eq!(replay, None);
        assert_eq!(excess.as_deref(), Some(" world"));
    }

    #[test]
    fn chunk_misaligned_replay_is_absorbed() {
        // Given: Hello を一つのチャンクで表示済み。
        let mut dedup = ReplayDeduper::new();
        dedup.filter_text("Hello");
        dedup.on_new_attempt();
        // When: 再試行では接頭辞を分割して受信する。
        let results = ["He", "llo", " world"].map(|delta| dedup.filter_text(delta));
        // Then: チャンク境界によらず続きだけ返す。
        assert_eq!(results, [None, None, Some(" world".to_owned())]);
    }

    #[test]
    fn mid_chunk_excess_is_forwarded() {
        // Given: Hello を表示済み。
        let mut dedup = ReplayDeduper::new();
        dedup.filter_text("Hello");
        dedup.on_new_attempt();
        // When: 一つのチャンクが表示済みの末尾を越える。
        let results = ["Hello w", "orld"].map(|delta| dedup.filter_text(delta));
        // Then: 超過部分以降はそのまま返す。
        assert_eq!(results, [Some(" w".to_owned()), Some("orld".to_owned())]);
    }

    #[test]
    fn divergence_flushes_buffer_and_enters_passthrough() {
        // Given: Hello を表示済み。
        let mut dedup = ReplayDeduper::new();
        dedup.filter_text("Hello");
        dedup.on_new_attempt();
        // When: 異なる接頭辞が表示済み長に達する。
        let results = ["Hi", "!!!", "Hello"].map(|delta| dedup.filter_text(delta));
        // Then: 長さ到達まで保留し、全バッファを返した後は吸収しない。
        assert_eq!(
            results,
            [None, Some("Hi!!!".to_owned()), Some("Hello".to_owned())]
        );
    }

    #[test]
    fn text_and_reasoning_are_independent() {
        // Given: テキストと推論に異なる内容を表示済み。
        let mut dedup = ReplayDeduper::new();
        assert_eq!(dedup.filter_text("Hello").as_deref(), Some("Hello"));
        assert_eq!(dedup.filter_reasoning("Think").as_deref(), Some("Think"));
        dedup.on_new_attempt();
        // When: テキストを先に通過モードへ移し、推論は分割再生する。
        let text = dedup.filter_text("Hello!");
        let reasoning = ["Th", "ink?"].map(|delta| dedup.filter_reasoning(delta));
        // Then: チャネルの内容と通過状態は互いに影響しない。
        assert_eq!(text.as_deref(), Some("!"));
        assert_eq!(reasoning, [None, Some("?".to_owned())]);
    }

    #[test]
    fn new_attempt_discards_partial_replay() {
        // Given: 二回目が接頭辞の途中で中断した。
        let mut dedup = ReplayDeduper::new();
        dedup.filter_text("Hello");
        dedup.on_new_attempt();
        dedup.filter_text("He");
        // When: 三回目が最初から再生する。
        dedup.on_new_attempt();
        let results = ["Hello", "!"].map(|delta| dedup.filter_text(delta));
        // Then: 古い再生バッファを捨て、表示済み内容は保持する。
        assert_eq!(results, [None, Some("!".to_owned())]);
    }

    #[test]
    fn new_attempt_keeps_all_forwarded_content() {
        // Given: 二回目で超過部分を表示し通過モードに入った。
        let mut dedup = ReplayDeduper::new();
        dedup.filter_text("Hello");
        dedup.on_new_attempt();
        dedup.filter_text("Hello w");
        dedup.filter_text("orld");
        // When: 三回目で全表示内容を再生する。
        dedup.on_new_attempt();
        let results = ["Hello world", "!"].map(|delta| dedup.filter_text(delta));
        // Then: 通過モードを解除し、試行をまたいだ表示済み部分を吸収する。
        assert_eq!(results, [None, Some("!".to_owned())]);
    }

    #[test]
    fn empty_deltas_leave_state_unchanged() {
        // Given: 初期、通過、接頭辞保留、一致完了の各状態を通る入力。
        let mut dedup = ReplayDeduper::new();
        // When: 各状態で空デルタを挟む。
        let initial = dedup.filter_text("");
        let first = dedup.filter_text("Hello");
        let passthrough = dedup.filter_text("");
        dedup.on_new_attempt();
        let results = ["He", "", "llo", "", "!"].map(|delta| dedup.filter_text(delta));
        // Then: 空デルタは出力も状態変更もなく、パニックもしない。
        assert_eq!(initial, None);
        assert_eq!(first.as_deref(), Some("Hello"));
        assert_eq!(passthrough, None);
        assert_eq!(results, [None, None, None, None, Some("!".to_owned())]);
    }

    #[test]
    fn japanese_replay_slices_only_at_utf8_boundaries() {
        // Given: 複数バイト文字の表示済み接頭辞。
        let mut dedup = ReplayDeduper::new();
        dedup.filter_text("こんにちは");
        dedup.on_new_attempt();
        // When: 分割された再生がチャンクの途中で表示済み末尾を越える。
        let results = ["こん", "にちは世界", "！"].map(|delta| dedup.filter_text(delta));
        // Then: 文字境界で切り出した超過部分を返す。
        assert_eq!(
            results,
            [None, Some("世界".to_owned()), Some("！".to_owned())]
        );
    }
}
