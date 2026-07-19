use thiserror::Error;

#[derive(Error, Debug)]
/// Error returned during DHT initialization or execution.
pub enum DHTError {
    /// Socket or other network I/O failed.
    #[error("网络错误: {0}")]
    Network(#[from] std::io::Error),

    /// A shared lock was poisoned.
    #[error("锁中毒: {0}")]
    LockPoisoned(String),

    /// Server initialization failed, for example because a socket could not bind.
    #[error("初始化错误: {0}")]
    Init(String),

    /// An internal invariant or worker operation failed.
    #[error("内部错误: {0}")]
    Internal(String),

    /// Another error represented by a human-readable message.
    #[error("{0}")]
    Other(String),
}

/// Result type used by the crate's public APIs.
pub type Result<T> = std::result::Result<T, DHTError>;
