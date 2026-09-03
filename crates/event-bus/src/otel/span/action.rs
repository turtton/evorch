//! span action DTO 型を定義するモジュールです。
//!
//! [`SpanMapper`](super::SpanMapper) が [`Event`](crate::event::Event) を
//! 写像した結果の action 列と、typed drop 記録の型。telemetry SDK 等の
//! 外部 crate には依存しない (feature 非依存制約)。

use std::time::SystemTime;

/// span を一意に識別する key。
///
/// 語彙は `run:{run_id}` / `agent:{run_id}` / `request:{request_id}` /
/// `tool:{call_id}` / `session:{session_id}`。run span と agent span は
/// 同一 `run_id` 空間に同居するため variant で区別する。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SpanKey {
    /// agent run の寿命 span (`run:{run_id}`)。
    Run {
        /// run の ID。
        run_id: String,
    },
    /// agent 呼び出し span (`agent:{run_id}`)。
    Agent {
        /// run の ID。
        run_id: String,
    },
    /// provider リクエスト attempt span (`request:{request_id}`)。
    Request {
        /// attempt 相関用の request ID。
        request_id: String,
    },
    /// ツール実行 span (`tool:{call_id}`)。
    Tool {
        /// ツール呼び出しの ID。
        call_id: String,
    },
    /// セッション span (`session:{session_id}`)。
    Session {
        /// セッションの ID。
        session_id: String,
    },
}

/// span 属性値の閉型。
///
/// 有限値のみを表現する。[`SpanAttributeValue::f64_finite`] 以外の経路では
/// `F64` を構築できないため、NaN / Infinity は型レベルで排除される。
#[derive(Debug, Clone, PartialEq)]
pub enum SpanAttributeValue {
    /// 文字列値。
    Str(String),
    /// 文字列配列値 (例: `gen_ai.response.finish_reasons` の string array)。
    Strings(Vec<String>),
    /// 整数値。
    I64(i64),
    /// 有限浮動小数点値。
    F64(FiniteF64),
    /// 真偽値。
    Bool(bool),
}

impl SpanAttributeValue {
    /// 有限な `f64` 値のみを受け入れる constructor。
    ///
    /// NaN / ±Infinity は `None` となる (属性値への非有限値混入を拒否)。
    pub fn f64_finite(value: f64) -> Option<Self> {
        FiniteF64::new(value).map(Self::F64)
    }
}

/// NaN / ±Infinity を表現できない浮動小数点属性値。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FiniteF64(f64);

impl FiniteF64 {
    /// 有限値のみを構築する。
    pub fn new(value: f64) -> Option<Self> {
        value.is_finite().then_some(Self(value))
    }

    /// 内部の有限値を返す。
    pub const fn get(self) -> f64 {
        self.0
    }
}

impl From<&str> for SpanAttributeValue {
    fn from(value: &str) -> Self {
        Self::Str(value.to_owned())
    }
}

impl From<String> for SpanAttributeValue {
    fn from(value: String) -> Self {
        Self::Str(value)
    }
}

impl From<Vec<String>> for SpanAttributeValue {
    fn from(value: Vec<String>) -> Self {
        Self::Strings(value)
    }
}

impl From<i64> for SpanAttributeValue {
    fn from(value: i64) -> Self {
        Self::I64(value)
    }
}

impl From<bool> for SpanAttributeValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

/// span 属性の key-value 対。
#[derive(Debug, Clone, PartialEq)]
pub struct SpanAttribute {
    /// semconv または evorch 拡張の属性キー。
    pub key: String,
    /// 属性値。
    pub value: SpanAttributeValue,
}

impl SpanAttribute {
    /// 属性を生成する。
    pub fn new(key: &'static str, value: impl Into<SpanAttributeValue>) -> Self {
        Self {
            key: key.to_owned(),
            value: value.into(),
        }
    }

    /// 有限 `f64` 値のみを受け入れる constructor。
    ///
    /// NaN / ±Infinity は `None` となる。
    pub fn finite_f64(key: &'static str, value: f64) -> Option<Self> {
        SpanAttributeValue::f64_finite(value).map(|value| Self {
            key: key.to_owned(),
            value,
        })
    }
}

/// span 種別を表すローカル enum。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanKind {
    /// 外部システムの呼び出し側 (`Client`)。
    Client,
    /// プロセス内部処理 (`Internal`)。
    Internal,
}

/// span 終了ステータスを表すローカル enum。
///
/// `Ok` は使用しない (成功は `Unset` で表現する)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanStatus {
    /// ステータス未設定 (成功終了)。
    Unset,
    /// エラー終了。
    Error,
}

/// mapper が 1 イベントから生成する span 操作。
///
/// `attributes` / `final_attributes` は意図的に [`Vec`] (生成順固定) であり、
/// golden テストの順序安定性を保つ (`HashMap` は使用しない)。
#[derive(Debug, Clone, PartialEq)]
pub enum SpanAction {
    /// span を開始する。
    Start {
        /// 開始する span の key。
        key: SpanKey,
        /// 親 span の key。root span では `None`。
        parent: Option<SpanKey>,
        /// span 名。
        name: String,
        /// span 種別。
        kind: SpanKind,
        /// 開始時刻 (元イベントの timestamp)。
        start_time: SystemTime,
        /// 生成順固定の開始属性リスト。
        attributes: Vec<SpanAttribute>,
    },
    /// span を終了する。
    ///
    /// `final_attributes` は閉鎖時点の属性**完全集合** (開始属性 +
    /// in-flight 追加属性 + 終端固有属性) である。
    End {
        /// 終了する span の key。
        key: SpanKey,
        /// 終了時刻 (元イベントの timestamp)。
        end_time: SystemTime,
        /// 終了ステータス。
        status: SpanStatus,
        /// 生成順固定の最終属性リスト。
        final_attributes: Vec<SpanAttribute>,
    },
}

/// mapper が span action に写像せず破棄した事象の分類。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SpanDropKind {
    /// 相関先 run ID が欠落していた (`run_id: None` の request / tool 開始)。
    MissingRunId,
    /// 親 `agent:{run_id}` が mapper 状態に存在しなかった。
    UnknownParent,
    /// 開始済み span が存在しない End 要求だった。
    UnknownSpanEnd,
    /// 同一 key の span が既に open だった。
    DuplicateSpan,
    /// 所属 run の sampling 判定が sampled-out だった (descendant も含む)。
    SampledOut,
    /// run 単位の in-flight 上限超過で Start を拒否した。
    BudgetInFlightPerRun,
    /// 全体の in-flight 上限超過で Start を拒否した。
    BudgetInFlightGlobal,
    /// 許可 window 内の admission 数上限超過で Start を拒否した。
    BudgetWindow,
    /// 属性上限 (数 / per-span bytes / per-value bytes / whitelist 防御) で
    /// 属性を破棄した。
    BudgetAttributes,
    /// `max_span_lifetime` 超過で open span を強制閉鎖した。
    BudgetEvicted,
}

/// mapper が破棄した事象 1 件分の typed 記録。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanDrop {
    /// 破棄分類。
    pub kind: SpanDropKind,
    /// 破棄対象となった span の key。
    pub key: SpanKey,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: NaN / +Infinity / -Infinity。
    // When: SpanAttributeValue::f64_finite で構築する。
    // Then: いずれも None となる。
    #[test]
    fn f64_finite_rejects_non_finite_values() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(SpanAttributeValue::f64_finite(value), None, "value={value}");
            assert!(
                SpanAttribute::finite_f64("test.key", value).is_none(),
                "value={value}"
            );
        }
    }

    // Given: 有限な f64 値。
    // When: SpanAttributeValue::f64_finite で構築する。
    // Then: 同値の F64 variant が得られる。
    #[test]
    fn f64_finite_accepts_finite_values() {
        assert_eq!(
            SpanAttributeValue::f64_finite(1.5),
            FiniteF64::new(1.5).map(SpanAttributeValue::F64)
        );
        assert_eq!(
            SpanAttribute::finite_f64("test.key", -0.25),
            Some(SpanAttribute {
                key: "test.key".to_owned(),
                value: SpanAttributeValue::F64(FiniteF64::new(-0.25).expect("finite fixture")),
            })
        );
    }
}
