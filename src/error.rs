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

    /// 显式标记为可重试的错误（对齐 TS 的 `error.isRetriable === true`）。
    /// 订阅回调抛出该错误时，消息会被退回队列而不是丢失。
    #[error("Retriable provider error: {0}")]
    Retriable(String),

    /// 带错误码的传输层错误（对齐 TS / Python 的 `error.code`）。
    /// `code` 命中重试白名单（503 / 504 / ECONNRESET / ETIMEDOUT）时同样视为可重试。
    #[error("Transport error {code}: {message}")]
    Transport { code: String, message: String },
}

impl MailboxError {
    /// 判定错误是否可重试，与 TS 的 `_isRetriable` / Python 的 `_is_retriable` 一致：
    /// 显式可重试标记，或传输层错误码在重试白名单内。
    /// 不可重试的错误按死信丢弃，避免同一条坏消息被无限重试。
    pub fn is_retriable(&self) -> bool {
        match self {
            MailboxError::Retriable(_) => true,
            MailboxError::Transport { code, .. } => {
                matches!(code.as_str(), "503" | "504" | "ECONNRESET" | "ETIMEDOUT")
            }
            _ => false,
        }
    }
}

pub type Result<T> = std::result::Result<T, MailboxError>;
