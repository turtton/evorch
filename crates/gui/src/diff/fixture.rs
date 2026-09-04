use super::{DiffError, DiffMode, DiffRequest, DiffSource};

/// demo とテスト向けに mode ごとの canned result を返す source。
#[derive(Debug, Clone)]
pub struct FixtureDiffSource {
    pub working_tree: Result<String, DiffError>,
    pub branch: Result<String, DiffError>,
}

impl FixtureDiffSource {
    /// mode ごとの canned result から source を生成する。
    pub const fn new(
        working_tree: Result<String, DiffError>,
        branch: Result<String, DiffError>,
    ) -> Self {
        Self {
            working_tree,
            branch,
        }
    }

    /// 両 mode で同じ表示テキストを返す source を生成する。
    pub fn ready(text: &str) -> Self {
        Self::new(Ok(text.to_string()), Ok(text.to_string()))
    }

    /// 両 mode で差分なしを返す source を生成する。
    pub const fn empty() -> Self {
        Self::new(Ok(String::new()), Ok(String::new()))
    }

    /// 両 mode で同じ I/O error を返す source を生成する。
    pub fn error(message: &str) -> Self {
        Self::new(
            Err(DiffError::Io(message.to_string())),
            Err(DiffError::Io(message.to_string())),
        )
    }
}

impl DiffSource for FixtureDiffSource {
    fn fetch(&self, req: &DiffRequest) -> Result<String, DiffError> {
        match &req.mode {
            DiffMode::WorkingTree => self.working_tree.clone(),
            DiffMode::Branch => self.branch.clone(),
        }
    }
}
