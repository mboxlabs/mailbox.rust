use thiserror::Error;

#[derive(Error, Debug)]
pub enum MailboxError {
    #[error("Invalid address: {0}")]
    InvalidAddress(String),

    #[error("No provider found for protocol: {0}")]
    ProviderNotFound(String),

    #[error("Provider error: {0}")]
    ProviderError(String),

    #[error("Serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),

    #[error("URL parse error: {0}")]
    UrlParseError(#[from] url::ParseError),

    #[error("Unknown error: {0}")]
    Unknown(String),
}

pub type Result<T> = std::result::Result<T, MailboxError>;
