use async_trait::async_trait;
use url::Url;
use std::sync::{Arc, RwLock};
use std::collections::HashMap;
use uuid::Uuid;
use futures::future::BoxFuture;
use std::time::Duration;
use once_cell::sync::Lazy;

use crate::error::Result;
use crate::message::{MailMessage, MailboxStatus, FetchOptions};
use crate::provider::{MailboxProvider, Subscription, AckableMessage};
use crate::utils::get_canonical_mailbox_address_identifier;
use crate::providers::queue::MailMessageQueue;

type Listener = Box<dyn Fn(MailMessage) -> BoxFuture<'static, ()> + Send + Sync>;

struct MemoryEventBus {
    topics: HashMap<String, Vec<Arc<Listener>>>,
    queue: MailMessageQueue<MailMessage>,
    last_activity: HashMap<String, String>,
}

impl MemoryEventBus {
    fn new() -> Self {
        Self {
            topics: HashMap::new(),
            queue: MailMessageQueue::new(),
            last_activity: HashMap::new(),
        }
    }
}

// Singleton instance using once_cell
static BUS: Lazy<Arc<RwLock<MemoryEventBus>>> = Lazy::new(|| {
    Arc::new(RwLock::new(MemoryEventBus::new()))
});

pub struct MemoryProvider {
    protocol: String,
}

impl MemoryProvider {
    pub fn new() -> Self {
        Self {
            protocol: "mem".to_string(),
        }
    }
}

struct MemorySubscription {
    topic: String,
    listener: Arc<Listener>,
}

#[async_trait]
impl Subscription for MemorySubscription {
    async fn unsubscribe(&mut self) -> Result<()> {
        let mut bus = BUS.write().unwrap();
        if let Some(listeners) = bus.topics.get_mut(&self.topic) {
            listeners.retain(|l| !Arc::ptr_eq(l, &self.listener));
        }
        Ok(())
    }
}

#[async_trait]
impl MailboxProvider for MemoryProvider {
    fn protocol(&self) -> &str {
        &self.protocol
    }

    async fn send(&self, message: MailMessage) -> Result<MailMessage> {
        let topic = get_canonical_mailbox_address_identifier(&message.to);
        let mut bus = BUS.write().unwrap();

        bus.last_activity.insert(topic.clone(), chrono::Utc::now().to_rfc3339());

        // Push to subscribers
        if let Some(listeners) = bus.topics.get(&topic) {
            for listener in listeners {
                let msg = message.clone();
                let listener = listener.clone();

                #[cfg(not(target_arch = "wasm32"))]
                tokio::spawn(async move {
                    (listener)(msg).await;
                });

                #[cfg(target_arch = "wasm32")]
                wasm_bindgen_futures::spawn_local(async move {
                    (listener)(msg).await;
                });
            }
        }

        // Enqueue for pull consumers
        bus.queue.enqueue(topic, message.clone());

        Ok(message)
    }

    async fn subscribe(
        &self,
        address: Url,
        callback: Box<dyn Fn(MailMessage) -> BoxFuture<'static, ()> + Send + Sync>,
    ) -> Result<Box<dyn Subscription>> {
        let topic = get_canonical_mailbox_address_identifier(&address);
        let mut bus = BUS.write().unwrap();

        let listener = Arc::new(callback);
        bus.topics
            .entry(topic.clone())
            .or_insert_with(Vec::new)
            .push(listener.clone());

        bus.last_activity.insert(topic.clone(), chrono::Utc::now().to_rfc3339());

        Ok(Box::new(MemorySubscription {
            topic,
            listener,
        }))
    }

    async fn fetch(&self, address: Url, options: FetchOptions) -> Result<Option<AckableMessage>> {
        let topic = get_canonical_mailbox_address_identifier(&address);
        let mut bus = BUS.write().unwrap();

        bus.last_activity.insert(topic.clone(), chrono::Utc::now().to_rfc3339());

        if !options.manual_ack {
            if let Some(msg) = bus.queue.dequeue(&topic) {
                return Ok(Some(AckableMessage {
                    message: msg,
                    ack: Box::new(|| Box::pin(async { Ok(()) })),
                    nack: Box::new(|_| Box::pin(async { Ok(()) })),
                }));
            }
            return Ok(None);
        }

        let timeout = options.ack_timeout.map(Duration::from_millis);
        if let Some(msg) = bus.queue.dequeue_for_ack(&topic, timeout) {
             let msg_id = msg.id.clone();
             let msg_id_nack = msg.id.clone();

             return Ok(Some(AckableMessage {
                 message: msg,
                 ack: Box::new(move || Box::pin(async move {
                     let mut bus = BUS.write().unwrap();
                     bus.queue.ack(&msg_id);
                     Ok(())
                 })),
                 nack: Box::new(move |requeue| Box::pin(async move {
                     let mut bus = BUS.write().unwrap();
                     bus.queue.nack(&msg_id_nack, requeue);
                     Ok(())
                 })),
             }));
        }

        Ok(None)
    }

    async fn status(&self, address: Url) -> Result<MailboxStatus> {
        let topic = get_canonical_mailbox_address_identifier(&address);
        let bus = BUS.read().unwrap();

        let unread_count = bus.queue.get_status(&topic);
        let last_activity_time = bus.last_activity.get(&topic).cloned();

        Ok(MailboxStatus {
            state: "online".to_string(),
            unread_count: Some(unread_count),
            last_activity_time,
            extra: HashMap::new(),
        })
    }

    fn generate_id(&self) -> String {
        Uuid::new_v4().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::OutgoingMail;
    use serde_json::json;
    use std::sync::Mutex;

    #[tokio::test]
    async fn test_send_and_subscribe() -> Result<()> {
        let provider = MemoryProvider::new();
        let address: Url = "mem:test/inbox".parse()?;

        let (tx, rx) = tokio::sync::oneshot::channel();
        let tx = Arc::new(Mutex::new(Some(tx)));

        let _sub = provider.subscribe(address.clone(), Box::new(move |msg| {
            let tx = tx.clone();
            Box::pin(async move {
                if let Some(tx) = tx.lock().unwrap().take() {
                    tx.send(msg).unwrap();
                }
            })
        })).await?;

        let mail = OutgoingMail {
            id: None,
            from: "mem:test/sender".parse()?,
            to: address,
            body: json!({"hello": "world"}),
            headers: HashMap::new(),
            meta: HashMap::new(),
        };

        provider.send(mail.into()).await?;

        let received = rx.await.unwrap();
        assert_eq!(received.body, json!({"hello": "world"}));
        Ok(())
    }

    #[tokio::test]
    async fn test_fetch_auto_ack() -> Result<()> {
        let provider = MemoryProvider::new();
        let address: Url = "mem:test/fetch".parse()?;

        let mail = OutgoingMail {
            id: Some("msg1".to_string()),
            from: "mem:test/sender".parse()?,
            to: address.clone(),
            body: json!("content"),
            headers: HashMap::new(),
            meta: HashMap::new(),
        };

        provider.send(mail.into()).await?;

        let fetched = provider.fetch(address.clone(), FetchOptions::default()).await?;
        assert!(fetched.is_some());
        let msg = fetched.unwrap();
        assert_eq!(msg.message.id, "msg1");

        // Should be gone
        let fetched2 = provider.fetch(address, FetchOptions::default()).await?;
        assert!(fetched2.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn test_fetch_manual_ack() -> Result<()> {
        let provider = MemoryProvider::new();
        let address: Url = "mem:test/ack".parse()?;

        let mail = OutgoingMail {
            id: Some("msg2".to_string()),
            from: "mem:test/sender".parse()?,
            to: address.clone(),
            body: json!("content"),
            headers: HashMap::new(),
            meta: HashMap::new(),
        };

        provider.send(mail.into()).await?;

        let options = FetchOptions {
            manual_ack: true,
            ack_timeout: None,
        };

        let fetched = provider.fetch(address.clone(), options.clone()).await?;
        assert!(fetched.is_some());
        let msg = fetched.unwrap();
        assert_eq!(msg.message.id, "msg2");

        // Should not be available yet (in flight)
        let fetched2 = provider.fetch(address.clone(), options.clone()).await?;
        assert!(fetched2.is_none());

        // Ack it
        msg.ack().await?;

        // Should be gone permanently
        let fetched3 = provider.fetch(address, options).await?;
        assert!(fetched3.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn test_fetch_nack_requeue() -> Result<()> {
        let provider = MemoryProvider::new();
        let address: Url = "mem:test/nack".parse()?;

        let mail = OutgoingMail {
            id: Some("msg3".to_string()),
            from: "mem:test/sender".parse()?,
            to: address.clone(),
            body: json!("content"),
            headers: HashMap::new(),
            meta: HashMap::new(),
        };

        provider.send(mail.into()).await?;

        let options = FetchOptions {
            manual_ack: true,
            ack_timeout: None,
        };

        let fetched = provider.fetch(address.clone(), options.clone()).await?;
        let msg = fetched.unwrap();

        // Nack with requeue
        msg.nack(true).await?;

        // Should be available again
        let fetched2 = provider.fetch(address, options).await?;
        assert!(fetched2.is_some());
        assert_eq!(fetched2.unwrap().message.id, "msg3");
        Ok(())
    }
}
