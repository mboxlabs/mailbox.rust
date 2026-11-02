use async_trait::async_trait;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{MailboxError, Result};
use crate::message::{FetchOptions, MailMessage, MailboxStatus};

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
     */
    async fn fetch(&self, address: Url, options: FetchOptions) -> Result<Option<AckableMessage>>;

    /**
     * 查询地址状态。未实现的 provider 返回 `unknown`，与 TS / Python 端一致。
     */
    async fn status(&self, address: Url) -> Result<MailboxStatus> {
        let _ = address;
        Ok(MailboxStatus {
            state: "unknown".to_string(),
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
