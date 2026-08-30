//! SSE バイトストリームを canonical 差分イベント列へ変換するアダプタ。
//!
//! reqwest のレスポンス本文バイト列を [`SseParser`] へ投入し、確定した
//! フレームを wire 層のインタプリタ ([`WireStreamInterpreter`]) へ渡し、
//! 解釈結果を [`StreamEvent`] 列として流す。完了シグナルを受け取った
//! 時点で [`StreamAccumulator`] を確定させ、usage を発行し、
//! [`StreamEvent::Completed`] を最後のイベントとして流す。

use std::collections::VecDeque;
use std::pin::Pin;

use bytes::Bytes;
use futures_core::Stream;
use futures_util::StreamExt;

use super::UsageEmitter;
use super::map_request_error;
use crate::error::ProviderError;
use crate::message::{FinishReason, Usage};
use crate::observe::AttemptObserver;
use crate::sse::{SseFrame, SseParseError, SseParser};
use crate::stream::{DeltaStream, StreamAccumulator, StreamEvent};

/// reqwest が返すレスポンス本文のバイトストリーム。
type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

/// 1 フレーム (または終端) の wire 解釈結果。
#[derive(Debug, Default)]
pub(crate) struct FrameInterpretation {
    /// ストリームへ投下する canonical イベント列。
    pub events: Vec<StreamEvent>,
    /// ストリーム完了シグナル。完了を意味するフレーム (`[DONE]` /
    /// `message_stop` 等) を解釈した時点で、それまでに蓄積した usage と
    /// 終了理由を伴って `Some` になる。完了フレーム以前に受け取った
    /// usage はインタプリタが内部状態として保持する。
    pub completion: Option<(Usage, FinishReason)>,
}

/// wire 形式の SSE フレームを canonical イベント列へ解釈するインタプリタ。
///
/// 各プロバイダの wire モジュールがこのトレイトを実装し
/// [`adapt_sse_stream`] へ渡す。完了シグナルは状態照会メソッドではなく
/// 解釈結果 ([`FrameInterpretation::completion`]) の一部として返す。
/// これにより「どのフレームが完了を引き起こしたか」という対応が 1 つの
/// 戻り値に閉じ、実装側は完了判定の状態照会タイミングを気にせず済む。
pub(crate) trait WireStreamInterpreter: Send {
    /// 1 フレームを解釈する。
    ///
    /// # Errors
    /// フレームの内容が wire 形式として不正な場合 [`ProviderError`] を返す。
    fn interpret(&mut self, frame: SseFrame) -> Result<FrameInterpretation, ProviderError>;

    /// 入力終端処理。パーサーの残りフレーム処理が終わった後に 1 回だけ
    /// 呼ばれ、インタプリタ内部に残った状態を解釈する。完了シグナルを
    /// 終端で出す実装 (入力終了をもって完了とみなす等) はここで
    /// `completion: Some` を返す。
    ///
    /// # Errors
    /// 終端状態が wire 形式として不正な場合 [`ProviderError`] を返す。
    fn finish(&mut self) -> Result<FrameInterpretation, ProviderError>;
}

/// SSE バイト列を canonical イベント列へ変換するポンプ。
///
/// バイトチャンクを [`SseParser`] へ投入し、確定フレームをインタプリタへ
/// 渡して解釈結果をキューイングする。完了シグナル受理時には
/// アキュムレータを確定させて usage を発行し、[`StreamEvent::Completed`]
/// をキューイングして以降の入力を無視する。
pub(crate) struct SsePump<I> {
    parser: SseParser,
    interpreter: I,
    accumulator: StreamAccumulator,
    usage: UsageEmitter,
    model: String,
    observer: AttemptObserver,
    pending: VecDeque<Result<StreamEvent, ProviderError>>,
    done: bool,
}

impl<I: WireStreamInterpreter> SsePump<I> {
    fn new(interpreter: I, usage: UsageEmitter, model: String, observer: AttemptObserver) -> Self {
        Self {
            parser: SseParser::new(),
            interpreter,
            accumulator: StreamAccumulator::default(),
            usage,
            model,
            observer,
            pending: VecDeque::new(),
            done: false,
        }
    }

    /// キュー済みイベントを 1 つ取り出す。
    fn pop_pending(&mut self) -> Option<Result<StreamEvent, ProviderError>> {
        self.pending.pop_front()
    }

    /// 完了または致命的エラーにより入力の消化を止めたかどうか。
    fn is_done(&self) -> bool {
        self.done
    }

    /// バイトチャンクを消化する。
    fn push_chunk(&mut self, chunk: &[u8]) {
        match self.parser.feed(chunk) {
            Ok(frames) => self.absorb(frames),
            Err(err) => self.fail_with_sse_error(err),
        }
    }

    /// 入力終端を消化する。パーサーの残りを処理してから
    /// インタプリタの終端処理を行い、以降の入力読み込みを止める。
    fn finish_tail(&mut self) {
        match self.parser.finish() {
            Ok(frames) => self.absorb(frames),
            Err(err) => {
                self.fail_with_sse_error(err);
                return;
            }
        }
        if !self.done {
            match self.interpreter.finish() {
                Ok(interpretation) => self.absorb_interpretation(interpretation),
                Err(err) => {
                    self.observer.emit_failed(&err);
                    self.pending.push_back(Err(err));
                }
            }
        }
        // 完了シグナル無しの入力終了は、canonical イベント列には Err を流さず
        // 従来どおり静かに終える (DeltaStream の契約は変更しない) が、attempt
        // 観測上は中途 EOF として Transport 失敗を 1 件発行する。
        if !self.done {
            let error =
                ProviderError::Request("stream ended without completion signal".to_string());
            self.observer.emit_failed(&error);
        }
        self.done = true;
    }

    /// トランスポート層エラーをキューイングし、入力の消化を止める。
    fn push_transport_error(&mut self, err: ProviderError) {
        self.observer.emit_failed(&err);
        self.pending.push_back(Err(err));
        self.done = true;
    }

    /// 確定フレーム列を順にインタプリタへ渡す。
    ///
    /// 完了シグナルが現れた時点で残りのフレームは破棄する
    /// (完了後のフレームに意味はないため)。
    fn absorb(&mut self, frames: Vec<SseFrame>) {
        for frame in frames {
            match self.interpreter.interpret(frame) {
                Ok(interpretation) => {
                    self.absorb_interpretation(interpretation);
                    if self.done {
                        return;
                    }
                }
                Err(err) => {
                    self.observer.emit_failed(&err);
                    self.pending.push_back(Err(err));
                    self.done = true;
                    return;
                }
            }
        }
    }

    /// 解釈結果をキューイングし、完了シグナルがあれば最終応答を組み立てる。
    ///
    /// 完了時はアキュムレータを確定させて usage を 1 回だけ発行し、
    /// [`StreamEvent::Completed`] を最後にキューイングする。
    fn absorb_interpretation(&mut self, interpretation: FrameInterpretation) {
        for event in interpretation.events {
            self.observer.note_delta(&event);
            self.accumulator.feed(&event);
            self.pending.push_back(Ok(event));
        }
        if let Some((usage, finish_reason)) = interpretation.completion {
            let accumulator = std::mem::take(&mut self.accumulator);
            let response = accumulator.finish(usage, finish_reason);
            self.usage.emit_usage(&self.model, &response.usage);
            self.observer
                .emit_completed(&response.usage, response.finish_reason.clone());
            self.pending
                .push_back(Ok(StreamEvent::Completed { response }));
            self.done = true;
        }
    }

    /// SSE 解析エラーを [`ProviderError::InvalidSse`] としてキューイングし、
    /// 入力の消化を止める。
    fn fail_with_sse_error(&mut self, err: SseParseError) {
        let error = ProviderError::InvalidSse { detail: err.detail };
        self.observer.emit_failed(&error);
        self.pending.push_back(Err(error));
        self.done = true;
    }
}

/// reqwest のレスポンス本文バイトストリームを [`DeltaStream`] へ変換する。
///
/// バイトチャンクは任意の境界で [`SseParser`] へ投入され、確定フレームが
/// `interpreter` へ渡される。`interpreter` が完了シグナルを返した時点で
/// アキュムレータを確定させ、`usage` をちょうど 1 回発行して
/// [`StreamEvent::Completed`] を流す。バス未設定の [`UsageEmitter`]
/// を渡せば発行は no-op になる。完了シグナルが来ないまま入力が終了した
/// 場合は `Completed` を流さずにストリームを終える。この中途 EOF は
/// canonical 契約上は静かな終了だが、attempt 観測上は Transport の
/// [`event_bus::ProviderEvent::RequestFailed`] として発行される。
#[allow(dead_code)] // TODO(T5/T6): provider 実装が利用するまでの一時許可
pub(crate) fn adapt_sse_stream<I>(
    byte_stream: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    interpreter: I,
    usage: UsageEmitter,
    model: String,
    observer: AttemptObserver,
) -> DeltaStream
where
    I: WireStreamInterpreter + 'static,
{
    let byte_stream: ByteStream = Box::pin(byte_stream);
    Box::pin(futures_util::stream::unfold(
        (
            SsePump::new(interpreter, usage, model, observer),
            byte_stream,
        ),
        |(mut pump, mut byte_stream)| async move {
            loop {
                if let Some(event) = pump.pop_pending() {
                    return Some((event, (pump, byte_stream)));
                }
                if pump.is_done() {
                    return None;
                }
                match byte_stream.next().await {
                    Some(Ok(chunk)) => pump.push_chunk(&chunk),
                    Some(Err(err)) => pump.push_transport_error(map_request_error(err)),
                    None => pump.finish_tail(),
                }
            }
        },
    ))
}

#[cfg(test)]
mod tests;
