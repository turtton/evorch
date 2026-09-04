use std::process::{Command, Stdio};

use super::{DiffError, DiffMode, DiffRequest, DiffSource};

/// branch diff の固定 base branch。base 選択は API に公開しない (issue #65 AC11)。
const DIFF_BASE_BRANCH: &str = "main";

/// ローカル Git リポジトリから unified diff を取得する読み取り専用 source。
///
/// GUI と同じ trust domain で動作し、資格情報を扱わず、network access を行わない。
#[derive(Debug, Clone, Copy, Default)]
pub struct GitCliDiffSource;

impl DiffSource for GitCliDiffSource {
    fn fetch(&self, req: &DiffRequest) -> Result<String, DiffError> {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&req.repo_root)
            .args(["diff", "--no-color", "--no-ext-diff"])
            .env("GIT_PAGER", "cat")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(Stdio::null());

        match &req.mode {
            DiffMode::WorkingTree => {}
            DiffMode::Branch => {
                command.arg(format!("{DIFF_BASE_BRANCH}...HEAD"));
            }
        }

        let output = command
            .output()
            .map_err(|error| DiffError::Spawn(error.to_string()))?;
        if !output.status.success() {
            return Err(DiffError::Git {
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }

        String::from_utf8(output.stdout).map_err(|error| DiffError::Io(error.to_string()))
    }
}
