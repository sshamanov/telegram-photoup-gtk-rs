use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("config error: {0}")]
    Config(String),
    #[error("image decode error: {0}")]
    Image(String),
    #[error("raw decode error: {0}")]
    Raw(String),
    #[error("encode error: {0}")]
    Encode(String),
    #[error("telegram error: {0}")]
    Telegram(String),
    #[error("state error: {0}")]
    State(String),
}

pub type Result<T> = std::result::Result<T, Error>;
