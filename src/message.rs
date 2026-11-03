use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use url::Url;

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
    /// provider 对**这个地址**的可观测状态：
    /// - `online`：provider 在线，且认得这个地址（有队列 / 订阅 / 活动记录）；
    /// - `unknown`：provider 在线，但对这个地址**没有任何记录**（地址不存在、拼错，
    ///   或者 provider 本身不支持状态查询）；
    /// - `offline` / `degraded`：provider 自己连不上 / 只能给出部分答复。
    ///
    /// 不要把「地址不存在」报成 `online` + 0：诊断页就区分不出「不存在」与「空闲」（issue#4）。
    /// 要区分「地址不存在」和「provider 答不了」，看 [`MailboxStatus::exists`]。
    pub state: String,
    /// provider 是否认得这个地址（有过消息 / 订阅 / 活动记录）。
    ///
    /// 只有在 provider **明确回答了**状态查询时才有值；不支持 `status` 的 provider 不会把它
    /// 设成 `Some(false)`（那会把「问不到」误报成「不存在」），因此它是 `Option` 且缺省不序列化。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exists: Option<bool>,
    /// 队列中等待被取走的消息数（含被重投的），即 `fetch` 能取到的数量。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unread_count: Option<usize>,
    /// 已取走但尚未 ack 的消息数。与 unread_count 分列，
    /// 否则 in-flight 消息在状态里隐形，会被当作已处理完。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_flight_count: Option<usize>,
    /// 最后一次有消息活动（发送 / 取回）的时间（ISO 8601）；没有活动记录时是 `None`，
    /// 在 JSON 里就是 `null`。
    ///
    /// 这里**不能** `skip_serializing_if`：字段消失后，诊断页拿到的是「没有这个概念」，
    /// 而不是「没有活动」（issue#4）。只读操作（`status` / `peek`）不会刷新它。
    pub last_activity_time: Option<String>,
    /// provider 特定的扩展信息（`subscriber_count`、错误说明 `message` 等）。
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// 快照里一个地址的状态：[`MailboxStatus`] 加地址身份。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxAddressSnapshot {
    /// 寻址用的规范形式（见 [`crate::utils::get_canonical_mailbox_address_identifier`]）。
    pub canonical_id: String,
    /// 处理这个地址的 provider 协议名。
    pub protocol: String,
    /// 该地址的状态（序列化时展平，与 TS / Python 的字段形状一致）。
    #[serde(flatten)]
    pub status: MailboxStatus,
}

/// 全量快照：一时刻整个 [`crate::Mailbox`]（所有 provider、所有 topic）的诊断视图。
///
/// 给诊断页、测试断言用（issue#4，等价于 TS / Python 里不传地址的 `status()`）。
/// 只能由能枚举自身 topic 的 provider（实现 [`crate::provider::MailboxProvider::list_addresses`]）
/// 提供地址；不能枚举的（AMQP / SQS 这类消息不在本地的 broker）列在
/// [`MailboxSnapshot::unsupported_providers`] 里，避免快照把「枚举不了」误读成「队列为空」。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxSnapshot {
    /// 快照的完整度：
    /// - `online`：所有 provider 都能枚举，且所有地址都是 `online`；
    /// - `degraded`：有 provider 无法枚举，或某个地址不是 `online`（只看到了一部分）；
    /// - `unknown`：没有任何 provider 能枚举（包括一个 provider 都没有注册）。
    pub state: String,
    /// 已注册的协议名（排序后）。
    pub protocols: Vec<String>,
    /// 快照里的地址总数。
    pub address_count: usize,
    /// 所有地址的 `unread_count` 之和。
    pub unread_count: usize,
    /// 所有地址的 `in_flight_count` 之和。
    pub in_flight_count: usize,
    /// 按 `canonical_id` 排序的地址状态，保证快照可重复、断言可写死。
    pub addresses: Vec<MailboxAddressSnapshot>,
    /// 不支持枚举自身地址的 provider（它们的队列不在本次快照里）。
    pub unsupported_providers: Vec<String>,
    /// 快照生成的时间（ISO 8601）。
    pub taken_at: String,
}

#[derive(Debug, Clone, Default)]
pub struct FetchOptions {
    pub manual_ack: bool,
    /**
     * 手动 ACK 下的 in-flight 超时（毫秒）。
     * `None` → 用 `DEFAULT_ACK_TIMEOUT`（30s）；`Some(0)` → 关闭超时，消息常驻 in-flight 直到 ack/nack。
     */
    pub ack_timeout: Option<u64>,
    /// 【只读快照】对齐 TS `fetch(addr, { peek: true })` / Python `FetchOptions(peek=True)` 的字段形状。
    ///
    /// Rust 里取信与只读快照的**返回类型不同**（`Option<AckableMessage>` vs `Vec<MailMessage>`），
    /// 所以拆成两个方法：只读快照请用 [`crate::Mailbox::peek`]。`fetch()` 收到 `peek: true` 会在入口
    /// 报 [`crate::error::MailboxError::InvalidFetchOptions`]，不会被当成普通取信跑（issue#4：
    /// 「调用方以为自己在看一眼，实际把消息取走了」是最坏的静默降级）。
    pub peek: bool,
    /// `peek` 最多看几条；`None` = 整个队列。必须是**非负**整数（Rust 的 `usize` 已在编译期保证）。
    pub limit: Option<usize>,
}
