use std::collections::HashMap;
use std::sync::Arc;
use url::Url;
use crate::error::{MailboxError, Result};
use crate::message::{MailMessage, OutgoingMail, MailboxStatus, FetchOptions};
use crate::provider::{MailboxProvider, Subscription, AckableMessage};
use futures::future::BoxFuture;

#[derive(Clone)]
pub struct Mailbox {
    providers: HashMap<String, Arc<dyn MailboxProvider>>,
}

impl Mailbox {
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
        }
    }

    pub fn register_provider(&mut self, provider: Box<dyn MailboxProvider>) {
        self.providers.insert(provider.protocol().to_string(), Arc::from(provider));
    }

    fn get_provider(&self, protocol: &str) -> Result<Arc<dyn MailboxProvider>> {
        // Protocol usually comes with ':', so we might need to strip it if the map keys don't have it.
        // In TS, it does `protocol.slice(0, -1)`.
        // Here, let's assume the provider.protocol() returns "mem" (without colon).
        // And the URL protocol is "mem:".
        let key = if protocol.ends_with(':') {
            &protocol[..protocol.len() - 1]
        } else {
            protocol
        };

        self.providers
            .get(key)
            .cloned()
            .ok_or_else(|| MailboxError::ProviderNotFound(key.to_string()))
    }

    pub async fn post(&self, mail: OutgoingMail) -> Result<MailMessage> {
        let provider = self.get_provider(mail.to.scheme())?;

        let mut message: MailMessage = mail.clone().into();
        if message.id.is_empty() {
             message.id = provider.generate_id();
        }

        provider.send(message).await
    }

    pub async fn subscribe(
        &self,
        address: Url,
        callback: Box<dyn Fn(MailMessage) -> BoxFuture<'static, ()> + Send + Sync>,
    ) -> Result<Box<dyn Subscription>> {
        let provider = self.get_provider(address.scheme())?;
        provider.subscribe(address, callback).await
    }

    pub async fn fetch(&self, address: Url, options: FetchOptions) -> Result<Option<AckableMessage>> {
        let provider = self.get_provider(address.scheme())?;
        provider.fetch(address, options).await
    }

    pub async fn status(&self, address: Url) -> Result<MailboxStatus> {
        let provider = self.get_provider(address.scheme())?;
        provider.status(address).await
    }
}

impl Default for Mailbox {
    fn default() -> Self {
        Self::new()
    }
}
