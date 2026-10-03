//! GUI の Diff タブ向け読み取り専用差分モデル。

mod fixture;
mod git_cli;
pub(crate) mod presentation;
mod refresh;

pub use refresh::{AUTO_REFRESH_INTERVAL, DiffModel};

use std::path::PathBuf;

pub use fixture::FixtureDiffSource;
pub use git_cli::GitCliDiffSource;

/// GUI に保持する差分テキストの最大バイト数。
pub const DIFF_BYTE_CAP: usize = 256 * 1024;

/// 取得する差分の範囲。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffMode {
    /// index と working tree の差分。
    WorkingTree,
    /// 固定 base branch (`main`) の merge base から現在の HEAD までの差分。
    ///
    /// base branch 選択は API に公開せず、unit variant で任意 base を
    /// 型レベルで表現不可能にする (issue #65 AC11)。
    Branch,
}

/// 差分取得要求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRequest {
    /// Git リポジトリのルート。
    pub repo_root: PathBuf,
    /// 取得する差分の範囲。
    pub mode: DiffMode,
}

/// Diff タブが表示する取得状態。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffState {
    /// 未取得。
    Idle,
    /// worker thread で取得中。
    Loading,
    /// 差分なし。
    Empty,
    /// 上限内の差分。
    Ready { text: String },
    /// 表示上限で切り詰めた差分。
    Truncated {
        text: String,
        total_bytes: usize,
        cap: usize,
    },
    /// 取得失敗。
    Error { message: String },
}

/// 差分取得時の失敗。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DiffError {
    /// Git がエラー終了した。
    #[error("git diff failed: {stderr}")]
    Git { stderr: String },
    /// Git 出力を表示文字列へ変換できなかった。
    #[error("diff output I/O error: {0}")]
    Io(String),
    /// Git process を起動できなかった。
    #[error("failed to spawn git: {0}")]
    Spawn(String),
}

/// 差分テキストの供給元。
pub trait DiffSource: Send + Sync {
    /// 要求に対応する差分を取得する。
    ///
    /// # Errors
    /// Git の実行、出力変換、または process 起動に失敗した場合は [`DiffError`] を返す。
    fn fetch(&self, req: &DiffRequest) -> Result<String, DiffError>;
}

fn state_from_result(result: Result<String, DiffError>) -> DiffState {
    match result {
        Ok(text) if text.trim().is_empty() => DiffState::Empty,
        Ok(text) if text.len() > DIFF_BYTE_CAP => {
            let total_bytes = text.len();
            let mut boundary = DIFF_BYTE_CAP;
            while !text.is_char_boundary(boundary) {
                boundary -= 1;
            }
            DiffState::Truncated {
                text: text[..boundary].to_string(),
                total_bytes,
                cap: DIFF_BYTE_CAP,
            }
        }
        Ok(text) => DiffState::Ready { text },
        Err(error) => DiffState::Error {
            message: error.to_string(),
        },
    }
}
