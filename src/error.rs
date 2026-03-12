use thiserror::Error;

#[derive(Error, Debug)]
pub enum DHTError {
    #[error("网络错误: {0}")]
    Network(#[from] std::io::Error),

    #[error("锁中毒: {0}")]
    LockPoisoned(String),

    #[error("初始化错误: {0}")]
    Init(String),

    #[error("内部错误: {0}")]
    Internal(String),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, DHTError>;
