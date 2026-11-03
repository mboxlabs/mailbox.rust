use thiserror::Error;

/// 稳定的错误码，跨端口对齐（TS `MailboxErrorCode` / Python `MailboxError.code`）。
///
/// 调用方判断错误类型请用 [`MailboxError::code`] 或模式匹配，**不要**匹配 message ——
/// message 是给人看的，会变。
#[derive(Error, Debug)]
pub enum MailboxError {
    /// 旧名（0.1.x 暴露过）：provider 内部的「输入 / 地址不合法」。码与新端口一致。
    /// 地址**校验**（query / hash 会被规范化丢掉）用的是 [`MailboxError::InvalidMailboxAddress`]。
    #[error("Invalid address: {0}")]
    InvalidAddress(String),

    /// 地址不合法：无法解析成 URL，或者带着规范化会**静默丢掉**的部件（query / hash）。
    ///
    /// 见 [`crate::utils::get_canonical_mailbox_address_identifier`]：邮箱语义必须留在 path 里，
    /// query / hash 不参与寻址，因此带它们的地址直接被拒绝，而不是被「顺手裁掉」（issue#4）。
    #[error("{message}")]
    InvalidMailboxAddress { address: String, message: String },

    /// `fetch()` / `peek()` 的选项自相矛盾或取值非法（例如 `peek` 与 `manual_ack` 同时出现）。
    /// 这类错误是调用方的代码问题，必须在调用点炸出来，不能被忽略。
    #[error("{0}")]
    InvalidFetchOptions(String),

    /// provider 不具备被请求的能力（例如对不支持只读 `peek` 的传输强要 peek）。
    ///
    /// 为什么要有这个错误：把 `peek` 这类新选项交给不认识它的 provider，等于**静默退化成普通
    /// fetch** —— 调用方以为自己在只读快照，实际已经把消息出队了。宁可抛错。
    #[error("The '{protocol}' provider does not support '{capability}'.")]
    UnsupportedCapability {
        protocol: String,
        capability: String,
    },

    /// 没有 provider 认领这个协议（对齐 TS `NoProviderError` / Python `ProviderNotFound`）。
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
    /// 稳定错误码；没有对应码的变体（传输 / 序列化 / 未知）返回 `None`，
    /// 与 Python 端 `MailboxError.code = None` 一致。
    pub fn code(&self) -> Option<&'static str> {
        match self {
            MailboxError::InvalidAddress(_) | MailboxError::InvalidMailboxAddress { .. } => {
                Some("MBX_INVALID_ADDRESS")
            }
            MailboxError::ProviderNotFound(_) => Some("MBX_NO_PROVIDER"),
            MailboxError::InvalidFetchOptions(_) => Some("MBX_INVALID_FETCH_OPTIONS"),
            MailboxError::UnsupportedCapability { .. } => Some("MBX_UNSUPPORTED_CAPABILITY"),
            _ => None,
        }
    }

    /// 构造地址校验错误：`address` 留给诊断页展示，`message` 说清「会跟谁合并」。
    pub fn invalid_mailbox_address(address: impl Into<String>, message: impl Into<String>) -> Self {
        MailboxError::InvalidMailboxAddress {
            address: address.into(),
            message: message.into(),
        }
    }

    /// 构造「provider 不具备该能力」错误。`capability` 里带上补救办法，报错即文档。
    pub fn unsupported_capability(protocol: &str, capability: &str) -> Self {
        MailboxError::UnsupportedCapability {
            protocol: protocol.to_string(),
            capability: capability.to_string(),
        }
    }

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
