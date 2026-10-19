use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("current directory is not inside a Git repository")]
    NotGitRepository,

    #[error("difftastic is not available in PATH (expected command: `difft`)")]
    MissingDifftastic,

    #[error("command `{cmd}` failed: {stderr}")]
    CommandFailed { cmd: String, stderr: String },

    #[error(transparent)]
    Io(#[from] std::io::Error),
}
