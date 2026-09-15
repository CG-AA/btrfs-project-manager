//! Error types that carry a distinct process exit code.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum BpmError {
    #[error("{0}")]
    Usage(String),
    #[error("vetoed by hook {hook}: {detail}")]
    HookVeto { hook: PathBuf, detail: String },
    #[error("locked by another bpm process ({holder})")]
    Locked { holder: String },
    #[error("this command needs root: {0}")]
    NeedsRoot(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("refused: {0}")]
    Refused(String),
    #[error("{failed} project(s) had errors")]
    Partial { failed: usize },
}

impl BpmError {
    pub fn exit_code(&self) -> i32 {
        match self {
            BpmError::Usage(_) => 2,
            BpmError::HookVeto { .. } => 3,
            BpmError::Locked { .. } => 4,
            BpmError::NeedsRoot(_) => 5,
            BpmError::NotFound(_) => 6,
            BpmError::Refused(_) => 7,
            BpmError::Partial { .. } => 8,
        }
    }
}

/// Exit code for any error: a `BpmError` anywhere in the chain decides, otherwise 1.
pub fn exit_code_for(err: &anyhow::Error) -> i32 {
    err.chain().find_map(|e| e.downcast_ref::<BpmError>()).map(BpmError::exit_code).unwrap_or(1)
}

pub fn refused(msg: impl Into<String>) -> anyhow::Error {
    BpmError::Refused(msg.into()).into()
}

pub fn not_found(msg: impl Into<String>) -> anyhow::Error {
    BpmError::NotFound(msg.into()).into()
}

pub fn usage(msg: impl Into<String>) -> anyhow::Error {
    BpmError::Usage(msg.into()).into()
}
