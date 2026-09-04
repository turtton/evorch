//! entry pre-routing: ユーザーメッセージ到着時に ExecutionShape を事前判定する (issue #71)。

mod keyword;

pub use keyword::{
    COORDINATION_KEYWORDS, DIRECT_KEYWORDS, LocalVerdict, UncertainReason, classify_local,
};
