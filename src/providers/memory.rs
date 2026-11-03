use crate::core::{DeadLetterHook, ProviderCore, ProviderCoreOptions};
use crate::delivery::DeliveryResult;
use crate::error::Result;
use crate::message::{FetchOptions, MailMessage, MailboxStatus};
use crate::provider::{
    reject_peek_in_fetch, AckableMessage, MailboxProvider, Subscription, SubscriptionHandler,
};
use crate::providers::queue::MailMessageQueue;
use crate::retry::RetryPolicy;
use crate::utils::{get_canonical_mailbox_address_identifier, normalize_peek_limit};

use async_trait::async_trait;
use futures::future::BoxFuture;
use once_cell::sync::Lazy;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use url::Url;
use uuid::Uuid;

/// 总线上的监听器 = core 的投递入口：返回本次投递的**最终结算**（含整条重试链）。
type Listener = dyn Fn(MailMessage) -> BoxFuture<'static, DeliveryResult> + Send + Sync;

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
static BUS: Lazy<Arc<RwLock<MemoryEventBus>>> = Lazy::new(|| Arc::new(RwLock::new(MemoryEventBus::new())));

fn touch(topic: &str) {
    BUS.write()
        .unwrap()
        .last_activity
        .insert(topic.to_string(), chrono::Utc::now().to_rfc3339());
}

/// 【只读】取一份队列快照（副本），不出队、不建 in-flight、不刷活动时间。
///
/// 注意：这里**不**写 `last_activity`。只读诊断不该把一个空闲地址刷成“刚刚有活动”，
/// 否则一个每隔几秒轮询一次的诊断页会让 `last_activity_time` 永远假绿。
fn peek(topic: &str, limit: Option<usize>) -> Vec<MailMessage> {
    BUS.read().unwrap().queue.peek(topic, limit)
}

/// 这份总线认不认这个 topic：有过订阅、有过活动记录，或现在 / 曾有未结算的消息。
///
/// 这是 `status().exists` 的依据。“地址不存在”与“地址空闲但确实存在”在诊断上是两件事
/// （issue#4）：前者 `exists: false` / `state: "unknown"`，后者 `exists: true` / `state: "online"`。
fn knows(bus: &MemoryEventBus, topic: &str) -> bool {
    bus.topics.contains_key(topic)
        || bus.last_activity.contains_key(topic)
        || bus.queue.has_topic(topic)
}

/// 这份总线上所有“认得”的 topic（订阅过 / 有过活动 / 有消息），按字典序排序。
/// 供 [`crate::Mailbox::snapshot`]（不传地址的 `status()`）做全量快照。
fn list_topics() -> Vec<String> {
    let bus = BUS.read().unwrap();
    let mut known: HashSet<&str> = HashSet::new();
    for topic in bus.topics.keys() {
        known.insert(topic.as_str());
    }
    for topic in bus.last_activity.keys() {
        known.insert(topic.as_str());
    }

    let mut topics: Vec<String> = known.into_iter().map(str::to_string).collect();
    topics.extend(bus.queue.topics());
    topics.sort();
    topics.dedup();
    topics
}

/// 发布一条消息：有订阅者时交给订阅回调，否则入队等待 fetch 取回。
///
/// 返回广播时每次投递的结算 future（供发送侧做结算判定，见 `_send`）；入队时返回 `None`。
/// 注意广播仍是 fire-and-forget：这里不等待，`post()` 也不会等订阅回调跑完。
fn publish(topic: &str, message: MailMessage) -> Option<Vec<BoxFuture<'static, DeliveryResult>>> {
    // 先 clone 出来再释放对 bus 的借用：监听器（core）会去拿自己的锁，绝不能持着 bus 的锁调它。
    let listeners = {
        let mut bus = BUS.write().unwrap();
        bus.last_activity
            .insert(topic.to_string(), chrono::Utc::now().to_rfc3339());
        bus.topics.get(topic).cloned().unwrap_or_default()
    };

    if listeners.is_empty() {
        // 入队，等待 fetch 取回
        BUS.write()
            .unwrap()
            .queue
            .enqueue(topic.to_string(), message);
        return None;
    }

    // 一条消息只走一条路：有订阅者就只走订阅。
    // 两条路都走时，订阅者从不 fetch → 队列只涨不消；退订后一次 fetch 又会把已处理过的
    // 消息再投递一遍（副作用执行两次）。必须判空：退订后 topics[topic] 仍存在（空 Vec）。
    //
    // 每次投递的结算要留一份给发送侧：部分失败时「整条消息回不回队」必须等全部投递结算后
    // 才能判断，不能由先失败的那个订阅者说了算。
    Some(listeners.iter().map(|listener| listener(message.clone())).collect())
}

/// 订阅模式下广播结算后的处置入口（由结算 barrier 调用一次，不在每个订阅者失败时调用）。
///
/// requeue=true（全部投递都失败）把消息退回 topic 队列头部，可被 `fetch` 取回且在
/// `unread_count` 里可见；requeue=false（有订阅者成功，或失败不可重试）按死信处理直接丢弃。
fn memory_nack_hook() -> crate::core::NackHook {
    Arc::new(|message: MailMessage, requeue: bool| {
        Box::pin(async move {
            if !requeue {
                return Ok(());
            }
            let topic = get_canonical_mailbox_address_identifier(&message.to)?;
            touch(&topic);
            BUS.write().unwrap().queue.requeue(topic, message);
            Ok(())
        })
    })
}

pub struct MemoryProvider {
    core: Arc<ProviderCore>,
}

impl Default for MemoryProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryProvider {
    pub fn new() -> Self {
        Self::with_retry(None)
    }

    /// 开启订阅重试（opt-in）。不传则保持默认：订阅失败只把消息退回队列，不自动重投。
    pub fn with_retry(policy: Option<RetryPolicy>) -> Self {
        Self::with_core_options(ProviderCoreOptions {
            retry: policy,
            ..ProviderCoreOptions::default()
        })
    }

    fn with_core_options(options: ProviderCoreOptions) -> Self {
        let options = ProviderCoreOptions {
            broadcast: true, // 订阅是广播：一条消息对每个订阅者各投递一次
            nack: Some(memory_nack_hook()),
            ..options
        };
        Self {
            core: ProviderCore::new("mem", options),
        }
    }

    /// 开启 / 关闭订阅重试。传 `None` 回到默认行为（失败只回队、不自动重投）。
    pub fn set_retry_policy(&self, policy: Option<RetryPolicy>) {
        self.core.set_retry_policy(policy);
    }

    /// 单次投递尝试的结算超时；`Duration::ZERO` 关闭看门狗。
    pub fn delivery_settle_timeout(&self) -> Duration {
        self.core.delivery_settle_timeout()
    }

    pub fn set_delivery_settle_timeout(&self, timeout: Duration) {
        self.core.set_delivery_settle_timeout(timeout);
    }

    /// 当前尚未结算的投递数量（诊断 / 测试用）。
    pub fn pending_delivery_count(&self) -> usize {
        self.core.pending_delivery_count()
    }

    /// 不回队的那些失败投递在这里可见（写死信队列 / 告警）。
    pub fn set_dead_letter_hook(&self, hook: Option<DeadLetterHook>) {
        self.core.set_dead_letter_hook(hook);
    }

    pub fn set_retry_exhausted_hook(&self, hook: Option<crate::core::RetryExhaustedHook>) {
        self.core.set_retry_exhausted_hook(hook);
    }

    /// 自己组装消息投给本地订阅者（生产者型 provider 的入口）。走与 `subscribe` 完全相同的
    /// 投递记账；直调用户回调等于这次投递没人记账。
    pub fn deliver_to_local_subscription(
        &self,
        subscription_id: &str,
        message: MailMessage,
    ) -> BoxFuture<'static, DeliveryResult> {
        self.core
            .deliver_to_local_subscription(subscription_id, message)
    }
}

#[async_trait]
impl MailboxProvider for MemoryProvider {
    fn protocol(&self) -> &str {
        self.core.protocol()
    }

    async fn close(&self) -> Result<()> {
        // 取消全部活跃订阅 + 待重投的投递，最后清零台账。
        self.core.close().await;
        Ok(())
    }

    ///
    /// 发送并给广播挂上结算 barrier：等这条消息的全部投递（含重试链）落定后，才决定要不要
    /// 把它退回队列（判定在 `delivery::should_requeue_after_broadcast`）。
    ///
    /// barrier 必须同步登记（任何 await 之前），否则派发时同步失败的投递会先看到“无 barrier”。
    async fn _send(&self, message: MailMessage) -> Result<()> {
        let topic = get_canonical_mailbox_address_identifier(&message.to)?;
        let deliveries = match publish(&topic, message.clone()) {
            Some(deliveries) => deliveries,
            None => return Ok(()), // 没有订阅者：消息入队，由 fetch / ack 那套语义负责
        };
        self.core.track_broadcast_settlement(message, deliveries);
        Ok(())
    }

    async fn subscribe(
        &self,
        address: Url,
        callback: SubscriptionHandler,
    ) -> Result<Box<dyn Subscription>> {
        let topic = get_canonical_mailbox_address_identifier(&address)?;

        // 对齐 TS / Python 的 MailboxProvider.subscribe：core 保存原始回调并驱动隐式 ack、
        // 错误处理、重试与投递结算；这里只负责把 core 给的监听器挂到总线上。
        let (sub_id, listener) = self.core.register_subscription(address, callback);

        {
            let mut bus = BUS.write().unwrap();
            bus.topics
                .entry(topic.clone())
                .or_default()
                .push(listener.clone());
            bus.last_activity
                .insert(topic.clone(), chrono::Utc::now().to_rfc3339());
        }

        let topic_for_unsub = topic.clone();
        self.core.attach_unsubscribe(
            &sub_id,
            Box::new(move || {
                let topic = topic_for_unsub.clone();
                let listener = listener.clone();
                Box::pin(async move {
                    if let Some(listeners) = BUS.write().unwrap().topics.get_mut(&topic) {
                        listeners.retain(|l| !Arc::ptr_eq(l, &listener));
                    }
                    Ok(())
                })
            }),
        );

        Ok(self.core.subscription_object(sub_id))
    }

    async fn fetch(&self, address: Url, options: FetchOptions) -> Result<Option<AckableMessage>> {
        // 入口先挡住 `peek: true`：不能把只读快照当消费型取件跑。
        reject_peek_in_fetch(&options)?;

        let topic = get_canonical_mailbox_address_identifier(&address)?;
        touch(&topic);

        if !options.manual_ack {
            if let Some(msg) = BUS.write().unwrap().queue.dequeue(&topic) {
                return Ok(Some(AckableMessage {
                    message: msg,
                    ack: Box::new(|| Box::pin(async { Ok(()) })),
                    nack: Box::new(|_| Box::pin(async { Ok(()) })),
                }));
            }
            return Ok(None);
        }

        let timeout = options.ack_timeout.map(Duration::from_millis);
        if let Some(msg) = BUS.write().unwrap().queue.dequeue_for_ack(&topic, timeout) {
            let msg_id = msg.id.clone();
            let msg_id_nack = msg.id.clone();

            return Ok(Some(AckableMessage {
                message: msg,
                ack: Box::new(move || Box::pin(async move {
                    BUS.write().unwrap().queue.ack(&msg_id);
                    Ok(())
                })),
                nack: Box::new(move |requeue| Box::pin(async move {
                    BUS.write().unwrap().queue.nack(&msg_id_nack, requeue);
                    Ok(())
                })),
            }));
        }

        Ok(None)
    }

    /// 只读快照：副本，不动队列、不建 in-flight、不刷活动时间。
    ///
    /// 之前没有这个能力：想“看一眼”只能 `fetch(..., manual_ack: true)` 不 ack，消息就进了
    /// in-flight（默认 30s 后重投），既干扰正常消费者的计数，也不是只读（issue#4）。
    async fn peek(&self, address: Url, limit: Option<usize>) -> Result<Vec<MailMessage>> {
        let topic = get_canonical_mailbox_address_identifier(&address)?;
        Ok(peek(&topic, normalize_peek_limit(limit)))
    }

    /// 本总线认得的地址（canonical id）—— `mem:` 的消息就在本地，所以能枚举。
    async fn list_addresses(&self) -> Result<Vec<String>> {
        Ok(list_topics())
    }

    async fn status(&self, address: Url) -> Result<MailboxStatus> {
        let topic = get_canonical_mailbox_address_identifier(&address)?;
        let bus = BUS.read().unwrap();

        let exists = knows(&bus, &topic);
        let unread_count = bus.queue.get_status(&topic);
        // in-flight 消息已离开队列但尚未确认，单独报出来，否则 status 会把它们当作已处理完。
        let in_flight_count = bus.queue.in_flight_count(&topic);
        let subscriber_count = bus.topics.get(&topic).map(|l| l.len()).unwrap_or(0);
        // 只读：这里不写 `last_activity`（诊断页轮询不该把空闲地址刷成“刚刚有活动”）。
        let last_activity_time = bus.last_activity.get(&topic).cloned();
        drop(bus);

        let mut extra = HashMap::new();
        extra.insert("subscriber_count".to_string(), serde_json::json!(subscriber_count));

        if !exists {
            // 对“这个地址从不存在”回 `online` + 0，诊断页就分不出「不存在」和「空闲」（issue#4）。
            extra.insert(
                "message".to_string(),
                serde_json::json!(format!(
                    "No queue, subscription or activity is known for '{}'.",
                    topic
                )),
            );

            return Ok(MailboxStatus {
                state: "unknown".to_string(),
                exists: Some(false),
                unread_count: Some(unread_count),
                in_flight_count: Some(in_flight_count),
                last_activity_time,
                extra,
            });
        }

        Ok(MailboxStatus {
            state: "online".to_string(),
            exists: Some(true),
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
    use crate::core::ProviderCore;
    use crate::delivery::FailureReason;
    use crate::error::MailboxError;
    use crate::message::OutgoingMail;
    use crate::provider::SubscriptionStatus;
    use serde_json::{json, Value};
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
    async fn test_send_injects_sent_at_header() -> Result<()> {
        // Provider 的契约（`interfaces.ts` / 三端一致）：发送时注入 `mbx-sent-at`。
        let provider = MemoryProvider::new();
        let sent = provider
            .send(
                OutgoingMail {
                    id: Some("hdr1".to_string()),
                    from: "mem:test/sender".parse()?,
                    to: "mem:rust-bus/headers".parse()?,
                    body: json!("content"),
                    headers: HashMap::new(),
                    meta: HashMap::new(),
                }
                .into(),
            )
            .await?;

        assert!(sent.headers.contains_key("mbx-sent-at"));
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
            ..Default::default()
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
            ..Default::default()
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

        let options = FetchOptions { manual_ack: true, ack_timeout: Some(5000), ..Default::default() };
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
        let options = FetchOptions { manual_ack: true, ack_timeout: Some(20), ..Default::default() };
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

    // ===================================================================
    // 投递级结算（issue#2）—— 与 TS `mailbox.subscribe-partial-failure.test.ts`
    // / `mailbox.delivery-settlement.test.ts` 同一组不变量。
    //
    //   [语义] 必须成立的投递语义，破坏它即回归；[事实] 附带锁定的实现约定。
    // ===================================================================

    static TOPIC_SEQ: AtomicUsize = AtomicUsize::new(0);

    /// MemoryEventBus 是进程级单例：每个用例必须用唯一 topic。
    fn addr(name: &str) -> Url {
        format!(
            "mem:rust-bus/{}-{}/inbox",
            name,
            TOPIC_SEQ.fetch_add(1, Ordering::SeqCst)
        )
        .parse()
        .unwrap()
    }

    type DeadLetterLog = Arc<Mutex<Vec<(MailMessage, Vec<DeliveryResult>)>>>;

    /// 观察死信钩子（= barrier 已结算的信号）。
    #[derive(Clone, Default)]
    struct DeadLetters {
        entries: DeadLetterLog,
    }

    impl DeadLetters {
        fn hook(&self) -> DeadLetterHook {
            let entries = self.entries.clone();
            Arc::new(move |message: MailMessage, failed: Vec<DeliveryResult>| {
                entries.lock().unwrap().push((message, failed));
            })
        }
        fn len(&self) -> usize {
            self.entries.lock().unwrap().len()
        }
        fn take(&self, index: usize) -> (MailMessage, Vec<DeliveryResult>) {
            self.entries.lock().unwrap()[index].clone()
        }
    }

    fn counter() -> Arc<AtomicUsize> {
        Arc::new(AtomicUsize::new(0))
    }

    fn counting_ok(calls: &Arc<AtomicUsize>) -> SubscriptionHandler {
        let calls = calls.clone();
        Box::new(move |_| {
            let calls = calls.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        })
    }

    /// [语义] A 成功 + B 可重试失败 → 整条消息不回队（部分失败不重复投递）
    #[tokio::test]
    async fn test_partial_failure_does_not_requeue() -> Result<()> {
        let provider = MemoryProvider::new();
        let address = addr("partial-no-requeue");
        let dead = DeadLetters::default();
        provider.set_dead_letter_hook(Some(dead.hook()));

        let ok_calls = counter();
        let _ok = provider.subscribe(address.clone(), counting_ok(&ok_calls)).await?;
        let _bad = provider.subscribe(address.clone(), failing_handler()).await?;

        post_once(&provider, &address, "partial1").await?;

        // 死信钩子只在 barrier 结算后触发 —— 拿它当「广播已判定」的信号，不用睡固定时长
        assert!(
            wait_until(|| async { dead.len() >= 1 }).await,
            "不回队的失败投递应能在死信钩子里看到"
        );
        assert_eq!(ok_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.status(address.clone()).await?.unread_count, Some(0));
        assert!(provider.fetch(address, FetchOptions::default()).await?.is_none());
        Ok(())
    }

    /// [语义] 不回队的失败投递带投递身份（subscription + message），可写死信队列 / 告警
    #[tokio::test]
    async fn test_dead_letter_hook_reports_the_failed_delivery() -> Result<()> {
        let provider = MemoryProvider::new();
        let address = addr("dead-letter-identity");
        let dead = DeadLetters::default();
        provider.set_dead_letter_hook(Some(dead.hook()));

        let ok_sub = provider.subscribe(address.clone(), counting_ok(&counter())).await?;
        let bad_sub = provider.subscribe(address.clone(), failing_handler()).await?;

        post_once(&provider, &address, "dl1").await?;
        assert!(wait_until(|| async { dead.len() >= 1 }).await);

        let (message, failed) = dead.take(0);
        assert_eq!(message.id, "dl1");
        assert_eq!(failed.len(), 1, "只有失败的那一次投递被记账");
        assert_eq!(failed[0].subscription_id, bad_sub.id());
        assert_eq!(failed[0].reason, Some(FailureReason::Retriable));
        assert!(failed[0].delivery_id.contains(bad_sub.id()));
        assert_eq!(ok_sub.status(), SubscriptionStatus::Active);
        Ok(())
    }

    /// [语义] 混合 subscribe + 轮询：同一条消息的副作用只执行一次
    #[tokio::test]
    async fn test_mixed_subscriber_and_poller_run_side_effect_once() -> Result<()> {
        let provider = MemoryProvider::new();
        let address = addr("single-side-effect");
        let side_effects = counter();

        let _ok = provider
            .subscribe(address.clone(), counting_ok(&side_effects))
            .await?;
        let _bad = provider.subscribe(address.clone(), failing_handler()).await?;

        post_once(&provider, &address, "mixed1").await?;

        // 轮询消费者：拿不到已广播过的消息，因此不会重跑副作用
        assert!(provider.fetch(address.clone(), FetchOptions::default()).await?.is_none());
        assert!(wait_until(|| async { provider.pending_delivery_count() == 0 }).await);
        assert_eq!(side_effects.load(Ordering::SeqCst), 1);
        Ok(())
    }

    /// [语义] 可重试 + 不可重试混合且无人成功 → 仍回队（可重试那份值得再试）
    #[tokio::test]
    async fn test_mixed_fatal_and_retriable_still_requeues() -> Result<()> {
        let provider = MemoryProvider::new();
        let address = addr("mixed-fatal-retriable");

        let _fatal = provider
            .subscribe(address.clone(), Box::new(|_| Box::pin(async {
                Err(MailboxError::InvalidAddress("malformed payload".to_string()))
            })))
            .await?;
        let _retriable = provider.subscribe(address.clone(), failing_handler()).await?;

        post_once(&provider, &address, "mixed2").await?;

        assert!(wait_until(|| async {
            provider.status(address.clone()).await.map(|s| s.unread_count == Some(1)).unwrap_or(false)
        })
        .await);
        let fetched = provider.fetch(address, FetchOptions::default()).await?;
        assert_eq!(fetched.unwrap().message.id, "mixed2");
        Ok(())
    }

    /// [语义] 重试耗尽也不把已成功者处理过的消息退回队列（只有失败者被重投）
    #[tokio::test]
    async fn test_retry_exhausted_with_partial_success_does_not_requeue() -> Result<()> {
        let provider = MemoryProvider::with_retry(Some(RetryPolicy::new(3, Duration::from_millis(1))));
        let address = addr("exhaust-partial");
        let dead = DeadLetters::default();
        provider.set_dead_letter_hook(Some(dead.hook()));

        let ok_calls = counter();
        let bad_calls = counter();
        let bad_for_cb = bad_calls.clone();
        let _ok = provider.subscribe(address.clone(), counting_ok(&ok_calls)).await?;
        let _bad = provider
            .subscribe(address.clone(), Box::new(move |_| {
                let bad_calls = bad_for_cb.clone();
                Box::pin(async move {
                    bad_calls.fetch_add(1, Ordering::SeqCst);
                    Err(MailboxError::Retriable("outage".to_string()))
                })
            }))
            .await?;

        post_once(&provider, &address, "exhaust2").await?;

        assert!(wait_until(|| async { dead.len() >= 1 }).await);
        assert_eq!(ok_calls.load(Ordering::SeqCst), 1, "已成功者不得被重试广播再跑一次");
        assert_eq!(bad_calls.load(Ordering::SeqCst), 3);
        assert_eq!(provider.status(address).await?.unread_count, Some(0));
        Ok(())
    }

    /// [语义] 唯一的失败订阅者在待重投期间退订 → 有成功者时不回队
    #[tokio::test]
    async fn test_cancelling_the_only_failing_subscriber_keeps_message_out_of_queue() -> Result<()> {
        let provider = MemoryProvider::with_retry(Some(RetryPolicy::new(3, Duration::from_millis(500))));
        let address = addr("cancel-partial");

        let ok_calls = counter();
        let _ok = provider.subscribe(address.clone(), counting_ok(&ok_calls)).await?;
        let mut bad_sub = provider.subscribe(address.clone(), failing_handler()).await?;

        post_once(&provider, &address, "cancel1").await?;
        tokio::time::sleep(Duration::from_millis(10)).await;
        bad_sub.unsubscribe().await?; // 取消待重投：投递结算为 cancelled

        assert!(wait_until(|| async { provider.pending_delivery_count() == 0 }).await);
        assert_eq!(ok_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.status(address).await?.unread_count, Some(0));
        Ok(())
    }

    /// [事实] 慢速成功订阅者还没结算时，判定还没发生（不会提前回队）
    #[tokio::test]
    async fn test_barrier_waits_for_the_slow_success() -> Result<()> {
        let provider = MemoryProvider::new();
        let address = addr("barrier-waits-slow-success");
        let settled = counter();

        let slow = settled.clone();
        let _slow = provider
            .subscribe(address.clone(), Box::new(move |_| {
                let slow = slow.clone();
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_millis(60)).await;
                    slow.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            }))
            .await?;
        let _fast = provider.subscribe(address.clone(), failing_handler()).await?;

        post_once(&provider, &address, "slow1").await?;
        tokio::time::sleep(Duration::from_millis(20)).await;

        // 旧行为：先失败的订阅者立刻回队 —— 而成功与否此刻还没发生
        assert_eq!(settled.load(Ordering::SeqCst), 0);
        assert_eq!(provider.status(address.clone()).await?.unread_count, Some(0));

        assert!(wait_until(|| async { settled.load(Ordering::SeqCst) == 1 }).await);
        assert_eq!(provider.status(address).await?.unread_count, Some(0));
        Ok(())
    }

    /// [事实] post() 只等派发，不等投递结算
    #[tokio::test]
    async fn test_send_does_not_await_delivery_settlement() -> Result<()> {
        let provider = MemoryProvider::new();
        let address = addr("post-not-awaiting-delivery");
        let settled = counter();

        let slow = settled.clone();
        let _sub = provider
            .subscribe(address.clone(), Box::new(move |_| {
                let slow = slow.clone();
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    slow.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            }))
            .await?;

        post_once(&provider, &address, "not-awaiting").await?;
        assert_eq!(settled.load(Ordering::SeqCst), 0, "广播已派发，但未结算");

        assert!(wait_until(|| async { settled.load(Ordering::SeqCst) == 1 }).await);
        Ok(())
    }

    /// [语义] 退订时正在执行的回调不得被当成失败（不回队、副作用一次）
    #[tokio::test]
    async fn test_unsubscribe_does_not_conclude_for_an_in_flight_delivery() -> Result<()> {
        let provider = MemoryProvider::new();
        let address = addr("unsubscribe-inflight");
        let side_effects = counter();

        let slow = side_effects.clone();
        let mut sub = provider
            .subscribe(address.clone(), Box::new(move |_| {
                let slow = slow.clone();
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    slow.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            }))
            .await?;

        post_once(&provider, &address, "inflight1").await?;
        tokio::time::sleep(Duration::from_millis(10)).await;
        sub.unsubscribe().await?; // 回调还在跑：不能替它结算成 cancelled

        assert!(wait_until(|| async { provider.pending_delivery_count() == 0 }).await);
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(side_effects.load(Ordering::SeqCst), 1);
        assert_eq!(provider.status(address.clone()).await?.unread_count, Some(0));
        assert!(provider.fetch(address, FetchOptions::default()).await?.is_none());
        Ok(())
    }

    /// [语义] 退订时正在执行的回调如果最终失败，仍可被 fetch 兜底
    #[tokio::test]
    async fn test_in_flight_delivery_that_fails_after_unsubscribe_is_requeued() -> Result<()> {
        let provider = MemoryProvider::new();
        let address = addr("unsubscribe-inflight-fail");

        let mut sub = provider
            .subscribe(address.clone(), Box::new(|_| Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(30)).await;
                Err(MailboxError::Retriable("upstream down".to_string()))
            })))
            .await?;

        post_once(&provider, &address, "inflight2").await?;
        tokio::time::sleep(Duration::from_millis(10)).await;
        sub.unsubscribe().await?; // 回调还在跑，随后才抛错：没有任何订阅者成功 → 交给 fetch()

        assert!(wait_until(|| async {
            provider.status(address.clone()).await.map(|s| s.unread_count == Some(1)).unwrap_or(false)
        })
        .await);
        let fetched = provider.fetch(address, FetchOptions::default()).await?;
        assert_eq!(fetched.unwrap().message.id, "inflight2");
        Ok(())
    }

    /// [语义] 退订仍会取消「等待重投」的投递：退避中的投递按取消结算并回队
    #[tokio::test]
    async fn test_unsubscribe_during_backoff_settles_the_delivery() -> Result<()> {
        let provider = MemoryProvider::with_retry(Some(RetryPolicy::new(5, Duration::from_millis(500))));
        let address = addr("unsubscribe-during-backoff");

        let mut sub = provider.subscribe(address.clone(), failing_handler()).await?;
        post_once(&provider, &address, "backoff1").await?;

        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(provider.pending_delivery_count(), 1, "退避中的投递仍未结算");
        sub.unsubscribe().await?;

        assert!(wait_until(|| async { provider.pending_delivery_count() == 0 }).await, "退避定时器已取消并结算");
        assert!(wait_until(|| async {
            provider.status(address.clone()).await.map(|s| s.unread_count == Some(1)).unwrap_or(false)
        })
        .await, "取消的重投必须退回队列");
        let fetched = provider.fetch(address, FetchOptions::default()).await?;
        assert_eq!(fetched.unwrap().message.id, "backoff1");
        Ok(())
    }

    /// [语义] 同 id 并发投递各自独立结算：两条投递都不得丢
    #[tokio::test]
    async fn test_concurrent_posts_with_the_same_id_settle_independently() -> Result<()> {
        let provider = MemoryProvider::new();
        let address = addr("dup-id-fatal");
        let dead = DeadLetters::default();
        provider.set_dead_letter_hook(Some(dead.hook()));

        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_for_cb = seen.clone();
        let _sub = provider
            .subscribe(address.clone(), Box::new(move |message| {
                let seen = seen_for_cb.clone();
                Box::pin(async move {
                    seen.lock().unwrap().push(message.body["n"].as_u64().unwrap());
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    Err(MailboxError::InvalidAddress("malformed payload".to_string()))
                })
            }))
            .await?;

        post_twice_with_same_id(&provider, &address, "dup-1").await?;

        // 修复前：两条投递共用记账键，只有一条 barrier 能跑到死信钩子
        assert!(wait_until(|| async { dead.len() >= 2 }).await, "两条投递各自结算，各自的 barrier 都跑完");
        assert_eq!(provider.pending_delivery_count(), 0);
        assert_eq!(*seen.lock().unwrap(), vec![1, 2]);
        let mut ids: Vec<String> = (0..2).map(|i| dead.take(i).0.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 1, "同一个消息 id 的两条投递都要被判定");
        Ok(())
    }

    /// [语义] 同 id 并发投递全部可重试失败时，队列仍只留一份
    #[tokio::test]
    async fn test_concurrent_posts_with_the_same_id_queue_one_copy() -> Result<()> {
        let provider = MemoryProvider::new();
        let address = addr("dup-id-retriable");
        let calls = counter();

        let c = calls.clone();
        let _sub = provider
            .subscribe(address.clone(), Box::new(move |_| {
                let c = c.clone();
                Box::pin(async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    Err(MailboxError::Retriable("upstream down".to_string()))
                })
            }))
            .await?;

        post_twice_with_same_id(&provider, &address, "dup-2").await?;

        assert!(wait_until(|| async { calls.load(Ordering::SeqCst) >= 2 }).await);
        assert!(wait_until(|| async { provider.pending_delivery_count() == 0 }).await);
        assert_eq!(provider.status(address.clone()).await?.unread_count, Some(1), "never queued twice");
        assert!(provider.fetch(address.clone(), FetchOptions::default()).await?.is_some());
        assert!(provider.fetch(address, FetchOptions::default()).await?.is_none());
        Ok(())
    }

    async fn post_twice_with_same_id(provider: &MemoryProvider, to: &Url, id: &str) -> Result<()> {
        for n in 1..=2 {
            provider
                .send(MailMessage {
                    id: id.to_string(),
                    from: "mem:rust-bus/sender".parse()?,
                    to: to.clone(),
                    body: json!({ "n": n }),
                    headers: HashMap::new(),
                    meta: HashMap::new(),
                })
                .await?;
        }
        Ok(())
    }

    // --- 结算看门狗：卡死的回调必须有界 ---

    fn hung_handler() -> SubscriptionHandler {
        Box::new(|_| Box::pin(async move {
            futures::future::pending::<()>().await;
            Ok(())
        }))
    }

    /// [语义] 永不 settle 的回调不得让消息永久不可见（看门狗兜底回队）
    #[tokio::test]
    async fn test_hung_callback_is_bounded_by_the_watchdog() -> Result<()> {
        let provider = MemoryProvider::new();
        provider.set_delivery_settle_timeout(Duration::from_millis(20));
        let address = addr("hung-callback");

        let _sub = provider.subscribe(address.clone(), hung_handler()).await?;
        post_once(&provider, &address, "hung1").await?;

        assert!(wait_until(|| async { provider.pending_delivery_count() == 0 }).await);
        assert_eq!(provider.status(address.clone()).await?.unread_count, Some(1));
        let fetched = provider.fetch(address, FetchOptions::default()).await?;
        assert_eq!(fetched.unwrap().message.id, "hung1");
        Ok(())
    }

    /// [语义] 看门狗超时只结算一次：回调后来才成功不得再兜一遍
    #[tokio::test]
    async fn test_late_success_after_a_stall_is_not_settled_twice() -> Result<()> {
        let provider = MemoryProvider::new();
        provider.set_delivery_settle_timeout(Duration::from_millis(20));
        let address = addr("late-success-after-stall");
        let dead = DeadLetters::default();
        provider.set_dead_letter_hook(Some(dead.hook()));

        let side_effects = counter();
        let slow = side_effects.clone();
        let _sub = provider
            .subscribe(address.clone(), Box::new(move |_| {
                let slow = slow.clone();
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_millis(80)).await; // 比看门狗晚
                    slow.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            }))
            .await?;

        post_once(&provider, &address, "late1").await?;

        assert!(wait_until(|| async { provider.pending_delivery_count() == 0 }).await);
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(side_effects.load(Ordering::SeqCst), 1);
        assert_eq!(dead.len(), 0);
        // 只有看门狗那一次回队；事后成功不会再把它投一遍
        assert_eq!(provider.status(address.clone()).await?.unread_count, Some(1));
        assert!(provider.fetch(address, FetchOptions::default()).await?.is_some());
        Ok(())
    }

    /// [语义] 看门狗不得打断健康的重试链（每次尝试各自计时）
    #[tokio::test]
    async fn test_watchdog_does_not_cut_a_healthy_retry_chain() -> Result<()> {
        let provider = MemoryProvider::with_retry(Some(RetryPolicy::new(5, Duration::from_millis(5))));
        provider.set_delivery_settle_timeout(Duration::from_millis(60));
        let address = addr("watchdog-not-cutting-retry");
        let calls = counter();

        let c = calls.clone();
        let _sub = provider
            .subscribe(address.clone(), Box::new(move |_| {
                let c = c.clone();
                Box::pin(async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(20)).await; // 单次尝试 < 看门狗，整条链 > 看门狗
                    if c.load(Ordering::SeqCst) < 3 {
                        return Err(MailboxError::Retriable("upstream down".to_string()));
                    }
                    Ok(())
                })
            }))
            .await?;

        post_once(&provider, &address, "retrychain1").await?;

        assert!(wait_until(|| async { provider.pending_delivery_count() == 0 }).await);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(provider.status(address).await?.unread_count, Some(0));
        Ok(())
    }

    /// [语义] 看门狗不得打断退避等待（退避期间不计时）
    #[tokio::test]
    async fn test_watchdog_does_not_cut_the_backoff() -> Result<()> {
        let provider = MemoryProvider::with_retry(Some(RetryPolicy::new(3, Duration::from_millis(80))));
        provider.set_delivery_settle_timeout(Duration::from_millis(20));
        let address = addr("watchdog-not-cutting-backoff");
        let calls = counter();

        let c = calls.clone();
        let _sub = provider
            .subscribe(address.clone(), Box::new(move |_| {
                let c = c.clone();
                Box::pin(async move {
                    if c.fetch_add(1, Ordering::SeqCst) < 1 {
                        return Err(MailboxError::Retriable("upstream down".to_string()));
                    }
                    Ok(())
                })
            }))
            .await?;

        post_once(&provider, &address, "backoff2").await?;

        assert!(wait_until(|| async { provider.pending_delivery_count() == 0 }).await);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "退避 80ms > 看门狗 20ms，但不能被掐断");
        assert_eq!(provider.status(address).await?.unread_count, Some(0));
        Ok(())
    }

    /// [事实] 超时设为 ZERO 关闭看门狗：无人结算的消息没人兜
    #[tokio::test]
    async fn test_watchdog_can_be_disabled() -> Result<()> {
        let provider = MemoryProvider::new();
        provider.set_delivery_settle_timeout(Duration::ZERO);
        assert_eq!(provider.delivery_settle_timeout(), Duration::ZERO);
        let address = addr("watchdog-off");

        let _sub = provider.subscribe(address.clone(), hung_handler()).await?;
        post_once(&provider, &address, "hung2").await?;

        tokio::time::sleep(Duration::from_millis(100)).await;
        // 关掉看门狗 = 调用方自己保证回调会结算；否则这条消息既不在队列也不在死信里。
        // 也不会替正在执行的回调下结论（否则事后成功的副作用会被再执行一次）。
        assert_eq!(provider.pending_delivery_count(), 1);
        assert_eq!(provider.status(address).await?.unread_count, Some(0));
        Ok(())
    }

    /// [事实] 默认看门狗存在且有界
    #[test]
    fn test_default_settle_timeout_is_configured() {
        let provider = MemoryProvider::new();
        assert_eq!(
            provider.delivery_settle_timeout(),
            crate::delivery::DEFAULT_DELIVERY_SETTLE_TIMEOUT
        );
    }

    /// [语义] 广播 provider 没接结算 barrier 时不得静默丢失（基类兜底 + 告警）
    #[tokio::test]
    async fn test_provider_without_barrier_still_requeues() -> Result<()> {
        let provider = RawBroadcastProvider::new();
        let address: Url = "raw://bus/no-barrier".parse()?;

        let _sub = provider.subscribe(address.clone(), failing_handler()).await?;
        provider
            .send(MailMessage {
                id: "raw1".to_string(),
                from: "raw://s@bus/u".parse()?,
                to: address.clone(),
                body: json!({ "n": 1 }),
                headers: HashMap::new(),
                meta: HashMap::new(),
            })
            .await?;

        assert!(wait_until(|| async { provider.queued(&address) == 1 }).await, "可重试失败必须仍可被 fetch 取回");
        let fetched = provider.fetch(address, FetchOptions::default()).await?;
        assert_eq!(fetched.unwrap().message.id, "raw1");
        Ok(())
    }

    /// [语义] 自己组装消息投给本地订阅者：同样记账，失败可被 fetch 兜底
    #[tokio::test]
    async fn test_local_delivery_is_bookkept_like_a_broadcast() -> Result<()> {
        let provider = MemoryProvider::new();
        let address = addr("local-producer");

        let sub = provider.subscribe(address.clone(), failing_handler()).await?;
        let reply = MailMessage {
            id: "reply-1".to_string(),
            from: "mem:rust-bus/sender".parse()?,
            to: address.clone(),
            body: json!({ "produced": true }),
            headers: HashMap::new(),
            meta: HashMap::new(),
        };
        let result = provider.deliver_to_local_subscription(sub.id(), reply).await;

        assert!(!result.ok);
        assert_eq!(provider.pending_delivery_count(), 0, "这次投递被记账并结算了");
        assert!(wait_until(|| async {
            provider.status(address.clone()).await.map(|s| s.unread_count == Some(1)).unwrap_or(false)
        })
        .await, "本地投递的失败同样要能被 fetch 兜底");
        let fetched = provider.fetch(address, FetchOptions::default()).await?;
        assert_eq!(fetched.unwrap().message.id, "reply-1");
        Ok(())
    }

    /// [语义] 关闭（stop）时正在执行的回调成功后不得回队
    #[tokio::test]
    async fn test_close_does_not_conclude_for_an_in_flight_delivery() -> Result<()> {
        let provider = MemoryProvider::new();
        let address = addr("close-inflight");
        let side_effects = counter();

        let slow = side_effects.clone();
        let _sub = provider
            .subscribe(address.clone(), Box::new(move |_| {
                let slow = slow.clone();
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    slow.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            }))
            .await?;

        post_once(&provider, &address, "close1").await?;
        tokio::time::sleep(Duration::from_millis(10)).await;
        provider.close().await?; // Mailbox::stop() 走这里

        assert!(wait_until(|| async { provider.pending_delivery_count() == 0 }).await);
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(side_effects.load(Ordering::SeqCst), 1);
        assert_eq!(provider.status(address.clone()).await?.unread_count, Some(0));
        // close 自动退订：总线上的监听器也被摘掉
        assert_eq!(provider.status(address).await?.extra.get("subscriber_count"), Some(&json!(0)));
        Ok(())
    }

    /// [语义] 关闭会取消「等待重投」的投递：消息退回队列，回调不再被调
    #[tokio::test]
    async fn test_close_cancels_pending_retries() -> Result<()> {
        let provider = MemoryProvider::with_retry(Some(RetryPolicy::new(5, Duration::from_millis(50))));
        let address = addr("close-during-backoff");
        let calls = counter();

        let c = calls.clone();
        let _sub = provider
            .subscribe(address.clone(), Box::new(move |_| {
                let c = c.clone();
                Box::pin(async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Err(MailboxError::Retriable("outage".to_string()))
                })
            }))
            .await?;

        post_once(&provider, &address, "close2").await?;
        tokio::time::sleep(Duration::from_millis(10)).await;
        provider.close().await?;

        assert!(wait_until(|| async { provider.pending_delivery_count() == 0 }).await);
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "进程都在收尾，不再把消息重新喂回回调");
        let fetched = provider.fetch(address, FetchOptions::default()).await?;
        assert_eq!(fetched.unwrap().message.id, "close2", "取消不等于消息可以消失");
        Ok(())
    }

    /// [事实] 重试耗尽钩子：预算用尽时把投递身份交给业务（写死信 / 告警）
    #[tokio::test]
    async fn test_retry_exhausted_hook_fires() -> Result<()> {
        let provider = MemoryProvider::with_retry(Some(RetryPolicy::new(2, Duration::from_millis(1))));
        let address = addr("retry-exhausted-hook");
        let exhausted = counter();

        let fired = exhausted.clone();
        provider.set_retry_exhausted_hook(Some(Arc::new(move |message: MailMessage, attempts: u32| {
            assert_eq!(message.id, "exhausted2");
            assert_eq!(attempts, 2);
            fired.fetch_add(1, Ordering::SeqCst);
        })));

        let _sub = provider.subscribe(address.clone(), failing_handler()).await?;
        post_once(&provider, &address, "exhausted2").await?;

        assert!(wait_until(|| async { exhausted.load(Ordering::SeqCst) >= 1 }).await);
        assert_eq!(provider.status(address).await?.unread_count, Some(1));
        Ok(())
    }

    // --- issue#4：地址规则 / 诚实的 status / 只读 peek -------------------------------------
    //
    // 与 TS `src/mailbox.peek.test.ts` + `src/mailbox.status-diagnostics.test.ts` +
    // `src/mailbox.address-canonicalization.test.ts`、Python `tests/test_*.py` 同一组不变量。
    // 注：`mem:` 的总线仍是进程级单例（issue#3 的实例隔离未背移到 Rust），所以每个测试用
    // 自己唯一的 topic 名。

    async fn post_body(provider: &MemoryProvider, to: &Url, id: &str, body: Value) -> Result<()> {
        provider
            .send(OutgoingMail {
                id: Some(id.to_string()),
                from: "mem:rust-bus/sender".parse()?,
                to: to.clone(),
                body,
                headers: HashMap::new(),
                meta: HashMap::new(),
            }.into())
            .await?;
        Ok(())
    }

    fn ok_handler() -> SubscriptionHandler {
        Box::new(|_| Box::pin(async { Ok(()) }))
    }

    /// `unwrap_err()` 要求 Ok 类型是 `Debug`（`AckableMessage` / `Subscription` 不是），这里只要错误。
    fn expect_err<T, E: std::fmt::Debug>(result: std::result::Result<T, E>) -> E {
        match result {
            Ok(_) => panic!("期望报错，结果却是 Ok"),
            Err(error) => error,
        }
    }

    #[tokio::test]
    async fn test_peek_returns_copies_without_consuming_anything() -> Result<()> {
        let provider = MemoryProvider::new();
        let address: Url = "mem:rust-bus/peek-basic".parse()?;

        post_body(&provider, &address, "peek-1", json!({ "n": 1 })).await?;
        post_body(&provider, &address, "peek-2", json!({ "n": 2 })).await?;

        let snapshot = provider.peek(address.clone(), None).await?;
        assert_eq!(
            snapshot.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["peek-1", "peek-2"]
        );

        let status = provider.status(address.clone()).await?;
        assert_eq!(status.unread_count, Some(2)); // 不出队
        assert_eq!(status.in_flight_count, Some(0)); // 也不建 in-flight

        // 反复快照稳定；改副本改不到队列里的对象（Rust 的 `Clone` 即深拷贝）。
        let mut copy = snapshot[0].clone();
        copy.body = json!({ "tampered": true });
        assert_eq!(provider.peek(address.clone(), None).await?[0].body, json!({ "n": 1 }));

        // 消息仍能被 FIFO 取走。
        assert_eq!(
            provider.fetch(address.clone(), FetchOptions::default()).await?.unwrap().message.id,
            "peek-1"
        );
        assert_eq!(
            provider.fetch(address.clone(), FetchOptions::default()).await?.unwrap().message.id,
            "peek-2"
        );
        assert!(provider.fetch(address.clone(), FetchOptions::default()).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn test_peek_limit_covers_the_head_only() -> Result<()> {
        let provider = MemoryProvider::new();
        let address: Url = "mem:rust-bus/peek-limit".parse()?;
        for n in 1..=3 {
            post_body(&provider, &address, &format!("limit-{}", n), json!({ "n": n })).await?;
        }

        assert!(provider.peek(address.clone(), Some(0)).await?.is_empty());
        assert_eq!(
            provider.peek(address.clone(), Some(2)).await?
                .iter().map(|m| m.body["n"].as_i64().unwrap()).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(provider.peek(address.clone(), Some(usize::MAX)).await?.len(), 3);
        assert_eq!(provider.peek(address.clone(), None).await?.len(), 3);
        assert_eq!(provider.status(address.clone()).await?.unread_count, Some(3)); // limit 不消耗消息
        Ok(())
    }

    #[tokio::test]
    async fn test_peek_covers_only_waiting_messages() -> Result<()> {
        let provider = MemoryProvider::new();
        let address: Url = "mem:rust-bus/peek-vs-inflight".parse()?;
        post_body(&provider, &address, "inflight-1", json!({ "n": 1 })).await?;
        post_body(&provider, &address, "inflight-2", json!({ "n": 2 })).await?;

        let options = FetchOptions { manual_ack: true, ack_timeout: None, ..Default::default() };
        let inflight = provider.fetch(address.clone(), options).await?.unwrap();
        assert_eq!(inflight.message.body, json!({ "n": 1 }));

        // 不会把另一个消费者手上的消息也列出来。
        assert_eq!(
            provider.peek(address.clone(), None).await?
                .iter().map(|m| m.body["n"].as_i64().unwrap()).collect::<Vec<_>>(),
            vec![2]
        );

        inflight.nack(true).await?;
        assert_eq!(provider.peek(address.clone(), None).await?.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn test_peek_and_status_do_not_refresh_last_activity_time() -> Result<()> {
        // 诊断页轮询不会把空闲地址刷成“刚刚有活动”。
        let provider = MemoryProvider::new();
        let address: Url = "mem:rust-bus/peek-activity".parse()?;
        post_body(&provider, &address, "activity-1", json!({ "n": 1 })).await?;

        let before = provider.status(address.clone()).await?;
        assert!(before.last_activity_time.is_some());

        provider.peek(address.clone(), None).await?;
        provider.peek(address.clone(), Some(1)).await?;
        let after = provider.status(address.clone()).await?;

        assert_eq!(after.last_activity_time, before.last_activity_time);
        Ok(())
    }

    #[tokio::test]
    async fn test_an_address_that_was_never_touched_is_unknown() -> Result<()> {
        let provider = MemoryProvider::new();
        let status = provider.status("mem:rust-bus/never-touched".parse()?).await?;

        assert_eq!(status.state, "unknown"); // 不是“online + 0”
        assert_eq!(status.exists, Some(false));
        assert_eq!(status.unread_count, Some(0));
        assert_eq!(status.in_flight_count, Some(0));
        assert_eq!(status.last_activity_time, None);
        assert_eq!(status.extra["subscriber_count"], json!(0));
        assert!(status.extra["message"]
            .as_str()
            .unwrap()
            .contains("No queue, subscription or activity is known"));

        // `last_activity_time` 必须是 `null` 而不是「字段消失」：否则 JSON 一轮之后诊断页就
        // 拿不到这个概念了。
        let wire: HashMap<String, Value> = serde_json::from_str(&serde_json::to_string(&status)?)?;
        assert!(wire.contains_key("last_activity_time"));
        assert!(wire["last_activity_time"].is_null());
        assert_eq!(wire["exists"], json!(false));
        assert_eq!(wire["state"], json!("unknown"));
        Ok(())
    }

    #[tokio::test]
    async fn test_drained_but_real_is_still_online() -> Result<()> {
        // “存在但空闲”与“不存在”是两回事。
        let provider = MemoryProvider::new();
        let address: Url = "mem:rust-bus/drained".parse()?;
        post_body(&provider, &address, "drained-1", json!({ "n": 1 })).await?;
        provider.fetch(address.clone(), FetchOptions::default()).await?;

        let status = provider.status(address).await?;
        assert_eq!(status.exists, Some(true));
        assert_eq!(status.state, "online");
        assert_eq!(status.unread_count, Some(0)); // 空闲
        assert!(status.last_activity_time.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn test_an_address_with_only_a_subscriber_exists_too() -> Result<()> {
        let provider = MemoryProvider::new();
        let address: Url = "mem:rust-bus/subscribed-only".parse()?;
        let _sub = provider.subscribe(address.clone(), ok_handler()).await?;

        let status = provider.status(address).await?;
        assert_eq!(status.exists, Some(true));
        assert_eq!(status.state, "online");
        assert_eq!(status.extra["subscriber_count"], json!(1));
        assert_eq!(status.unread_count, Some(0));
        Ok(())
    }

    #[tokio::test]
    async fn test_in_flight_messages_keep_an_address_existing() -> Result<()> {
        let provider = MemoryProvider::new();
        let address: Url = "mem:rust-bus/inflight-exists".parse()?;
        post_body(&provider, &address, "inflight-exists-1", json!({ "n": 1 })).await?;
        provider
            .fetch(address.clone(), FetchOptions { manual_ack: true, ..Default::default() })
            .await?;

        let status = provider.status(address).await?;
        assert_eq!(status.exists, Some(true));
        assert_eq!(status.unread_count, Some(0));
        assert_eq!(status.in_flight_count, Some(1));
        Ok(())
    }

    #[tokio::test]
    async fn test_query_and_hash_addresses_are_rejected_at_every_entry() -> Result<()> {
        // 绕过 `Mailbox` 直接用 provider 也拦得住：canonicalization 自己就是防线。
        let provider = MemoryProvider::new();
        let with_query: Url = "mem:rust-bus/reject?thread=1".parse()?;
        let with_hash: Url = "mem:rust-bus/reject#frag".parse()?;

        let error = expect_err(provider
            .send(MailMessage {
                id: "direct-1".to_string(),
                from: "mem:rust-bus/sender".parse()?,
                to: with_query.clone(),
                body: json!({}),
                headers: HashMap::new(),
                meta: HashMap::new(),
            })
            .await);
        assert_eq!(error.code(), Some("MBX_INVALID_ADDRESS"));
        assert!(error.to_string().contains("would be dropped by address canonicalization"));

        assert_eq!(
            expect_err(provider.subscribe(with_query.clone(), ok_handler()).await).code(),
            Some("MBX_INVALID_ADDRESS")
        );
        assert_eq!(
            expect_err(provider.fetch(with_hash.clone(), FetchOptions::default()).await).code(),
            Some("MBX_INVALID_ADDRESS")
        );
        assert_eq!(
            expect_err(provider.status(with_query.clone()).await).code(),
            Some("MBX_INVALID_ADDRESS")
        );
        // peek 不是绕过地址校验的后门。
        assert_eq!(
            expect_err(provider.peek(with_query.clone(), None).await).code(),
            Some("MBX_INVALID_ADDRESS")
        );

        // 旧行为：不带 query 的 status 也会看到这封信（两条业务流共用一条队列）。
        let status = provider.status("mem:rust-bus/reject".parse()?).await?;
        assert_eq!(status.exists, Some(false));
        assert_eq!(status.unread_count, Some(0));
        assert_eq!(status.extra["subscriber_count"], json!(0));
        Ok(())
    }

    #[tokio::test]
    async fn test_logical_partitions_live_in_the_path() -> Result<()> {
        let provider = MemoryProvider::new();
        let thread_one: Url = "mem:rust-bus/partitions/inbox/thread-1".parse()?;
        let thread_two: Url = "mem:rust-bus/partitions/inbox/thread-2".parse()?;

        post_body(&provider, &thread_one, "part-1", json!({ "thread": 1 })).await?;
        post_body(&provider, &thread_two, "part-2", json!({ "thread": 2 })).await?;

        assert_eq!(provider.status(thread_one.clone()).await?.unread_count, Some(1));
        assert_eq!(provider.status(thread_two.clone()).await?.unread_count, Some(1));

        // 取一个分区不会动到另一个分区。
        let fetched = provider.fetch(thread_one.clone(), FetchOptions::default()).await?.unwrap();
        assert_eq!(fetched.message.body, json!({ "thread": 1 }));
        assert_eq!(provider.status(thread_one).await?.unread_count, Some(0));
        assert_eq!(provider.status(thread_two.clone()).await?.unread_count, Some(1));
        assert_eq!(
            provider.fetch(thread_two, FetchOptions::default()).await?.unwrap().message.body,
            json!({ "thread": 2 })
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_opaque_and_hierarchical_spellings_are_the_same_address() -> Result<()> {
        let provider = MemoryProvider::new();
        post_body(&provider, &"mem:rust-bus/eq-spelling/inbox".parse()?, "eq-1", json!({ "n": 1 })).await?;

        let hierarchical: Url = "mem://rust-bus/eq-spelling/inbox".parse()?;
        assert_eq!(provider.status(hierarchical.clone()).await?.unread_count, Some(1));
        assert_eq!(
            provider.fetch(hierarchical, FetchOptions::default()).await?.unwrap().message.body,
            json!({ "n": 1 })
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_peek_in_fetch_is_rejected_not_silently_consumed() -> Result<()> {
        let provider = MemoryProvider::new();
        let address: Url = "mem:rust-bus/peek-conflict".parse()?;
        post_body(&provider, &address, "conflict-1", json!({ "n": 1 })).await?;

        let error = expect_err(
            provider
                .fetch(address.clone(), FetchOptions { peek: true, manual_ack: true, ..Default::default() })
                .await,
        );
        assert_eq!(error.code(), Some("MBX_INVALID_FETCH_OPTIONS"));
        assert!(error.to_string().contains("contradictory"));

        let status = provider.status(address).await?;
        assert_eq!(status.unread_count, Some(1)); // 报错没消耗消息
        assert_eq!(status.in_flight_count, Some(0)); // 也没偷偷建 in-flight
        Ok(())
    }

    #[tokio::test]
    async fn test_provider_without_peek_reports_unsupported_and_keeps_the_message() -> Result<()> {
        // 不支持只读快照的传输型 provider：必须报错，不能退化成取件。
        let provider = RawBroadcastProvider::new();
        let address: Url = "raw:rust-bus/inbox".parse()?;
        provider
            .send(MailMessage {
                id: "raw-1".to_string(),
                from: "raw:rust-bus/sender".parse()?,
                to: address.clone(),
                body: json!({ "n": 1 }),
                headers: HashMap::new(),
                meta: HashMap::new(),
            })
            .await?;

        let error = expect_err(provider.peek(address.clone(), None).await);
        assert_eq!(error.code(), Some("MBX_UNSUPPORTED_CAPABILITY"));
        match error {
            MailboxError::UnsupportedCapability { protocol, capability } => {
                assert_eq!(protocol, "raw");
                assert!(capability.contains("read-only peek"), "{}", capability);
            }
            other => panic!("期望 UnsupportedCapability，得到 {:?}", other),
        }
        assert_eq!(provider.queued(&address), 1); // 关键：没有“以为在只读、实际被取走”

        // 枚举能力同理（`Mailbox::snapshot` 会把它归入 unsupported_providers）。
        assert_eq!(
            expect_err(provider.list_addresses().await).code(),
            Some("MBX_UNSUPPORTED_CAPABILITY")
        );
        // 直接用 `fetch(peek: true)` 也不会被静默降级。
        assert_eq!(
            expect_err(provider.fetch(address.clone(), FetchOptions { peek: true, ..Default::default() }).await).code(),
            Some("MBX_INVALID_FETCH_OPTIONS")
        );
        assert_eq!(provider.queued(&address), 1);
        Ok(())
    }

    #[test]
    fn test_bus_level_knowledge_and_topic_enumeration() {
        // 总线层的同一套语义（对应 Python 的 `MemoryEventBus.knows` / `list_topics`）。
        let subscribed = "mem://rust-bus/list-a";
        let queued = "mem://rust-bus/list-b";
        let never = "mem://rust-bus/list-never";

        {
            let bus = BUS.read().unwrap();
            assert!(!knows(&bus, never));
        }

        publish(
            queued,
            MailMessage {
                id: "list-b-1".to_string(),
                from: "mem:rust-bus/sender".parse().unwrap(),
                to: queued.parse().unwrap(),
                body: json!({}),
                headers: HashMap::new(),
                meta: HashMap::new(),
            },
        );
        BUS.write().unwrap().topics.entry(subscribed.to_string()).or_default();

        let topics = list_topics();
        assert!(topics.contains(&subscribed.to_string()), "{:?}", topics);
        assert!(topics.contains(&queued.to_string()), "{:?}", topics);
        assert!(!topics.contains(&never.to_string()));
        let mut sorted = topics.clone();
        sorted.sort();
        assert_eq!(topics, sorted); // 快照与测试断言需要确定性

        {
            let bus = BUS.read().unwrap();
            assert!(knows(&bus, subscribed)); // 只订阅也算“这个地址有人用”
            assert!(knows(&bus, queued));
            assert!(!knows(&bus, never));
        }
    }

    /// 第三方广播 provider 的常见写法：`_send` 里把消息派发出去，但**丢弃**了每次投递的结算
    /// future（没接 `track_broadcast_settlement`）。投递级记账把回队判定搬到了 barrier 上，
    /// 于是这类 provider 的可重试失败会静默消失 —— core 必须兜住。
    struct RawBroadcastProvider {
        core: Arc<ProviderCore>,
        topics: Arc<RwLock<HashMap<String, Vec<Arc<Listener>>>>>,
        queue: Arc<Mutex<MailMessageQueue<MailMessage>>>,
    }

    impl RawBroadcastProvider {
        fn new() -> Self {
            let topics = Arc::new(RwLock::new(HashMap::new()));
            let queue = Arc::new(Mutex::new(MailMessageQueue::new()));
            let queue_for_nack = queue.clone();
            let core = ProviderCore::new(
                "raw",
                ProviderCoreOptions {
                    broadcast: true,
                    nack: Some(Arc::new(move |message: MailMessage, requeue: bool| {
                        let queue = queue_for_nack.clone();
                        Box::pin(async move {
                            if requeue {
                                let topic = get_canonical_mailbox_address_identifier(&message.to)?;
                                queue.lock().unwrap().requeue(topic, message);
                            }
                            Ok(())
                        })
                    })),
                    ..ProviderCoreOptions::default()
                },
            );
            Self { core, topics, queue }
        }

        fn queued(&self, address: &Url) -> usize {
            self.queue
                .lock()
                .unwrap()
                .get_status(&get_canonical_mailbox_address_identifier(address).unwrap())
        }
    }

    #[async_trait]
    impl MailboxProvider for RawBroadcastProvider {
        fn protocol(&self) -> &str {
            self.core.protocol()
        }

        async fn _send(&self, message: MailMessage) -> Result<()> {
            let topic = get_canonical_mailbox_address_identifier(&message.to)?;
            let listeners = self.topics.read().unwrap().get(&topic).cloned().unwrap_or_default();
            if listeners.is_empty() {
                self.queue.lock().unwrap().enqueue(topic, message);
                return Ok(());
            }
            // 故意不调用 `track_broadcast_settlement`：模拟漏接 barrier 的 provider。
            for listener in listeners {
                // 投递照旧发生，只是结算 future 被丢掉了
                let _settlement = listener(message.clone());
            }
            Ok(())
        }

        async fn subscribe(
            &self,
            address: Url,
            callback: SubscriptionHandler,
        ) -> Result<Box<dyn Subscription>> {
            let topic = get_canonical_mailbox_address_identifier(&address)?;
            let (sub_id, listener) = self.core.register_subscription(address, callback);
            self.topics
                .write()
                .unwrap()
                .entry(topic.clone())
                .or_default()
                .push(listener.clone());

            let topics = self.topics.clone();
            self.core.attach_unsubscribe(&sub_id, Box::new(move || {
                let topics = topics.clone();
                let topic = topic.clone();
                let listener = listener.clone();
                Box::pin(async move {
                    if let Some(listeners) = topics.write().unwrap().get_mut(&topic) {
                        listeners.retain(|l| !Arc::ptr_eq(l, &listener));
                    }
                    Ok(())
                })
            }));
            Ok(self.core.subscription_object(sub_id))
        }

        async fn fetch(&self, address: Url, options: FetchOptions) -> Result<Option<AckableMessage>> {
            // 这个第三方写法示范：入口必须挡住 `peek`，否则只读快照会被静默当成取件。
            reject_peek_in_fetch(&options)?;

            let topic = get_canonical_mailbox_address_identifier(&address)?;
            let message = self.queue.lock().unwrap().dequeue(&topic);
            Ok(message.map(|message| AckableMessage {
                message,
                ack: Box::new(|| Box::pin(async { Ok(()) })),
                nack: Box::new(|_| Box::pin(async { Ok(()) })),
            }))
        }

        // 没有 `peek` / `list_addresses`：保留 trait 默认实现，用来验证
        // 「不支持只读要报错」与「枚举不了要进 unsupported_providers」两条护栏。
        async fn status(&self, address: Url) -> Result<MailboxStatus> {
            Ok(MailboxStatus {
                state: "online".to_string(),
                exists: Some(true),
                unread_count: Some(self.queued(&address)),
                in_flight_count: None,
                last_activity_time: None,
                extra: HashMap::new(),
            })
        }

        fn generate_id(&self) -> String {
            Uuid::new_v4().to_string()
        }
    }
}
