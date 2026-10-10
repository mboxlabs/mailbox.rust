use async_trait::async_trait;
use url::Url;
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::collections::HashMap;
use uuid::Uuid;
use futures::future::BoxFuture;
use std::time::Duration;
use once_cell::sync::Lazy;

use crate::error::Result;
use crate::message::{MailMessage, MailboxStatus, FetchOptions};
use crate::provider::{MailboxProvider, Subscription, AckableMessage, SubscriptionHandler, handle_subscription_error};
use crate::retry::{next_retry_delay, AttemptLedger, RetryPolicy};
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
    /// 订阅失败后的重试策略。`None` 表示不重试（默认）：可重试失败只把消息退回队列，
    /// 由 `fetch` 兜底。重试是 opt-in 的，因为 AMQP / SQS 这类传输自带重投与死信，
    /// 库再重投就是双重投递。
    retry: Option<RetryPolicy>,
    /// 按「订阅者 + 消息」记账尝试次数；订阅是广播，不能混在一起算。
    attempts: Arc<Mutex<AttemptLedger>>,
}

impl MemoryProvider {
    pub fn new() -> Self {
        Self::with_retry(None)
    }

    /// 开启订阅重试（opt-in）。不传则保持默认：订阅失败只把消息退回队列，不自动重投。
    pub fn with_retry(policy: Option<RetryPolicy>) -> Self {
        Self {
            protocol: "mem".to_string(),
            retry: policy.map(|p| p.resolved()),
            attempts: Arc::new(Mutex::new(AttemptLedger::new(Duration::from_secs(300)))),
        }
    }
}

/// 某个 listener 是否仍注册在该 topic 上（即订阅仍活跃）。
fn listener_is_active(listener: &Arc<Listener>, topic: &str) -> bool {
    let bus = BUS.read().unwrap();
    bus.topics
        .get(topic)
        .map(|listeners| listeners.iter().any(|l| Arc::ptr_eq(l, listener)))
        .unwrap_or(false)
}

/// 等待重试延迟。
///
/// wasm32 下 tokio 未启用 `time` feature，退化为不等待（仍受 `max_attempts` 约束）。
async fn sleep_before_retry(delay: Duration) {
    #[cfg(not(target_arch = "wasm32"))]
    tokio::time::sleep(delay).await;
    #[cfg(target_arch = "wasm32")]
    let _ = delay;
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

        // 投递给订阅者（先 clone 出来，释放对 bus 的借用）
        let listeners: Vec<Arc<Listener>> = bus.topics.get(&topic).cloned().unwrap_or_default();
        if !listeners.is_empty() {
            for listener in listeners {
                let msg = message.clone();

                #[cfg(not(target_arch = "wasm32"))]
                tokio::spawn(async move {
                    (listener)(msg).await;
                });

                #[cfg(target_arch = "wasm32")]
                wasm_bindgen_futures::spawn_local(async move {
                    (listener)(msg).await;
                });
            }

            // 已交给订阅者的消息不再入队（与 TS 端一致）：两条路都走时，订阅者
            // 从不 fetch → 队列只涨不消；退订后一次 fetch 又会把已处理的消息再投递一遍。
            // 必须判空：退订后 topics[topic] 仍存在（空 Vec），不判就静默丢消息。
            return Ok(message);
        }

        // 入队，等 fetch 消费者取走
        bus.queue.enqueue(topic, message.clone());

        Ok(message)
    }

    async fn subscribe(
        &self,
        address: Url,
        callback: SubscriptionHandler,
    ) -> Result<Box<dyn Subscription>> {
        let topic = get_canonical_mailbox_address_identifier(&address);

        // 对齐 TS / Python 的 MailboxProvider.subscribe：用户回调返回 Err 时，
        // 由 provider 统一记录日志并按可重试判定回队（不可重试则丢弃，避免无限重试坏消息）。
        // 回队走 queue.requeue，因此失败的消息在 unread_count 里可见、可被 fetch 取回。
        let sub_id = Uuid::new_v4().to_string();
        let user = Arc::new(callback);
        let topic_for_cb = topic.clone();
        let retry = self.retry;
        let attempts = self.attempts.clone();
        // 重投时要调的就是这个包装后的 listener（会再次走错误处理），而不是用户原始回调。
        // 构造期间无法引用自身，因此用 holder 在构造完成后回填（Weak 避免 listener 自引用泄漏）。
        let holder: Arc<RwLock<Weak<Listener>>> = Arc::new(RwLock::new(Weak::new()));
        let holder_for_cb = holder.clone();

        let wrapped: Listener = Box::new(move |msg: MailMessage| {
            let user = user.clone();
            let topic = topic_for_cb.clone();
            let sub_id = sub_id.clone();
            let attempts = attempts.clone();
            let holder = holder_for_cb.clone();
            Box::pin(async move {
                let payload = msg.clone();

                if let Err(error) = (user)(msg).await {
                    let requeue = handle_subscription_error("mem", &error, &payload);
                    if !requeue {
                        return; // 不可重试：按死信丢弃，避免同一条坏消息被无限重试
                    }

                    if let Some(policy) = retry {
                        let attempt = attempts.lock().unwrap().record(&sub_id, &payload.id);

                        if attempt < policy.max_attempts {
                            let delay = next_retry_delay(&policy, attempt);
                            let holder = holder.clone();
                            let topic = topic.clone();
                            let message = payload.clone();

                            // 只重投给失败的那个订阅者（不重新广播），否则已成功的
                            // 订阅者会被再执行一次副作用。
                            let task = async move {
                                sleep_before_retry(delay).await;

                                let listener = holder.read().unwrap().upgrade();
                                let still_active = listener
                                    .as_ref()
                                    .map(|l| listener_is_active(l, &topic))
                                    .unwrap_or(false);

                                if !still_active {
                                    // 等待期间退订了：不重投给已消失的订阅者，退回队列而不是静默丢掉。
                                    let mut bus = BUS.write().unwrap();
                                    bus.queue.requeue(topic, message);
                                    return;
                                }

                                if let Some(listener) = listener {
                                    (listener)(message).await;
                                }
                            };

                            #[cfg(not(target_arch = "wasm32"))]
                            tokio::spawn(task);
                            #[cfg(target_arch = "wasm32")]
                            wasm_bindgen_futures::spawn_local(task);
                            return;
                        }

                        // 次数耗尽：清零台账，再退回队列交给 fetch 兜底。
                        attempts.lock().unwrap().reset(&sub_id, &payload.id);
                    }

                    let mut bus = BUS.write().unwrap();
                    bus.queue.requeue(topic, payload);
                    return;
                }

                // 成功：清零台账，让同 id 的消息日后还能重试。
                attempts.lock().unwrap().reset(&sub_id, &payload.id);
            })
        });

        let mut bus = BUS.write().unwrap();

        let listener = Arc::new(wrapped);
        *holder.write().unwrap() = Arc::downgrade(&listener);
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
        // in-flight 消息已离开队列但尚未确认，单独报出来，否则 status 会把它们当作已处理完。
        let in_flight_count = bus.queue.in_flight_count(&topic);
        let subscriber_count = bus.topics.get(&topic).map(|l| l.len()).unwrap_or(0);
        let last_activity_time = bus.last_activity.get(&topic).cloned();

        let mut extra = HashMap::new();
        extra.insert("subscriber_count".to_string(), serde_json::json!(subscriber_count));

        Ok(MailboxStatus {
            state: "online".to_string(),
            unread_count: Some(unread_count),
            in_flight_count: Some(in_flight_count),
            last_activity_time,
            extra,
        })
    }

    fn generate_id(&self) -> String {
        Uuid::new_v4().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::MailboxError;
    use crate::message::OutgoingMail;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// 轮询等待条件成立（订阅回调在 spawn 的任务里跑，回队发生在回调返回之后）。
    async fn wait_until<F: Fn() -> Fut, Fut: std::future::Future<Output = bool>>(cond: F) -> bool {
        for _ in 0..100 {
            if cond().await {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    async fn post_once(provider: &MemoryProvider, to: &Url, id: &str) -> Result<()> {
        provider.send(OutgoingMail {
            id: Some(id.to_string()),
            from: "mem:rust-bus/sender".parse()?,
            to: to.clone(),
            body: json!("content"),
            headers: HashMap::new(),
            meta: HashMap::new(),
        }.into()).await?;
        Ok(())
    }

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
                Ok(())
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

    // --- 订阅与 fetch 二选一、in-flight 可见（与 TS / Python 端对齐）---
    // BUS 是全局单例，测试并发跑，地址必须全局唯一。

    #[tokio::test]
    async fn test_subscriber_leaves_no_queued_copy() -> Result<()> {
        let provider = MemoryProvider::new();
        let address: Url = "mem:rust-bus/sub-only".parse()?;

        let (tx, rx) = tokio::sync::oneshot::channel();
        let tx = Arc::new(Mutex::new(Some(tx)));

        let _sub = provider
            .subscribe(address.clone(), Box::new(move |msg| {
                let tx = tx.clone();
                Box::pin(async move {
                    if let Some(tx) = tx.lock().unwrap().take() {
                        tx.send(msg).unwrap();
                    }
                    Ok(())
                })
            }))
            .await?;

        let mail = OutgoingMail {
            id: Some("sub1".to_string()),
            from: "mem:rust-bus/sender".parse()?,
            to: address.clone(),
            body: json!("content"),
            headers: HashMap::new(),
            meta: HashMap::new(),
        };
        provider.send(mail.into()).await?;

        let received = rx.await.unwrap();
        assert_eq!(received.body, json!("content"));

        // 旧行为：投递给监听器的同时又入队 → unread_count 永久增长
        let status = provider.status(address.clone()).await?;
        assert_eq!(status.unread_count, Some(0));
        assert!(provider.fetch(address, FetchOptions::default()).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn test_switches_back_to_queue_after_unsubscribe() -> Result<()> {
        let provider = MemoryProvider::new();
        let address: Url = "mem:rust-bus/unsub-then-fetch".parse()?;

        let mut sub = provider
            .subscribe(address.clone(), Box::new(|_| Box::pin(async { Ok(()) })))
            .await?;
        sub.unsubscribe().await?;

        // 守护“只判 topic 是否存在”的写法：退订后 topics[topic] 仍存在但是空 Vec，
        // 不判空就会静默丢弃后续消息。
        let mail = OutgoingMail {
            id: Some("after-unsub".to_string()),
            from: "mem:rust-bus/sender".parse()?,
            to: address.clone(),
            body: json!("content"),
            headers: HashMap::new(),
            meta: HashMap::new(),
        };
        provider.send(mail.into()).await?;

        let fetched = provider.fetch(address, FetchOptions::default()).await?;
        assert!(fetched.is_some());
        assert_eq!(fetched.unwrap().message.id, "after-unsub");
        Ok(())
    }

    #[tokio::test]
    async fn test_in_flight_count_visible_in_status() -> Result<()> {
        let provider = MemoryProvider::new();
        let address: Url = "mem:rust-bus/in-flight-visible".parse()?;

        let mail = OutgoingMail {
            id: Some("inflight1".to_string()),
            from: "mem:rust-bus/sender".parse()?,
            to: address.clone(),
            body: json!("content"),
            headers: HashMap::new(),
            meta: HashMap::new(),
        };
        provider.send(mail.into()).await?;

        let initial = provider.status(address.clone()).await?;
        assert_eq!(initial.unread_count, Some(1));
        assert_eq!(initial.in_flight_count, Some(0));

        let options = FetchOptions { manual_ack: true, ack_timeout: Some(5000) };
        let fetched = provider.fetch(address.clone(), options).await?.unwrap();

        // 旧行为：in-flight 消息在状态里完全隐形
        let during = provider.status(address.clone()).await?;
        assert_eq!(during.unread_count, Some(0));
        assert_eq!(during.in_flight_count, Some(1));

        fetched.ack().await?;
        let after = provider.status(address).await?;
        assert_eq!(after.in_flight_count, Some(0));
        Ok(())
    }

    #[tokio::test]
    async fn test_never_acked_message_is_recovered_after_timeout() -> Result<()> {
        let provider = MemoryProvider::new();
        let address: Url = "mem:rust-bus/in-flight-recovered".parse()?;

        let mail = OutgoingMail {
            id: Some("lost1".to_string()),
            from: "mem:rust-bus/sender".parse()?,
            to: address.clone(),
            body: json!("content"),
            headers: HashMap::new(),
            meta: HashMap::new(),
        };
        provider.send(mail.into()).await?;

        // 消费者“崩溃”：取走后不 ack
        let options = FetchOptions { manual_ack: true, ack_timeout: Some(20) };
        assert!(provider.fetch(address.clone(), options.clone()).await?.is_some());
        assert!(provider.fetch(address.clone(), options.clone()).await?.is_none());

        tokio::time::sleep(Duration::from_millis(40)).await;

        let recovered = provider.fetch(address, options).await?.unwrap();
        assert_eq!(recovered.message.id, "lost1");
        Ok(())
    }

    // --- 订阅失败处理（与 TS / Python 的 subscribe 包装回调对齐）---

    #[tokio::test]
    async fn test_subscriber_retriable_failure_returns_to_queue() -> Result<()> {
        let provider = MemoryProvider::new();
        let address: Url = "mem:rust-bus/sub-retriable".parse()?;

        let _sub = provider
            .subscribe(address.clone(), Box::new(|_| Box::pin(async {
                Err(MailboxError::Retriable("upstream unavailable".to_string()))
            })))
            .await?;

        post_once(&provider, &address, "retry1").await?;

        // 旧行为：回调返回 () ，错误无人处理 → 消息既不入队也不回队，静默消失。
        let requeued = wait_until(|| async {
            provider.status(address.clone()).await.map(|s| s.unread_count == Some(1)).unwrap_or(false)
        }).await;
        assert!(requeued, "可重试失败后消息应退回队列");

        let fetched = provider.fetch(address.clone(), FetchOptions::default()).await?;
        assert!(fetched.is_some(), "退回的消息应可被 fetch 取回");
        assert_eq!(fetched.unwrap().message.id, "retry1");
        Ok(())
    }

    #[tokio::test]
    async fn test_subscriber_transport_error_code_is_retriable() -> Result<()> {
        let provider = MemoryProvider::new();
        let address: Url = "mem:rust-bus/sub-transport".parse()?;

        let _sub = provider
            .subscribe(address.clone(), Box::new(|_| Box::pin(async {
                Err(MailboxError::Transport {
                    code: "ETIMEDOUT".to_string(),
                    message: "connection reset by peer".to_string(),
                })
            })))
            .await?;

        post_once(&provider, &address, "retry2").await?;

        // 错误码命中重试白名单（与 TS / Python 一致），同样应回队。
        let requeued = wait_until(|| async {
            provider.status(address.clone()).await.map(|s| s.unread_count == Some(1)).unwrap_or(false)
        }).await;
        assert!(requeued, "传输层错误码命中白名单时应退回队列");
        Ok(())
    }

    #[tokio::test]
    async fn test_subscriber_non_retriable_failure_is_dropped() -> Result<()> {
        let provider = MemoryProvider::new();
        let address: Url = "mem:rust-bus/sub-fatal".parse()?;

        let (tx, rx) = tokio::sync::oneshot::channel();
        let tx = Arc::new(Mutex::new(Some(tx)));

        let _sub = provider
            .subscribe(address.clone(), Box::new(move |_| {
                let tx = tx.clone();
                Box::pin(async move {
                    if let Some(tx) = tx.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                    Err(MailboxError::InvalidAddress("bad payload".to_string()))
                })
            }))
            .await?;

        post_once(&provider, &address, "dead1").await?;
        // 等回调确实跑过（而不是断言抢在投递之前）
        rx.await.unwrap();

        // 不可重试 → 按死信丢弃，避免同一条坏消息被无限重试。
        let status = provider.status(address.clone()).await?;
        assert_eq!(status.unread_count, Some(0));
        assert!(provider.fetch(address, FetchOptions::default()).await?.is_none());
        Ok(())
    }

    // --- 回队去重与订阅重试（opt-in，与 TS / Python 对齐）---

    fn failing_handler() -> SubscriptionHandler {
        Box::new(|_| Box::pin(async { Err(MailboxError::Retriable("outage".to_string())) }))
    }

    #[tokio::test]
    async fn test_multiple_failing_subscribers_leave_one_copy() -> Result<()> {
        let provider = MemoryProvider::new();
        let address: Url = "mem:rust-bus/dedup-multi-sub".parse()?;

        let _s1 = provider.subscribe(address.clone(), failing_handler()).await?;
        let _s2 = provider.subscribe(address.clone(), failing_handler()).await?;

        post_once(&provider, &address, "dup1").await?;

        // 旧行为：每个失败的订阅者各回队一份 → unread_count 为 2，同一条消息被 fetch 两次
        let settled = wait_until(|| async {
            provider.status(address.clone()).await.map(|s| s.unread_count >= Some(1)).unwrap_or(false)
        }).await;
        assert!(settled);

        let status = provider.status(address.clone()).await?;
        assert_eq!(status.unread_count, Some(1), "队列里只应留一份副本");

        let fetched = provider.fetch(address.clone(), FetchOptions::default()).await?;
        assert!(fetched.is_some());
        assert_eq!(fetched.unwrap().message.id, "dup1");
        assert!(provider.fetch(address, FetchOptions::default()).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn test_retry_targets_only_the_failing_subscriber() -> Result<()> {
        let provider = MemoryProvider::with_retry(Some(RetryPolicy::new(3, Duration::from_millis(5))));
        let address: Url = "mem:rust-bus/retry-targeted".parse()?;

        let healthy = Arc::new(AtomicUsize::new(0));
        let flaky = Arc::new(AtomicUsize::new(0));

        let h = healthy.clone();
        let _s1 = provider.subscribe(address.clone(), Box::new(move |_| {
            let h = h.clone();
            Box::pin(async move {
                h.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        })).await?;

        let f = flaky.clone();
        let _s2 = provider.subscribe(address.clone(), Box::new(move |_| {
            let f = f.clone();
            Box::pin(async move {
                let attempt = f.fetch_add(1, Ordering::SeqCst) + 1;
                if attempt < 3 {
                    return Err(MailboxError::Retriable("outage".to_string()));
                }
                Ok(())
            })
        })).await?;

        post_once(&provider, &address, "targeted1").await?;

        let recovered = wait_until(|| async { flaky.load(Ordering::SeqCst) >= 3 }).await;
        assert!(recovered, "失败订阅者应被重投直到成功");

        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(flaky.load(Ordering::SeqCst), 3);
        // 已经成功的订阅者不会被重试广播再跑一次（否则副作用执行两次）
        assert_eq!(healthy.load(Ordering::SeqCst), 1);
        assert_eq!(provider.status(address).await?.unread_count, Some(0));
        Ok(())
    }

    #[tokio::test]
    async fn test_retry_exhausted_falls_back_to_queue() -> Result<()> {
        let provider = MemoryProvider::with_retry(Some(RetryPolicy::new(2, Duration::from_millis(5))));
        let address: Url = "mem:rust-bus/retry-exhausted".parse()?;

        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let _sub = provider.subscribe(address.clone(), Box::new(move |_| {
            let c = c.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err(MailboxError::Retriable("outage".to_string()))
            })
        })).await?;

        post_once(&provider, &address, "exhausted1").await?;

        let done = wait_until(|| async { calls.load(Ordering::SeqCst) >= 2 }).await;
        assert!(done);
        tokio::time::sleep(Duration::from_millis(60)).await;

        assert_eq!(calls.load(Ordering::SeqCst), 2, "不应超过 max_attempts");
        assert_eq!(provider.status(address.clone()).await?.unread_count, Some(1));

        let fetched = provider.fetch(address, FetchOptions::default()).await?;
        assert!(fetched.is_some(), "重试耗尽后消息应退回队列，交给 fetch 兜底");
        assert_eq!(fetched.unwrap().message.id, "exhausted1");
        Ok(())
    }

    #[tokio::test]
    async fn test_retry_does_not_retry_non_retriable_error() -> Result<()> {
        let provider = MemoryProvider::with_retry(Some(RetryPolicy::new(3, Duration::from_millis(5))));
        let address: Url = "mem:rust-bus/retry-fatal".parse()?;

        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let _sub = provider.subscribe(address.clone(), Box::new(move |_| {
            let c = c.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err(MailboxError::InvalidAddress("malformed payload".to_string()))
            })
        })).await?;

        post_once(&provider, &address, "fatal1").await?;

        let done = wait_until(|| async { calls.load(Ordering::SeqCst) >= 1 }).await;
        assert!(done);
        tokio::time::sleep(Duration::from_millis(60)).await;

        assert_eq!(calls.load(Ordering::SeqCst), 1, "不可重试错误不进入重试");
        assert_eq!(provider.status(address).await?.unread_count, Some(0));
        Ok(())
    }

    #[tokio::test]
    async fn test_unsubscribe_during_pending_retry_returns_message_to_queue() -> Result<()> {
        let provider = MemoryProvider::with_retry(Some(RetryPolicy::new(5, Duration::from_millis(50))));
        let address: Url = "mem:rust-bus/retry-unsubscribed".parse()?;

        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let mut sub = provider.subscribe(address.clone(), Box::new(move |_| {
            let c = c.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err(MailboxError::Retriable("outage".to_string()))
            })
        })).await?;

        post_once(&provider, &address, "pending1").await?;
        tokio::time::sleep(Duration::from_millis(10)).await;
        sub.unsubscribe().await?;
        tokio::time::sleep(Duration::from_millis(150)).await;

        assert_eq!(calls.load(Ordering::SeqCst), 1, "退订后不应再调回调");
        // 重投被取消，但消息不能静默消失
        assert_eq!(provider.status(address.clone()).await?.unread_count, Some(1));

        let fetched = provider.fetch(address, FetchOptions::default()).await?;
        assert!(fetched.is_some());
        assert_eq!(fetched.unwrap().message.id, "pending1");
        Ok(())
    }
}
