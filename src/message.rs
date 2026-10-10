use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;
use std::collections::HashMap;

// We need to import Identifiable trait if we want to implement it here?
// Or we can just implement it in providers/queue.rs if we import MailMessage there?
// But queue.rs is generic.
// So we should define Identifiable in a common place or in queue.rs and import it here.
// But queue.rs depends on message.rs? No, queue.rs is generic.
// Wait, I defined Identifiable in queue.rs.
// So I need to import it here.
// But message.rs is a core module, queue.rs is a provider module.
// Core shouldn't depend on provider modules.
// So Identifiable should be in `utils.rs` or `lib.rs` or `message.rs`.
// I'll move Identifiable to `message.rs` or `utils.rs`.
// `message.rs` seems best as it relates to message identity.

pub trait Identifiable {
    fn id(&self) -> &str;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutgoingMail {
    pub id: Option<String>,
    pub from: Url,
    pub to: Url,
    pub body: Value,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub meta: HashMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailMessage {
    pub id: String,
    pub from: Url,
    pub to: Url,
    pub body: Value,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub meta: HashMap<String, Value>,
}

impl Identifiable for MailMessage {
    fn id(&self) -> &str {
        &self.id
    }
}

impl From<OutgoingMail> for MailMessage {
    fn from(mail: OutgoingMail) -> Self {
        MailMessage {
            id: mail.id.unwrap_or_default(),
            from: mail.from,
            to: mail.to,
            body: mail.body,
            headers: mail.headers,
            meta: mail.meta,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxStatus {
    pub state: String,
    /// 队列中等待被取走的消息数（含被重投的），即 `fetch` 能取到的数量。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unread_count: Option<usize>,
    /// 已取走但尚未 ack 的消息数。与 unread_count 分列，
    /// 否则 in-flight 消息在状态里隐形，会被当作已处理完。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_flight_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity_time: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

#[derive(Debug, Clone)]
pub struct FetchOptions {
    pub manual_ack: bool,
    /**
     * 手动 ACK 下的 in-flight 超时（毫秒）。
     * `None` → 用 `DEFAULT_ACK_TIMEOUT`（30s）；`Some(0)` → 关闭超时，消息常驻 in-flight 直到 ack/nack。
     */
    pub ack_timeout: Option<u64>,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            manual_ack: false,
            ack_timeout: None,
        }
    }
}
