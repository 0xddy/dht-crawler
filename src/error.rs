use thiserror::Error;

#[derive(Error, Debug)]
pub enum DHTError {
    #[error("网络错误: {0}")]
    Network(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, DHTError>;
