//! GUI の Diff タブ向け読み取り専用差分モデル。

mod fixture;
mod git_cli;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};

pub use fixture::FixtureDiffSource;
pub use git_cli::GitCliDiffSource;

/// GUI に保持する差分テキストの最大バイト数。
pub const DIFF_BYTE_CAP: usize = 256 * 1024;

/// 取得する差分の範囲。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffMode {
    /// index と working tree の差分。
    WorkingTree,
    /// merge base から現在の HEAD までの差分。
    Branch { base: String },
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

/// UI frame から非同期 worker の完了を監視する差分モデル。
#[derive(Debug)]
pub struct DiffModel {
    working_tree: DiffState,
    branch: DiffState,
    rx: Receiver<(DiffMode, Result<String, DiffError>)>,
    tx: mpsc::Sender<(DiffMode, Result<String, DiffError>)>,
    worker: Option<JoinHandle<()>>,
}

impl DiffModel {
    /// 全 mode が未取得のモデルを生成する。
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            working_tree: DiffState::Idle,
            branch: DiffState::Idle,
            rx,
            tx,
            worker: None,
        }
    }

    /// 指定 mode の現在状態を返す。
    pub const fn state(&self, mode: &DiffMode) -> &DiffState {
        match mode {
            DiffMode::WorkingTree => &self.working_tree,
            DiffMode::Branch { base: _ } => &self.branch,
        }
    }

    /// 差分取得を worker thread へ移し、即座に `Loading` へ遷移する。
    pub fn request(&mut self, source: Arc<dyn DiffSource>, req: DiffRequest) {
        *self.state_mut(&req.mode) = DiffState::Loading;
        let tx = self.tx.clone();
        self.worker = Some(thread::spawn(move || {
            let mode = req.mode.clone();
            let result = source.fetch(&req);
            let _send_result = tx.send((mode, result));
        }));
    }

    /// 完了済みの worker result を drain し、表示状態へ変換する。
    pub fn poll(&mut self) {
        while let Ok((mode, result)) = self.rx.try_recv() {
            *self.state_mut(&mode) = state_from_result(result);
        }

        if self.worker.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(worker) = self.worker.take()
        {
            let _join_result = worker.join();
        }
    }

    const fn state_mut(&mut self, mode: &DiffMode) -> &mut DiffState {
        match mode {
            DiffMode::WorkingTree => &mut self.working_tree,
            DiffMode::Branch { base: _ } => &mut self.branch,
        }
    }
}

impl Default for DiffModel {
    fn default() -> Self {
        Self::new()
    }
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
