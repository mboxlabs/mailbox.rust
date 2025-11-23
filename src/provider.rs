use async_trait::async_trait;
use url::Url;
use crate::error::Result;
use crate::message::{MailMessage, MailboxStatus, FetchOptions};
use futures::future::BoxFuture;

#[async_trait]
pub trait Subscription: Send + Sync {
    async fn unsubscribe(&mut self) -> Result<()>;
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

    async fn send(&self, message: MailMessage) -> Result<MailMessage>;

    async fn subscribe(
        &self,
        address: Url,
        callback: Box<dyn Fn(MailMessage) -> BoxFuture<'static, ()> + Send + Sync>
    ) -> Result<Box<dyn Subscription>>;

    async fn fetch(&self, address: Url, options: FetchOptions) -> Result<Option<AckableMessage>>;

    async fn status(&self, address: Url) -> Result<MailboxStatus>;

    fn generate_id(&self) -> String;
}
