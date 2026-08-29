//! Server-Sent Events (SSE) のインクリメンタル解析を提供します。
//!
//! [`SseParser`] はストリーミング応答を任意のチャンク境界で受け取り、空行で
//! 区切られたフレーム([`SseFrame`])を順に組み立てる push 型パーサーです。
//!
//! # 対応する SSE 仕様のサブセット
//!
//! - 行区切りは LF。CRLF も許容し、行末の `\r` は取り除く。
//! - `field: value` / `field:value` の両形式に対応し、コロンの直後の空白
//!   1 個だけを取り除く(それ以降の空白は保持する)。
//! - `:` で始まる行はコメントとして無視する。
//! - `data:` 行は蓄積され、空行で 1 フレームに確定する(複数行は `\n` で結合)。
//! - `event:` 行はフレームのイベント名を設定する(複数あれば最後のものが有効)。
//! - `id:` / `retry:` などその他のフィールドは無視する。
//! - `data: [DONE]` は特殊扱いせず、通常のフレームとしてそのまま配送する
//!   (解釈は wire 層の責務)。
//!
//! # 配送ポリシー
//!
//! 空行が現れた時点で蓄積 data が空文字列の場合(data 行が一度も現れない、
//! または値が空の `data:` 行のみ)は、フレームを配送せず蓄積状態だけを
//! リセットする。「空データのフレームは配送しない」方針であり、イベント名
//! だけのブロックが後続のフレームへ漏れ出さないことを保証する。
//!
//! # エラーポリシー
//!
//! 完結した行の `data` / `event` フィールドの値が不正な UTF-8 だった場合に
//! [`SseParseError`] を返す。コメント行・フィールド名・未知フィールドに含まれる
//! 不正な UTF-8 はエラーにしない。マルチバイト文字がチャンク境界で分割された
//! 場合でも、行が完結するまで生バイトのままバッファリングするため正しく復元される。

/// SSE の 1 フレーム分のデータ。
///
/// 空行によって 1 フレームが確定し、`data:` 行の内容が `\n` で結合されて
/// [`SseFrame::data`] に入る。`event:` 行が無い場合は [`SseFrame::event`] が `None`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    /// `event:` 行で指定されたイベント名。指定が無ければ `None`。
    pub event: Option<String>,
    /// `data:` 行の内容。複数行は `\n` で結合される。
    pub data: String,
}

/// SSE フレームの解析に失敗したことを表すエラー。
///
/// `crate::error::ProviderError` からは独立した局所的な型であり、
/// 呼び出し側(http 層)がラップして利用する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseParseError {
    /// 失敗の詳細。
    pub detail: String,
}

impl std::fmt::Display for SseParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SSE の解析に失敗しました: {}", self.detail)
    }
}

impl std::error::Error for SseParseError {}

/// SSE のインクリメンタル push 型パーサー。
///
/// [`SseParser::feed`] にネットワークから受け取ったチャンクを任意の区切りで
/// 投入すると、そのチャンクで完結したフレームだけが返る。未完結の入力は
/// 生バイトのまま内部バッファに保持され、行が完結してから初めて UTF-8 として
/// 解釈される。ストリーム終端では [`SseParser::finish`] を呼び出す。
pub struct SseParser {
    /// 改行でまだ区切られていない生バイト列。
    buffer: Vec<u8>,
    /// 蓄積中のフレームのイベント名(`event:` 行の最後のものが有効)。
    event: Option<String>,
    /// 蓄積中の `data:` 行。空行で 1 フレームとして確定する。
    data_lines: Vec<String>,
}

impl SseParser {
    /// 空のパーサーを生成します。
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            event: None,
            data_lines: Vec::new(),
        }
    }

    /// チャンクを投入し、このチャンクで完結したフレームをすべて返します。
    ///
    /// 不完全な入力は内部バッファに保持され、次回以降の [`SseParser::feed`] や
    /// [`SseParser::finish`] で処理されます。空のチャンクは何も完了させません。
    ///
    /// # Errors
    /// 完結した行の `data` / `event` フィールドの値が不正な UTF-8 だった場合に
    /// [`SseParseError`] を返します。エラー時、当該の行はバッファから取り除かれた
    /// 状態になります。
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<SseFrame>, SseParseError> {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();
        while let Some(pos) = self.buffer.iter().position(|&b| b == b'\n') {
            // \n 込みで取り出し、行本体だけを処理に渡す
            let line: Vec<u8> = self.buffer.drain(..=pos).collect();
            self.process_line(&line[..line.len() - 1], &mut frames)?;
        }
        Ok(frames)
    }

    /// ストリームの終端を通知し、未確定の残りを最終フレームとして返します。
    ///
    /// 末尾が空行で終端していないフレームもここで配送されます。残りが無ければ
    /// 空の `Vec` を返します。呼び出し後もパーサーはリセット済みの状態で再利用できます。
    ///
    /// # Errors
    /// 残りのバッファの `data` / `event` フィールドの値が不正な UTF-8 だった場合に
    /// [`SseParseError`] を返します。
    pub fn finish(&mut self) -> Result<Vec<SseFrame>, SseParseError> {
        let mut frames = Vec::new();
        // 改行で終端していない残りを最終行として処理する
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            self.process_line(&line, &mut frames)?;
        }
        if let Some(frame) = self.take_pending_frame() {
            frames.push(frame);
        }
        Ok(frames)
    }

    /// 1 行分のバイト列(`\n` を除く)を処理し、確定したフレームを `frames` に追加します。
    fn process_line(
        &mut self,
        line: &[u8],
        frames: &mut Vec<SseFrame>,
    ) -> Result<(), SseParseError> {
        // CRLF を許容するため、行末の \r を取り除く
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        // 空行はフレーム確定の合図
        if line.is_empty() {
            if let Some(frame) = self.take_pending_frame() {
                frames.push(frame);
            }
            return Ok(());
        }
        // `:` で始まる行はコメント
        if line.starts_with(b":") {
            return Ok(());
        }
        // `field:value` に分割(コロンの無い行は空値のフィールドとして扱う)
        let (field, value) = match line.iter().position(|&b| b == b':') {
            Some(pos) => (&line[..pos], &line[pos + 1..]),
            None => (line, [].as_slice()),
        };
        // コロンの直後の空白 1 個だけを取り除き、それ以降は保持する
        let value = value.strip_prefix(b" ").unwrap_or(value);
        match field {
            b"data" => {
                let value = std::str::from_utf8(value).map_err(|e| SseParseError {
                    detail: format!("不正な UTF-8 を含む data 行です: {e}"),
                })?;
                self.data_lines.push(value.to_string());
            }
            b"event" => {
                let value = std::str::from_utf8(value).map_err(|e| SseParseError {
                    detail: format!("不正な UTF-8 を含む event 行です: {e}"),
                })?;
                self.event = Some(value.to_string());
            }
            // id:, retry: など未知のフィールドは無視する
            _ => {}
        }
        Ok(())
    }

    /// 蓄積中の data から 1 フレームを組み立てて取り出します。
    ///
    /// 蓄積 data が空文字列の場合(data 行が無い、または値が空の `data:` 行のみ)は
    /// `None` を返し、イベント名を含む蓄積状態をリセットします。
    fn take_pending_frame(&mut self) -> Option<SseFrame> {
        let data = self.data_lines.join("\n");
        self.data_lines.clear();
        if data.is_empty() {
            self.event = None;
            return None;
        }
        Some(SseFrame {
            event: self.event.take(),
            data,
        })
    }
}

impl Default for SseParser {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 複数フレームを含むサンプルストリーム。
    /// コメント・CRLF・未知フィールド・マルチバイト UTF-8・[DONE] を含む。
    const SAMPLE: &str = "event: message_start\ndata: {\"type\":\"message_start\"}\r\n\n\
                          : keep-alive\n\n\
                          id: 42\ndata: こんにちは\ndata: 世界\n\n\
                          data: [DONE]\n\n";

    /// SAMPLE を正しく解析した場合の期待フレーム列。
    fn expected_sample_frames() -> Vec<SseFrame> {
        vec![
            SseFrame {
                event: Some("message_start".to_string()),
                data: "{\"type\":\"message_start\"}".to_string(),
            },
            SseFrame {
                event: None,
                data: "こんにちは\n世界".to_string(),
            },
            SseFrame {
                event: None,
                data: "[DONE]".to_string(),
            },
        ]
    }

    /// チャンク列を順に feed し、最後に finish した結果を 1 つの列にして返す。
    fn parse_all(chunks: &[&[u8]]) -> Vec<SseFrame> {
        let mut parser = SseParser::new();
        let mut frames = Vec::new();
        for chunk in chunks {
            frames.extend(parser.feed(chunk).unwrap());
        }
        frames.extend(parser.finish().unwrap());
        frames
    }

    #[test]
    fn single_data_line_frame() {
        // Given: 単純な 1 フレーム分の入力
        let mut parser = SseParser::new();
        // When: 一括で feed する
        let frames = parser.feed(b"data: hello\n\n").unwrap();
        // Then: event 無し・data "hello" のフレームが 1 つ得られる
        assert_eq!(
            frames,
            vec![SseFrame {
                event: None,
                data: "hello".to_string()
            }]
        );
    }

    #[test]
    fn event_named_frame_anthropic_style() {
        // Given: Anthropic 風の event: 行と data: 行
        let mut parser = SseParser::new();
        // When: feed する
        let frames = parser
            .feed(b"event: message_start\ndata: {\"ok\":true}\n\n")
            .unwrap();
        // Then: イベント名付きフレームが 1 つ得られる
        assert_eq!(
            frames,
            vec![SseFrame {
                event: Some("message_start".to_string()),
                data: "{\"ok\":true}".to_string(),
            }]
        );
    }

    #[test]
    fn multi_line_data_joined_with_newline() {
        // Given: 連続する 3 つの data 行
        let mut parser = SseParser::new();
        // When: feed する
        let frames = parser.feed(b"data: a\ndata: b\ndata: c\n\n").unwrap();
        // Then: data が \n で結合された 1 フレームになる
        assert_eq!(
            frames,
            vec![SseFrame {
                event: None,
                data: "a\nb\nc".to_string()
            }]
        );
    }

    #[test]
    fn comment_lines_are_ignored() {
        // Given: コメント行と data 行を含む入力
        let mut parser = SseParser::new();
        // When: feed する
        let frames = parser.feed(b": ping\n\n: note\ndata: x\n\n").unwrap();
        // Then: コメントのみのブロックは配送されず、data フレームだけ得られる
        assert_eq!(
            frames,
            vec![SseFrame {
                event: None,
                data: "x".to_string()
            }]
        );
    }

    #[test]
    fn crlf_line_endings_tolerated() {
        // Given: CRLF 改行の 2 フレーム分の入力
        let mut parser = SseParser::new();
        // When: feed する
        let frames = parser.feed(b"data: a\r\n\r\ndata: b\r\n\r\n").unwrap();
        // Then: \r は取り除かれ、2 フレームが得られる
        assert_eq!(
            frames,
            vec![
                SseFrame {
                    event: None,
                    data: "a".to_string()
                },
                SseFrame {
                    event: None,
                    data: "b".to_string()
                },
            ]
        );
    }

    #[test]
    fn colon_space_handling() {
        // Given: コロン直後に空白が無い行と空白が 2 つある行
        let mut parser = SseParser::new();
        // When: feed する
        let frames = parser.feed(b"data:value\n\ndata:  two spaces\n\n").unwrap();
        // Then: 先頭の空白 1 個だけが取り除かれ、2 個目以降は保持される
        assert_eq!(
            frames,
            vec![
                SseFrame {
                    event: None,
                    data: "value".to_string()
                },
                SseFrame {
                    event: None,
                    data: " two spaces".to_string()
                },
            ]
        );
    }

    #[test]
    fn unknown_fields_are_ignored() {
        // Given: id: / retry: / 未知フィールドを含む入力
        let mut parser = SseParser::new();
        // When: feed する
        let frames = parser
            .feed(b"id: 42\nretry: 1000\nfoo:bar\ndata: x\n\n")
            .unwrap();
        // Then: data だけが反映されたフレームになる
        assert_eq!(
            frames,
            vec![SseFrame {
                event: None,
                data: "x".to_string()
            }]
        );
    }

    #[test]
    fn last_event_field_wins() {
        // Given: event 行が 2 回現れる入力
        let mut parser = SseParser::new();
        // When: feed する
        let frames = parser
            .feed(b"event: first\nevent: second\ndata: x\n\n")
            .unwrap();
        // Then: 最後の event 名が使われる
        assert_eq!(
            frames,
            vec![SseFrame {
                event: Some("second".to_string()),
                data: "x".to_string(),
            }]
        );
    }

    #[test]
    fn event_without_data_is_not_dispatched_and_resets() {
        // Given: data を伴わない event 行と、その後の data 行
        let mut parser = SseParser::new();
        // When: 順に feed する
        let frames = parser.feed(b"event: ping\n\ndata: x\n\n").unwrap();
        // Then: event 単独のブロックは配送されず、リセットにより次のフレームは event 無し
        assert_eq!(
            frames,
            vec![SseFrame {
                event: None,
                data: "x".to_string()
            }]
        );
    }

    #[test]
    fn frame_completes_on_blank_line() {
        // Given: 空行で終端していない data 行を含む 2 チャンクの入力
        let mut parser = SseParser::new();
        // When: 空行の前まで feed する
        let before = parser.feed(b"data: a\n").unwrap();
        // Then: まだフレームは返らない
        assert!(before.is_empty());
        // When: 空行を feed する
        let after = parser.feed(b"\n").unwrap();
        // Then: フレームが確定して返る
        assert_eq!(
            after,
            vec![SseFrame {
                event: None,
                data: "a".to_string()
            }]
        );
    }

    #[test]
    fn multiple_frames_in_one_chunk() {
        // Given: 1 チャンクに 2 フレーム分の入力
        let mut parser = SseParser::new();
        // When: feed する
        let frames = parser.feed(b"data: 1\n\ndata: 2\n\n").unwrap();
        // Then: 2 フレームが順に返る
        assert_eq!(
            frames,
            vec![
                SseFrame {
                    event: None,
                    data: "1".to_string()
                },
                SseFrame {
                    event: None,
                    data: "2".to_string()
                },
            ]
        );
    }

    #[test]
    fn sample_stream_parses_in_one_feed() {
        // Given: 複数フレームのサンプルストリーム
        // When: 一括で feed する
        let frames = parse_all(&[SAMPLE.as_bytes()]);
        // Then: 期待どおりの 3 フレーム列になる
        assert_eq!(frames, expected_sample_frames());
    }

    #[test]
    fn frames_are_identical_when_split_at_every_byte_boundary() {
        // Given: サンプルストリームと、その全分割位置
        let bytes = SAMPLE.as_bytes();
        let expected = expected_sample_frames();
        for split in 0..=bytes.len() {
            // When: split 位置で 2 つに分けて feed する
            let frames = parse_all(&[&bytes[..split], &bytes[split..]]);
            // Then: 一括投入と同じフレーム列になる
            assert_eq!(frames, expected, "split at {split}");
        }
    }

    #[test]
    fn frames_are_identical_when_fed_byte_by_byte() {
        // Given: サンプルストリームの各バイト
        let bytes = SAMPLE.as_bytes();
        let chunks: Vec<&[u8]> = bytes.iter().map(std::slice::from_ref).collect();
        // When: 1 バイトずつ feed する
        let frames = parse_all(&chunks);
        // Then: 一括投入と同じフレーム列になる
        assert_eq!(frames, expected_sample_frames());
    }

    #[test]
    fn finish_flushes_trailing_frame_without_blank_line() {
        // Given: 末尾が空行で終端していない入力
        let mut parser = SseParser::new();
        parser.feed(b"data: hello\ndata: world\n").unwrap();
        // When: finish する
        let frames = parser.finish().unwrap();
        // Then: 残った data が最終フレームとして配送される
        assert_eq!(
            frames,
            vec![SseFrame {
                event: None,
                data: "hello\nworld".to_string()
            }]
        );
    }

    #[test]
    fn finish_flushes_partial_line_without_newline() {
        // Given: 改行すら無い途中の data 行
        let mut parser = SseParser::new();
        parser.feed(b"data: hel").unwrap();
        // When: finish する
        let frames = parser.finish().unwrap();
        // Then: バッファの残りが最終行として扱われる
        assert_eq!(
            frames,
            vec![SseFrame {
                event: None,
                data: "hel".to_string()
            }]
        );
    }

    #[test]
    fn finish_returns_empty_when_stream_already_complete() {
        // Given: 空行で完結済みのストリーム
        let mut parser = SseParser::new();
        parser.feed(b"data: a\n\n").unwrap();
        // When: finish する
        let frames = parser.finish().unwrap();
        // Then: 空の列が返る
        assert!(frames.is_empty());
    }

    #[test]
    fn finish_on_fresh_parser_returns_empty() {
        // Given: 何も投入していないパーサー
        let mut parser = SseParser::new();
        // When: finish する
        let frames = parser.finish().unwrap();
        // Then: 空の列が返る
        assert!(frames.is_empty());
    }

    #[test]
    fn multibyte_utf8_split_across_chunks() {
        // Given: マルチバイト文字「あ」をバイト境界で 3 つに分割した data 行
        let mut parser = SseParser::new();
        // When: 分割したチャンクを順に feed する
        parser.feed(b"data: \xe3").unwrap();
        parser.feed(b"\x81").unwrap();
        let frames = parser.feed(b"\x82\n\n").unwrap();
        // Then: 行が完結した時点で正しく 1 文字に復元される
        assert_eq!(
            frames,
            vec![SseFrame {
                event: None,
                data: "あ".to_string()
            }]
        );
    }

    #[test]
    fn invalid_utf8_in_data_line_is_error() {
        // Given: 不正な UTF-8 バイトを含む完結した data 行
        let mut parser = SseParser::new();
        // When: feed する
        let result = parser.feed(b"data: \xff\xfe\n\n");
        // Then: エラーになる
        assert!(result.is_err());
    }

    #[test]
    fn invalid_utf8_in_event_line_is_error() {
        // Given: 不正な UTF-8 バイトを含む完結した event 行
        let mut parser = SseParser::new();
        // When: feed する
        let result = parser.feed(b"event: \xff\n\n");
        // Then: エラーになる(String として表現できないため)
        assert!(result.is_err());
    }

    #[test]
    fn invalid_utf8_outside_data_value_is_tolerated() {
        // Given: コメント行と未知フィールドに不正な UTF-8 を含む入力
        let mut parser = SseParser::new();
        // When: feed する
        let frames = parser.feed(b": \xff\nbroken\xff: x\ndata: ok\n\n").unwrap();
        // Then: エラーにならず data だけが配送される
        assert_eq!(
            frames,
            vec![SseFrame {
                event: None,
                data: "ok".to_string()
            }]
        );
    }

    #[test]
    fn done_marker_passes_through_as_normal_frame() {
        // Given: data: [DONE] 行
        let mut parser = SseParser::new();
        // When: feed する
        let frames = parser.feed(b"data: [DONE]\n\n").unwrap();
        // Then: 特別扱いされず通常のフレームとして配送される
        assert_eq!(
            frames,
            vec![SseFrame {
                event: None,
                data: "[DONE]".to_string()
            }]
        );
    }

    #[test]
    fn empty_data_value_is_not_dispatched() {
        // Given: 値が空の data 行のみのブロックと、その後の通常フレーム
        let mut parser = SseParser::new();
        // When: 順に feed する
        let frames = parser.feed(b"data:\n\ndata: x\n\n").unwrap();
        // Then: 空データのフレームは配送されず、リセット後に次のフレームだけ得られる
        assert_eq!(
            frames,
            vec![SseFrame {
                event: None,
                data: "x".to_string()
            }]
        );
    }
}
