use async_trait::async_trait;
use url::Url;
use crate::error::{MailboxError, Result};
use crate::message::{MailMessage, MailboxStatus, FetchOptions};
use futures::future::BoxFuture;

#[async_trait]
pub trait Subscription: Send + Sync {
    async fn unsubscribe(&mut self) -> Result<()>;
}

/**
 * 订阅回调。
 *
 * 返回 `Err` 时由 provider 统一处理：可重试错误把消息退回队列（可被 `fetch` 取回），
 * 不可重试错误按死信丢弃。与 TS / Python 端 `subscribe` 包装回调的语义一致——
 * 之前 Rust 端回调返回 `()`，订阅者失败等于消息静默消失。
 */
pub type SubscriptionHandler = Box<dyn Fn(MailMessage) -> BoxFuture<'static, Result<()>> + Send + Sync>;

/**
 * 订阅回调失败时的统一处理：记录日志，并返回是否应当回队。
 *
 * 对应 TS 的 `_handleReceiveError` / Python 的 `_handle_receive_error`。
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
     */
    async fn close(&self) -> Result<()> {
        Ok(())
    }

    async fn send(&self, message: MailMessage) -> Result<MailMessage>;

    /**
     * 订阅：消息到达时由 provider 调用 `callback`。
     * 回调返回 `Err` 时按 `MailboxError::is_retriable` 决定回队还是丢弃。
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

    async fn status(&self, address: Url) -> Result<MailboxStatus>;

    fn generate_id(&self) -> String;
}
