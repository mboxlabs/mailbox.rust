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

    /**
     * 获取所有已注册的 Provider。
     */
    pub fn providers(&self) -> &HashMap<String, Arc<dyn MailboxProvider>> {
        &self.providers
    }

    /**
     * 根据协议名获取指定的 Provider。
     * @param protocol 协议名（例如 "mem" 或 "mem:"）
     * @param raise_error_if_failed 如果为 true，且找不到 Provider 时返回错误。
     */
    pub fn get_provider(&self, protocol: &str, raise_error_if_failed: bool) -> Result<Option<Arc<dyn MailboxProvider>>> {
        let key = if protocol.ends_with(':') {
            &protocol[..protocol.len() - 1]
        } else {
            protocol
        };

        match self.providers.get(key) {
            Some(p) => Ok(Some(p.clone())),
            None => {
                if raise_error_if_failed {
                    Err(MailboxError::ProviderNotFound(key.to_string()))
                } else {
                    Ok(None)
                }
            }
        }
    }

    fn get_provider_or_fail(&self, protocol: &str) -> Result<Arc<dyn MailboxProvider>> {
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

    /**
     * 启动所有注册的 Provider。
     */
    pub async fn start(&self) -> Result<()> {
        let mut futures = Vec::new();
        for provider in self.providers.values() {
            futures.push(provider.init());
        }
        
        // 并行初始化
        let results = futures::future::join_all(futures).await;
        for res in results {
            res?;
        }
        Ok(())
    }

    /**
     * 关闭所有注册的 Provider，并释放资源。
     */
    pub async fn stop(&self) -> Result<()> {
        let mut futures = Vec::new();
        for provider in self.providers.values() {
            futures.push(provider.close());
        }
        
        // 并行关闭
        let results = futures::future::join_all(futures).await;
        for res in results {
            res?;
        }
        Ok(())
    }

    pub fn register_provider(&mut self, provider: Box<dyn MailboxProvider>) {
        self.providers.insert(provider.protocol().to_string(), Arc::from(provider));
    }

    pub async fn post(&self, mail: OutgoingMail) -> Result<MailMessage> {
        let provider = self.get_provider_or_fail(mail.to.scheme())?;

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
        let provider = self.get_provider_or_fail(address.scheme())?;
        provider.subscribe(address, callback).await
    }

    pub async fn fetch(&self, address: Url, options: FetchOptions) -> Result<Option<AckableMessage>> {
        let provider = self.get_provider_or_fail(address.scheme())?;
        provider.fetch(address, options).await
    }

    pub async fn status(&self, address: Url) -> Result<MailboxStatus> {
        let provider = self.get_provider_or_fail(address.scheme())?;
        provider.status(address).await
    }
}

impl Default for Mailbox {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::memory::MemoryProvider;
    use crate::message::OutgoingMail;
    use serde_json::json;

    #[tokio::test]
    async fn test_mailbox_lifecycle() -> Result<()> {
        let mut mailbox = Mailbox::new();
        mailbox.register_provider(Box::new(MemoryProvider::new()));
        
        // 1. Test start
        mailbox.start().await?;
        
        // 2. Test operations
        let to_address: Url = "mem:test/lifecycle".parse().unwrap();
        let mail = OutgoingMail {
            id: None,
            from: "mem:test/sender".parse().unwrap(),
            to: to_address.clone(),
            body: json!({"test": "lifecycle"}),
            headers: HashMap::new(),
            meta: HashMap::new(),
        };
        
        mailbox.post(mail).await?;
        let status = mailbox.status(to_address.clone()).await?;
        assert_eq!(status.unread_count, Some(1));
        
        // 3. Test stop
        mailbox.stop().await?;
        
        Ok(())
    }

    #[tokio::test]
    async fn test_provider_access() -> Result<()> {
        let mut mailbox = Mailbox::new();
        mailbox.register_provider(Box::new(MemoryProvider::new()));

        // Test providers getter
        let providers = mailbox.providers();
        assert!(providers.contains_key("mem"));

        // Test get_provider with and without colon
        assert!(mailbox.get_provider("mem", false)?.is_some());
        assert!(mailbox.get_provider("mem:", false)?.is_some());

        // Test get_provider without raise_error
        assert!(mailbox.get_provider("unknown", false)?.is_none());

        // Test get_provider with raise_error
        let result = mailbox.get_provider("unknown", true);
        assert!(result.is_err());
        match result {
            Err(MailboxError::ProviderNotFound(name)) => assert_eq!(name, "unknown"),
            _ => panic!("Expected ProviderNotFound error"),
        }

        Ok(())
    }
}
