use thiserror::Error;

#[derive(Error, Debug)]
pub enum DHTError {
    #[error("网络错误: {0}")]
    Network(#[from] std::io::Error),

    #[error("Bencode 解析错误: {0}")]
    Bencode(String),

    #[error("协议错误: {0}")]
    Protocol(String),

    #[error("超时")]
    Timeout,

    #[error("未找到 Peer")]
    NoPeersAvailable,

    #[error("元数据验证失败")]
    InvalidMetadata,

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, DHTError>;
