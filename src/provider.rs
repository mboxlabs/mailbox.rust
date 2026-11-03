use async_trait::async_trait;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{MailboxError, Result};
use crate::message::{FetchOptions, MailMessage, MailboxStatus};

/// 只读快照能力缺失时的提示（写进错误信息，报错即文档）。
pub const PEEK_CAPABILITY: &str =
    "read-only peek (implement `peek(address, limit)` on the provider, or fetch without `peek`)";

/// 地址枚举能力缺失时的提示。`Mailbox::snapshot()` 据此把 provider 归入
/// [`crate::message::MailboxSnapshot::unsupported_providers`]。
pub const ADDRESS_ENUMERATION_CAPABILITY: &str =
    "address enumeration (implement `list_addresses()` on the provider)";

/// 订阅状态。与 TS / Python 端 `Subscription.status` 的 `'active' | 'closed'` 对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubscriptionStatus {
    Active,
    Closed,
}

#[async_trait]
pub trait Subscription: Send + Sync {
    /// 订阅身份。投递记账（`DeliveryResult.subscription_id`）用的就是它。
    fn id(&self) -> &str {
        ""
    }

    fn status(&self) -> SubscriptionStatus {
        SubscriptionStatus::Active
    }

    /// 订阅的地址。第三方 provider 若没记账可以返回 `None`。
    fn address(&self) -> Option<Url> {
        None
    }

    async fn unsubscribe(&mut self) -> Result<()>;
}

/**
 * 订阅回调。
 *
 * 返回 `Err` 时由 provider 按投递级记账处理：可重试错误最终把消息退回队列（可被 `fetch`
 * 取回），不可重试错误按死信丢弃。与 TS / Python 端 `subscribe` 包装回调的语义一致——
 * 之前 Rust 端回调返回 `()`，订阅者失败等于消息静默消失。
 */
pub type SubscriptionHandler = Box<dyn Fn(MailMessage) -> BoxFuture<'static, Result<()>> + Send + Sync>;

/**
 * 订阅回调失败时的统一处理：记录日志，并返回是否应当回队。
 *
 * 对应 TS 的 `_handleReceiveError` / Python 的 `_handle_receive_error`。
 *
 * 只适用于**不使用** [`crate::core::ProviderCore`] 的第三方 provider：走 core 的投递记账
 * 已经包含日志、可重试判定、重试与 barrier，不要再自己处理一遍。
 */
pub fn handle_subscription_error(protocol: &str, error: &MailboxError, message: &MailMessage) -> bool {
    eprintln!("[{}] Error processing message {}: {}", protocol, message.id, error);
    error.is_retriable()
}

/// `fetch()` 入口的公共校验：`peek: true` 不能当成普通取信跑。
///
/// TS / Python 的基类在 `fetch()` 里把只读快照分流到 `_peek`；Rust 里两种取法的**返回类型不同**
/// （`Option<AckableMessage>` vs `Vec<MailMessage>`），因此拆成 `peek()` 方法。无论走哪条路，
/// 规则一样：`fetch()` 见到 `peek: true` 直接报错，绝不「猜」调用方想要哪个 —— 把消息取走而
/// 调用方以为自己在看一眼，就是 issue#4 里最坏的那种静默降级。
///
/// 自己实现 [`MailboxProvider::fetch`] 的 provider 应在入口调用它（`MemoryProvider` 是示范）。
pub fn reject_peek_in_fetch(options: &FetchOptions) -> Result<()> {
    if !options.peek {
        return Ok(());
    }
    if options.manual_ack {
        // 与 TS / Python 同一措辞：peek 的消息从不交给消费者，没东西可 ack。
        return Err(MailboxError::InvalidFetchOptions(
            "`fetch({ peek: true, manualAck: true })` is contradictory: a peeked message is never handed \
             to a consumer, so there is nothing to acknowledge. Drop one of the two options."
                .to_string(),
        ));
    }
    Err(MailboxError::InvalidFetchOptions(
        "`peek: true` is a read-only snapshot, but `fetch()` always consumes: this port keeps them as \
         separate methods because their return types differ — call `Mailbox::peek(address, limit)` \
         (or `MailboxProvider::peek`) instead."
            .to_string(),
    ))
}

pub struct AckableMessage {
    pub message: MailMessage,
    pub ack: Box<dyn FnOnce() -> BoxFuture<'static, Result<()>> + Send + Sync>,
    pub nack: Box<dyn FnOnce(bool) -> BoxFuture<'static, Result<()>> + Send + Sync>,
}

impl AckableMessage {
    pub async fn ack(self) -> Result<()> {
        (self.ack)().await
    }

    pub async fn nack(self, requeue: bool) -> Result<()> {
        (self.nack)(requeue).await
    }
}

#[async_trait]
pub trait MailboxProvider: Send + Sync {
    fn protocol(&self) -> &str;

    /**
     * 初始化 Provider。用于建立连接、验证配置等。
     */
    async fn init(&self) -> Result<()> {
        Ok(())
    }

    /**
     * 关闭 Provider。用于释放资源、断开连接等。
     *
     * 走 [`crate::core::ProviderCore`] 的 provider 应转调 `ProviderCore::close()`：
     * 它会取消全部活跃订阅以及「等待重投」的投递。
     */
    async fn close(&self) -> Result<()> {
        Ok(())
    }

    /**
     * 公共发送入口：注入 `mbx-sent-at`（Provider 的契约，见 `interfaces.ts`），再交给
     * `_send` 做协议相关的具体投递。返回真正被投递出去的那条消息（含注入的 header）。
     */
    async fn send(&self, message: MailMessage) -> Result<MailMessage> {
        let mut message = message;
        // Always inject mbx-sent-at for outgoing messages（不改调用方传进来的对象之外语义）
        message.headers.insert(
            "mbx-sent-at".to_string(),
            chrono::Utc::now().to_rfc3339(),
        );
        self._send(message.clone()).await?;
        Ok(message)
    }

    /**
     * [Provider 职责] 协议相关的具体投递逻辑。
     *
     * 广播型 provider 在这里派发订阅者，并把每次投递的结算 future 交给
     * [`crate::core::ProviderCore::track_broadcast_settlement`]（同步登记，不要在 await 之后）。
     */
    async fn _send(&self, message: MailMessage) -> Result<()>;

    /**
     * 订阅：消息到达时由 provider 调用 `callback`。
     * 回调返回 `Err` 时按 `MailboxError::is_retriable` 与投递结算判定决定回队还是丢弃。
     */
    async fn subscribe(
        &self,
        address: Url,
        callback: SubscriptionHandler
    ) -> Result<Box<dyn Subscription>>;

    /**
     * `fetch` 取件：由消费者主动取消息。`manual_ack` 下消息进入 in-flight，
     * 必须 `ack` / `nack`，超过 `ack_timeout` 未确认会被重投。
     *
     * 只读快照不在这里：`options.peek == true` 必须报错（见 [`reject_peek_in_fetch`]），
     * 不能默默退化成消费型取件。实现入口请调一下它。
     */
    async fn fetch(&self, address: Url, options: FetchOptions) -> Result<Option<AckableMessage>>;

    /**
     * 【只读快照】返回队列里消息的**副本**：不出队、不建 in-flight、不刷活动时间。
     * 对应 TS / Python 的 `fetch(addr, { peek: true, limit })`（Rust 拆成方法是因为返回类型不同）。
     *
     * 默认实现报 [`MailboxError::UnsupportedCapability`]：把只读快照交给不支持它的传输，
     * 等于静默退化成普通取件（调用方以为在看一眼，实际把消息取走了）—— 宁可报错。
     * 消息确实存在本地的 provider（如 [`crate::providers::memory::MemoryProvider`]）应该实现它。
     *
     * @param limit 已经过 [`crate::utils::normalize_peek_limit`] 归一：`None` = 整个队列。
     */
    async fn peek(&self, address: Url, limit: Option<usize>) -> Result<Vec<MailMessage>> {
        let _ = (address, limit);
        Err(MailboxError::unsupported_capability(self.protocol(), PEEK_CAPABILITY))
    }

    /**
     * 列出本 provider 当前认得的地址（canonical id，见
     * [`crate::utils::get_canonical_mailbox_address_identifier`]）。可选能力。
     *
     * 只有消息确实存在本地的 provider 能实现。默认报 [`MailboxError::UnsupportedCapability`]，
     * [`crate::Mailbox::snapshot()`] 会把这类 provider 归入
     * [`crate::message::MailboxSnapshot::unsupported_providers`]，不会把「枚举不了」当成「队列为空」。
     */
    async fn list_addresses(&self) -> Result<Vec<String>> {
        Err(MailboxError::unsupported_capability(
            self.protocol(),
            ADDRESS_ENUMERATION_CAPABILITY,
        ))
    }

    /**
     * 查询地址状态。未实现的 provider 返回 `unknown`，与 TS / Python 端一致。
     * 注意这里**不**设 `exists: Some(false)`：问不到不等于不存在。
     */
    async fn status(&self, address: Url) -> Result<MailboxStatus> {
        let _ = address;
        Ok(MailboxStatus {
            state: "unknown".to_string(),
            exists: None,
            unread_count: None,
            in_flight_count: None,
            last_activity_time: None,
            extra: {
                let mut extra = std::collections::HashMap::new();
                extra.insert(
                    "message".to_string(),
                    serde_json::json!(format!(
                        "The '{}' provider does not support status queries.",
                        self.protocol()
                    )),
                );
                extra
            },
        })
    }

    fn generate_id(&self) -> String;
}
