use std::collections::HashMap;
use std::sync::Arc;
use url::Url;
use crate::error::{MailboxError, Result};
use crate::message::{
    FetchOptions, MailMessage, MailboxAddressSnapshot, MailboxSnapshot, MailboxStatus, OutgoingMail,
};
use crate::provider::{reject_peek_in_fetch, AckableMessage, MailboxProvider, Subscription, SubscriptionHandler};
use crate::utils::{assert_lossless_mailbox_address, normalize_peek_limit};

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
     *
     * 默认只拆订阅、不动积压消息：`stop()` 后同一个 provider（或共享同一份状态的另一个
     * `Mailbox`）重新 `start()`，队列里的残留消息照旧可见 —— 这就是 issue#3 里「测试隔离失效」
     * 的根源。确实要销毁状态用 [`Mailbox::stop_and_purge`]（= TS `stop({ purge: true })` /
     * Python `stop(purge=True)`）。
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

    /**
     * 先拆订阅（`close`），再丢弃各 provider 的积压消息（含 in-flight）。
     * 等价于 TS 的 `mailbox.stop({ purge: true })` / Python 的 `stop(purge=True)`；Rust 没有
     * 可选参数，所以拆成两个方法。没实现 `clear()` 的 provider（传输型）在这一步是 no-op。
     */
    pub async fn stop_and_purge(&self) -> Result<()> {
        self.stop().await?;
        self.clear().await
    }

    /**
     * 丢弃各 provider 持有的积压消息（含 in-flight），**不**拆订阅。
     *
     * 测试隔离、用例之间重置状态用它；没实现 [`MailboxProvider::clear`] 的 provider 会被跳过
     * （清 broker 属于破坏性运维，库不代劳）。拆订阅是 `stop()` 的事；反过来，`clear()` 不拆订阅：
     * 那会让旧 `Subscription` 变成「报 active 但再也收不到消息」的僵尸。
     *
     * 注意默认构造的 `MemoryProvider` 共用进程级单例总线 —— `clear()` 会清掉同一份总线上其它
     * `Mailbox` 的队列。只清自己那份，请用 `MemoryProvider::create_isolated()` 或
     * `MemoryProvider::for_scope(scope, ..)`（issue#3）。
     */
    pub async fn clear(&self) -> Result<()> {
        let mut futures = Vec::new();
        for provider in self.providers.values() {
            futures.push(provider.clear());
        }

        // 并行清理
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
        // 地址在每个入口的规则必须一致：带着 query / hash 的地址不能只在这里合法。
        assert_lossless_mailbox_address(&mail.to, "post()")?;
        assert_lossless_mailbox_address(&mail.from, "post() (from)")?;

        let provider = self.get_provider_or_fail(mail.to.scheme())?;

        let mut message: MailMessage = mail.clone().into();
        if message.id.is_empty() {
             message.id = provider.generate_id();
        }

        provider.send(message).await
    }

    /**
     * 订阅。回调返回 `Err` 时由 provider 按可重试判定回队或丢弃。
     *
     * 地址规则与 `post` / `fetch` / `status` 相同：带 query / hash 的地址直接报错
     * （订阅 `?thread=1` 实际订阅的是裸地址的队列，曾经隐蔽程度最高的一半）。
     */
    pub async fn subscribe(
        &self,
        address: Url,
        callback: SubscriptionHandler,
    ) -> Result<Box<dyn Subscription>> {
        assert_lossless_mailbox_address(&address, "subscribe()")?;
        let provider = self.get_provider_or_fail(address.scheme())?;
        provider.subscribe(address, callback).await
    }

    /**
     * 取信（消费型）。
     *
     * - 默认（`FetchOptions::default()`）：取一封，隐式 ACK（出队即完成）。
     * - `manual_ack: true`：取一封，进 in-flight，直到 ack / nack 或超时重投。
     *
     * 只读快照不在这里：Rust 里两种取法的返回类型不同，所以下面是单独的 [`Mailbox::peek`]
     * （对应 TS / Python 的 `fetch(addr, { peek: true })`）。`options.peek == true` 会在入口报错，
     * 不会被静默降级成普通取信。
     */
    pub async fn fetch(&self, address: Url, options: FetchOptions) -> Result<Option<AckableMessage>> {
        assert_lossless_mailbox_address(&address, "fetch()")?;
        reject_peek_in_fetch(&options)?;

        let provider = self.get_provider_or_fail(address.scheme())?;
        provider.fetch(address, options).await
    }

    /**
     * 【只读快照】看一眼队列里等待的消息：返回**副本**数组（空队列 / 未知地址是空数组），
     * 不出队、不建 in-flight、不刷活动时间。
     *
     * 诊断与测试断言用这个，不要用「`manual_ack: true` 取一封不 ack」凑 —— 那会把消息弄进
     * in-flight（默认 30s 后重投），既不是只读，也会干扰真正消费者的 `unread_count` /
     * `in_flight_count`（issue#4）。
     *
     * provider 不具备只读能力时报 [`MailboxError::UnsupportedCapability`]，不会退化成取信。
     *
     * @param limit 最多看几条；`None` = 整个队列。
     */
    pub async fn peek(&self, address: Url, limit: Option<usize>) -> Result<Vec<MailMessage>> {
        assert_lossless_mailbox_address(&address, "peek()")?;
        let provider = self.get_provider_or_fail(address.scheme())?;
        provider.peek(address, normalize_peek_limit(limit)).await
    }

    /**
     * 查询指定地址的状态信息。
     *
     * 地址在 provider 里没有任何记录（没投过、没订阅过、没有活动）时，返回
     * `state: "unknown"` + `exists: Some(false)`，而不是 `state: "online"` + 0 —— 诊断页要能
     * 区分“这个地址不存在”和“这个地址空闲”（issue#4）。provider 答不了状态时 `exists` 不填
     * （问不到 ≠ 不存在）。
     *
     * 本方法只读：不会刷新 `last_activity_time`。全量快照请用 [`Mailbox::snapshot`]
     * （即 TS / Python 里不传地址的 `status()`）。
     *
     * @param address 要查询的地址。
     * @returns 该地址的邮箱状态。
     */
    pub async fn status(&self, address: Url) -> Result<MailboxStatus> {
        assert_lossless_mailbox_address(&address, "status()")?;
        let provider = self.get_provider_or_fail(address.scheme())?;
        provider.status(address).await
    }

    /**
     * 全量快照：把所有能枚举自身地址的 provider 的 topic 状态汇成一个可断言的结构。
     *
     * 等价于 TS / Python 里不传地址的 `status()`（Rust 没有可选参数，所以名字直接到位）。
     * 不能枚举的 provider（消息不在本地，如 AMQP / SQS）会列在 `unsupported_providers` 里，
     * 不会被误报成“队列为空”。
     */
    pub async fn snapshot(&self) -> Result<MailboxSnapshot> {
        let mut protocols: Vec<String> = self.providers.keys().cloned().collect();
        protocols.sort();

        let mut addresses: Vec<MailboxAddressSnapshot> = Vec::new();
        let mut unsupported_providers: Vec<String> = Vec::new();

        for (protocol, provider) in &self.providers {
            let canonical_ids = match provider.list_addresses().await {
                Ok(ids) => ids,
                // 枚举不了 ≠ 队列为空：把这份 provider 说出来，而不是当它不存在。
                Err(MailboxError::UnsupportedCapability { .. }) => {
                    unsupported_providers.push(protocol.clone());
                    continue;
                }
                Err(error) => return Err(error),
            };

            for canonical_id in canonical_ids {
                let status = status_of_canonical_address(provider, protocol, &canonical_id).await;
                addresses.push(MailboxAddressSnapshot {
                    canonical_id,
                    protocol: protocol.clone(),
                    status,
                });
            }
        }

        addresses.sort_by(|a, b| a.canonical_id.cmp(&b.canonical_id));
        unsupported_providers.sort(); // provider 表是 HashMap：不排序同一份快照每次都不一样。

        let unread_count = addresses
            .iter()
            .map(|entry| entry.status.unread_count.unwrap_or(0))
            .sum();
        let in_flight_count = addresses
            .iter()
            .map(|entry| entry.status.in_flight_count.unwrap_or(0))
            .sum();
        let enumerated = protocols.len() - unsupported_providers.len();

        let state = if enumerated == 0 {
            "unknown".to_string() // 没有任何 provider 能枚举：这份快照什么都没说
        } else if !unsupported_providers.is_empty()
            || addresses.iter().any(|entry| entry.status.state != "online")
        {
            "degraded".to_string() // 只看到一部分，或有地址本身就不在线
        } else {
            "online".to_string()
        };

        Ok(MailboxSnapshot {
            state,
            protocols,
            address_count: addresses.len(),
            unread_count,
            in_flight_count,
            addresses,
            unsupported_providers,
            taken_at: chrono::Utc::now().to_rfc3339(),
        })
    }
}

/// 快照里对单个地址的查询：canonical id 本身可以重新解析成地址（它已经是
/// `protocol://user@host/path` 形式）。解析不了是 provider 的 bug，宁可报 unknown
/// 也不能让整个快照崩掉。
async fn status_of_canonical_address(
    provider: &Arc<dyn MailboxProvider>,
    protocol: &str,
    canonical_id: &str,
) -> MailboxStatus {
    let fallback = |cause: String| MailboxStatus {
        state: "unknown".to_string(),
        // provider 自己列出了这个地址，所认得它是真的；只是查不回来。
        exists: Some(true),
        unread_count: None,
        in_flight_count: None,
        last_activity_time: None,
        extra: {
            let mut extra = HashMap::new();
            extra.insert(
                "message".to_string(),
                serde_json::json!(format!(
                    "The '{}' provider listed '{}', but it could not be queried back.",
                    protocol, canonical_id
                )),
            );
            extra.insert("cause".to_string(), serde_json::json!(cause));
            extra
        },
    };

    let address = match Url::parse(canonical_id) {
        Ok(address) => address,
        Err(error) => return fallback(error.to_string()),
    };

    match provider.status(address).await {
        Ok(status) => status,
        Err(error) => fallback(error.to_string()),
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
    use crate::provider::SubscriptionStatus;
    use crate::providers::memory::MemoryProvider;
    use crate::utils::get_canonical_mailbox_address_identifier;
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

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

    // --- issue#4：诚实的 status、全量快照、只读 peek、地址规则 ------------------------------
    //
    // 与 TS `src/mailbox.status-diagnostics.test.ts` + `src/mailbox.peek.test.ts` +
    // `src/mailbox.address-canonicalization.test.ts`、Python `tests/test_status_diagnostics.py`
    // 同一组不变量。
    // 注：默认构造的 `MemoryProvider` 仍共享进程级总线（`MemoryProvider::create_isolated()` 才是
    // 隔离的，见 issue#3），所以涉及 `MemoryProvider` 的快照断言只看本次创建的那几个地址，不用
    // 绝对计数；需要确定性的快照测试用下面的本地 provider。

    /// 消息不在本地：既答不了 status，也枚举不了自己的地址（AMQP / SQS 那一类 broker）。
    struct RemoteBrokerProvider {
        protocol: &'static str,
        sent: Arc<Mutex<Vec<MailMessage>>>,
    }

    fn remote(protocol: &'static str) -> RemoteBrokerProvider {
        RemoteBrokerProvider {
            protocol,
            sent: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[async_trait]
    impl MailboxProvider for RemoteBrokerProvider {
        fn protocol(&self) -> &str {
            self.protocol
        }

        async fn _send(&self, message: MailMessage) -> Result<()> {
            self.sent.lock().unwrap().push(message);
            Ok(())
        }

        async fn subscribe(
            &self,
            _address: Url,
            _callback: SubscriptionHandler,
        ) -> Result<Box<dyn Subscription>> {
            Err(MailboxError::unsupported_capability(self.protocol(), "subscriptions"))
        }

        async fn fetch(&self, _address: Url, options: FetchOptions) -> Result<Option<AckableMessage>> {
            crate::provider::reject_peek_in_fetch(&options)?;
            let message = {
                let mut sent = self.sent.lock().unwrap();
                if sent.is_empty() {
                    None
                } else {
                    Some(sent.remove(0))
                }
            };
            Ok(message.map(|message| AckableMessage {
                message,
                ack: Box::new(|| Box::pin(async { Ok(()) })),
                nack: Box::new(|_| Box::pin(async { Ok(()) })),
            }))
        }

        fn generate_id(&self) -> String {
            "nostatus-id".to_string()
        }
    }

    /// 消息在本地、能报状态也能枚举，但**没**实现 `peek` 的 provider：快照路径的确定性夹具
    /// （`mem:` 总线是进程级单例，全局共享，快照断言需要一个只属于本测试的 provider）。
    struct LocalQueueProvider {
        queued: Arc<Mutex<Vec<MailMessage>>>,
        in_flight: Arc<Mutex<Vec<(String, MailMessage)>>>,
    }

    impl LocalQueueProvider {
        fn new() -> Self {
            Self {
                queued: Arc::new(Mutex::new(Vec::new())),
                in_flight: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn topic_of(address: &Url) -> String {
            get_canonical_mailbox_address_identifier(address).expect("测试里的地址必须是合法的")
        }

        fn matching(&self, topic: &str) -> Vec<MailMessage> {
            self.queued
                .lock()
                .unwrap()
                .iter()
                .filter(|message| Self::topic_of(&message.to) == topic)
                .cloned()
                .collect()
        }
    }

    #[async_trait]
    impl MailboxProvider for LocalQueueProvider {
        fn protocol(&self) -> &str {
            "local"
        }

        async fn _send(&self, message: MailMessage) -> Result<()> {
            self.queued.lock().unwrap().push(message);
            Ok(())
        }

        async fn subscribe(
            &self,
            _address: Url,
            _callback: SubscriptionHandler,
        ) -> Result<Box<dyn Subscription>> {
            Err(MailboxError::unsupported_capability(self.protocol(), "subscriptions"))
        }

        async fn fetch(&self, address: Url, options: FetchOptions) -> Result<Option<AckableMessage>> {
            crate::provider::reject_peek_in_fetch(&options)?;
            let topic = Self::topic_of(&address);
            let message = {
                let mut queued = self.queued.lock().unwrap();
                let index = queued
                    .iter()
                    .position(|message| Self::topic_of(&message.to) == topic);
                index.map(|index| queued.remove(index))
            };
            let message = match message {
                Some(message) => message,
                None => return Ok(None),
            };

            if !options.manual_ack {
                return Ok(Some(AckableMessage {
                    message,
                    ack: Box::new(|| Box::pin(async { Ok(()) })),
                    nack: Box::new(|_| Box::pin(async { Ok(()) })),
                }));
            }

            // manual_ack：离开队列但没结算完，`status` 里必须看得见。
            let id = message.id.clone();
            self.in_flight.lock().unwrap().push((topic, message.clone()));
            let in_flight_for_ack = self.in_flight.clone();
            let in_flight_for_nack = self.in_flight.clone();
            let queued_for_nack = self.queued.clone();
            let id_for_ack = id.clone();
            Ok(Some(AckableMessage {
                message,
                ack: Box::new(move || {
                    Box::pin(async move {
                        in_flight_for_ack
                            .lock()
                            .unwrap()
                            .retain(|(flight_id, _)| *flight_id != id_for_ack);
                        Ok(())
                    })
                }),
                nack: Box::new(move |requeue| {
                    Box::pin(async move {
                        let taken = {
                            let mut flights = in_flight_for_nack.lock().unwrap();
                            let index = flights.iter().position(|(flight_id, _)| *flight_id == id);
                            index.map(|index| flights.remove(index))
                        };
                        if requeue {
                            if let Some((_, message)) = taken {
                                queued_for_nack.lock().unwrap().push(message);
                            }
                        }
                        Ok(())
                    })
                }),
            }))
        }

        async fn status(&self, address: Url) -> Result<MailboxStatus> {
            let topic = Self::topic_of(&address);
            let unread_count = self.matching(&topic).len();
            let in_flight_count = self
                .in_flight
                .lock()
                .unwrap()
                .iter()
                .filter(|(flight_topic, _)| *flight_topic == topic)
                .count();
            Ok(MailboxStatus {
                state: "online".to_string(),
                exists: Some(unread_count + in_flight_count > 0),
                unread_count: Some(unread_count),
                in_flight_count: Some(in_flight_count),
                last_activity_time: None,
                extra: HashMap::new(),
            })
        }

        async fn list_addresses(&self) -> Result<Vec<String>> {
            let mut topics: Vec<String> = self
                .queued
                .lock()
                .unwrap()
                .iter()
                .map(|message| Self::topic_of(&message.to))
                .collect();
            topics.extend(
                self.in_flight
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|(topic, _)| topic.clone()),
            );
            topics.sort();
            topics.dedup();
            Ok(topics)
        }

        fn generate_id(&self) -> String {
            uuid::Uuid::new_v4().to_string()
        }
    }

    fn mail(from: &str, to: &str, body: serde_json::Value) -> OutgoingMail {
        OutgoingMail {
            id: None,
            from: from.parse().unwrap(),
            to: to.parse().unwrap(),
            body,
            headers: HashMap::new(),
            meta: HashMap::new(),
        }
    }

    fn expect_err<T>(result: Result<T>) -> MailboxError {
        match result {
            Ok(_) => panic!("期望报错，结果却是 Ok"),
            Err(error) => error,
        }
    }

    fn mailbox_with(provider: Box<dyn MailboxProvider>) -> Mailbox {
        let mut mailbox = Mailbox::new();
        mailbox.register_provider(provider);
        mailbox
    }

    #[tokio::test]
    async fn test_status_reports_unknown_for_an_address_that_was_never_touched() -> Result<()> {
        let mailbox = mailbox_with(Box::new(MemoryProvider::new()));

        let status = mailbox.status("mem:rust-mailbox/never-touched".parse()?).await?;
        assert_eq!(status.state, "unknown");
        assert_eq!(status.exists, Some(false));
        assert_eq!(status.unread_count, Some(0));

        // 序列化后 `last_activity_time` 仍是 `null`，不是整个字段消失。
        let wire = serde_json::to_value(&status)?;
        assert!(wire.get("last_activity_time").is_some());
        assert!(wire["last_activity_time"].is_null());
        assert_eq!(wire["exists"], json!(false));
        Ok(())
    }

    #[tokio::test]
    async fn test_a_provider_that_cannot_answer_status_does_not_claim_absence() -> Result<()> {
        // 问不到 ≠ 不存在：`exists` 必须留空，不是 `Some(false)`。
        let mailbox = mailbox_with(Box::new(remote("nostatus")));

        let status = mailbox.status("nostatus:x@example.com/inbox".parse()?).await?;
        assert_eq!(status.state, "unknown");
        assert_eq!(status.exists, None);
        assert_eq!(
            status.extra["message"],
            json!("The 'nostatus' provider does not support status queries.")
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_peek_through_the_mailbox_does_not_consume_anything() -> Result<()> {
        let mailbox = mailbox_with(Box::new(MemoryProvider::new()));
        let address: Url = "mem:rust-mailbox/peek-basic".parse()?;

        mailbox.post(mail("mem:rust-mailbox/sender", address.as_str(), json!({ "n": 1 }))).await?;
        mailbox.post(mail("mem:rust-mailbox/sender", address.as_str(), json!({ "n": 2 }))).await?;

        let snapshot = mailbox.peek(address.clone(), None).await?;
        assert_eq!(
            snapshot.iter().map(|m| m.body["n"].as_i64().unwrap()).collect::<Vec<_>>(),
            vec![1, 2]
        );

        let status = mailbox.status(address.clone()).await?;
        assert_eq!(status.unread_count, Some(2));
        assert_eq!(status.in_flight_count, Some(0));

        // limit 只取头部；取走还是走 fetch。
        assert_eq!(mailbox.peek(address.clone(), Some(1)).await?.len(), 1);
        assert_eq!(
            mailbox.fetch(address.clone(), FetchOptions::default()).await?.unwrap().message.body,
            json!({ "n": 1 })
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_unknown_address_peek_is_an_empty_vec() -> Result<()> {
        let mailbox = mailbox_with(Box::new(MemoryProvider::new()));
        assert!(mailbox.peek("mem:rust-mailbox/peek-empty".parse()?, None).await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn test_peek_on_a_transport_without_the_capability_raises_and_keeps_the_message() -> Result<()> {
        let broker = remote("nostatus");
        let sent = broker.sent.clone();
        let mailbox = mailbox_with(Box::new(broker));
        let address: Url = "nostatus:b@example.com/inbox".parse()?;

        mailbox.post(mail("nostatus:a@example.com/u", address.as_str(), json!({ "n": 1 }))).await?;

        let error = expect_err(mailbox.peek(address.clone(), None).await);
        assert_eq!(error.code(), Some("MBX_UNSUPPORTED_CAPABILITY"));
        assert_eq!(sent.lock().unwrap().len(), 1); // 没有“以为在只读、实际被取走”
        Ok(())
    }

    #[tokio::test]
    async fn test_fetch_rejects_peek_options_without_consuming() -> Result<()> {
        // Rust 端取信与只读快照是两个方法（返回类型不同）；`fetch` 里给 `peek` 必须报错。
        let mailbox = mailbox_with(Box::new(MemoryProvider::new()));
        let address: Url = "mem:rust-mailbox/peek-in-fetch".parse()?;
        mailbox.post(mail("mem:rust-mailbox/sender", address.as_str(), json!({ "n": 1 }))).await?;

        let contradictory = expect_err(mailbox
            .fetch(
                address.clone(),
                FetchOptions { peek: true, manual_ack: true, ..Default::default() },
            )
            .await);
        assert_eq!(contradictory.code(), Some("MBX_INVALID_FETCH_OPTIONS"));
        assert!(contradictory.to_string().contains("contradictory"));

        let routed = expect_err(mailbox
            .fetch(address.clone(), FetchOptions { peek: true, ..Default::default() })
            .await);
        assert_eq!(routed.code(), Some("MBX_INVALID_FETCH_OPTIONS"));
        assert!(routed.to_string().contains("Mailbox::peek"));

        let status = mailbox.status(address).await?;
        assert_eq!(status.unread_count, Some(1));
        assert_eq!(status.in_flight_count, Some(0));
        Ok(())
    }

    #[tokio::test]
    async fn test_address_rules_apply_at_every_mailbox_entry() -> Result<()> {
        let mailbox = mailbox_with(Box::new(MemoryProvider::new()));
        let with_query: Url = "mem:rust-mailbox/entry-rules?thread=1".parse()?;
        let with_hash: Url = "mem:rust-mailbox/entry-rules#frag".parse()?;

        let error = expect_err(
            mailbox
                .post(mail("mem:rust-mailbox/u", with_query.as_str(), json!({ "n": 2 })))
                .await,
        );
        assert_eq!(error.code(), Some("MBX_INVALID_ADDRESS"));
        // 错误信息要能指出「会跟谁合并」。
        let message = error.to_string();
        assert!(message.contains("mem:rust-mailbox/entry-rules?thread=1"), "{}", message);
        assert!(message.contains("mem://rust-mailbox/entry-rules"), "{}", message);
        assert!(message.contains("in post()"), "{}", message);

        // from 端受同一套规则约束。
        assert!(mailbox
            .post(mail("mem:rust-mailbox/u?route=direct", "mem:rust-mailbox/entry-rules", json!({})))
            .await
            .is_err());

        assert_eq!(
            expect_err(
                mailbox
                    .subscribe(with_query.clone(), Box::new(|_| Box::pin(async { Ok(()) })))
                    .await,
            )
            .code(),
            Some("MBX_INVALID_ADDRESS")
        );
        assert_eq!(
            expect_err(mailbox.fetch(with_hash.clone(), FetchOptions::default()).await).code(),
            Some("MBX_INVALID_ADDRESS")
        );
        assert_eq!(
            expect_err(mailbox.status(with_query.clone()).await).code(),
            Some("MBX_INVALID_ADDRESS")
        );
        assert_eq!(
            expect_err(mailbox.peek(with_query.clone(), Some(3)).await).code(),
            Some("MBX_INVALID_ADDRESS")
        );

        // 旧行为：这些入口会把带 query / hash 的地址当成裸地址，痕迹全落在裸地址上。
        let bare = mailbox.status("mem:rust-mailbox/entry-rules".parse()?).await?;
        assert_eq!(bare.exists, Some(false));
        assert_eq!(bare.unread_count, Some(0));
        assert_eq!(bare.extra["subscriber_count"], json!(0));
        Ok(())
    }

    #[tokio::test]
    async fn test_snapshot_lists_every_topic_sorted_by_canonical_id_with_aggregates() -> Result<()> {
        let mailbox = mailbox_with(Box::new(LocalQueueProvider::new()));

        mailbox.post(mail("local:a@example.com/u", "local:b@example.com/queue-b", json!({ "n": 1 }))).await?;
        mailbox.post(mail("local:a@example.com/u", "local:b@example.com/queue-b", json!({ "n": 2 }))).await?;
        mailbox.post(mail("local:a@example.com/u", "local:a@example.com/queue-a", json!({ "n": 3 }))).await?;
        mailbox
            .fetch("local:b@example.com/queue-b".parse()?, FetchOptions { manual_ack: true, ..Default::default() })
            .await?;

        let snap = mailbox.snapshot().await?;
        assert_eq!(snap.state, "online");
        assert_eq!(snap.protocols, vec!["local".to_string()]);
        assert_eq!(snap.address_count, 2);
        assert_eq!(snap.unread_count, 2); // queue-a: 1 + queue-b: 2 - 1 (in-flight)
        assert_eq!(snap.in_flight_count, 1);
        // 按 canonical id 排序：快照可重复、断言可写死。
        assert_eq!(
            snap.addresses
                .iter()
                .map(|e| e.canonical_id.as_str())
                .collect::<Vec<_>>(),
            vec!["local://a@example.com/queue-a", "local://b@example.com/queue-b"]
        );
        assert_eq!(snap.addresses[0].protocol, "local");
        assert_eq!(snap.addresses[0].status.exists, Some(true));
        assert_eq!(snap.addresses[0].status.unread_count, Some(1));
        assert_eq!(snap.addresses[1].status.unread_count, Some(1));
        assert_eq!(snap.addresses[1].status.in_flight_count, Some(1));
        assert!(!snap.taken_at.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn test_an_empty_mailbox_still_produces_a_readable_snapshot() -> Result<()> {
        let mailbox = Mailbox::new();
        let snap = mailbox.snapshot().await?;

        assert_eq!(snap.state, "unknown"); // 没有任何 provider 能枚举：这份快照什么都没说
        assert!(snap.protocols.is_empty());
        assert_eq!(snap.address_count, 0);
        assert!(snap.addresses.is_empty());
        assert!(snap.unsupported_providers.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn test_providers_that_cannot_enumerate_degrade_the_snapshot() -> Result<()> {
        // 枚举不了的 provider 记在 `unsupported_providers`，而不是被当成“队列为空”。
        // 注册顺序与字典序刻意不同：provider 表是 HashMap，快照必须自己排序。
        let mut mailbox = Mailbox::new();
        mailbox.register_provider(Box::new(remote("zzz-remote")));
        mailbox.register_provider(Box::new(LocalQueueProvider::new()));
        mailbox.register_provider(Box::new(remote("aaa-remote")));

        mailbox.post(mail("local:a@example.com/u", "local:snap-visible@example.com/inbox", json!({}))).await?;
        mailbox.post(mail("aaa-remote:a@example.com/u", "aaa-remote:snap-hidden@example.com/inbox", json!({}))).await?;
        mailbox.post(mail("zzz-remote:a@example.com/u", "zzz-remote:snap-hidden@example.com/inbox", json!({}))).await?;

        let snap = mailbox.snapshot().await?;
        assert_eq!(
            snap.protocols,
            vec!["aaa-remote".to_string(), "local".to_string(), "zzz-remote".to_string()]
        );
        assert_eq!(
            snap.unsupported_providers,
            vec!["aaa-remote".to_string(), "zzz-remote".to_string()]
        );
        assert_eq!(snap.state, "degraded");
        assert_eq!(snap.address_count, 1); // 只有 local 那份能看见
        assert_eq!(snap.addresses[0].protocol, "local");
        assert_eq!(snap.addresses[0].status.state, "online");
        assert_eq!(snap.addresses[0].status.exists, Some(true));
        Ok(())
    }

    #[tokio::test]
    async fn test_every_snapshot_entry_can_be_queried_back_by_canonical_id() -> Result<()> {
        let mailbox = mailbox_with(Box::new(LocalQueueProvider::new()));
        let address = "local:roundtrip@example.com/inbox/thread-1";
        mailbox.post(mail("local:a@example.com/u", address, json!({}))).await?;

        let entry = &mailbox.snapshot().await?.addresses[0];
        assert_eq!(
            entry.canonical_id,
            get_canonical_mailbox_address_identifier(&address.parse()?)?
        );

        let direct = mailbox.status(entry.canonical_id.parse()?).await?;
        assert_eq!(direct.unread_count, entry.status.unread_count);
        assert_eq!(direct.state, entry.status.state);
        Ok(())
    }

    #[tokio::test]
    async fn test_snapshot_covers_the_memory_topics_this_mailbox_created() -> Result<()> {
        // `mem:` 总线在测试进程里是共享的，所以只看本次创建的地址（不用绝对计数）。
        let mailbox = mailbox_with(Box::new(MemoryProvider::new()));
        let kept = "mem:rust-mailbox/snapshot-mine/inbox";
        mailbox.post(mail("mem:rust-mailbox/sender", kept, json!({ "n": 1 }))).await?;

        let snap = mailbox.snapshot().await?;
        assert_eq!(snap.protocols, vec!["mem".to_string()]);
        assert!(snap.unsupported_providers.is_empty());
        assert_eq!(snap.state, "online");

        let entry = snap
            .addresses
            .iter()
            .find(|entry| entry.canonical_id == "mem://rust-mailbox/snapshot-mine/inbox")
            .expect("本次投递的 topic 必须在快照里");
        assert_eq!(entry.status.unread_count, Some(1));
        assert_eq!(entry.status.exists, Some(true));
        Ok(())
    }

    // --- issue#3：清理链路 `MailboxProvider::clear` → `Mailbox::clear` / `stop_and_purge` ------
    //
    // 与 TS `src/providers/memory.isolation.test.ts` 的清理组、Python `tests/test_memory_isolation.py`
    // 同一组不变量。provider 层的实例边界（私有 bus / scope）见 `providers/memory.rs` 的同名测试组。

    static SCOPE_SEQ: AtomicUsize = AtomicUsize::new(0);

    /// 隔离用的唯一 scope（不弄脏进程级共享总线，也不被其它用例弄脏）。
    fn unique_scope(label: &str) -> String {
        format!("rust-{}-{}", label, SCOPE_SEQ.fetch_add(1, Ordering::SeqCst))
    }

    /// 轮询等待条件成立（订阅回调在 spawn 的任务里跑）。
    async fn wait_until<Fut: std::future::Future<Output = bool>>(cond: impl Fn() -> Fut) -> bool {
        for _ in 0..100 {
            if cond().await {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        false
    }

    /// 数 `clear()` 被调了几次的夹具；消息存在本地，好断言「清没清」。
    struct ClearCounterProvider {
        cleared: Arc<AtomicUsize>,
        queued: Arc<Mutex<Vec<MailMessage>>>,
    }

    #[async_trait]
    impl MailboxProvider for ClearCounterProvider {
        fn protocol(&self) -> &str {
            "counter"
        }

        async fn _send(&self, message: MailMessage) -> Result<()> {
            self.queued.lock().unwrap().push(message);
            Ok(())
        }

        async fn subscribe(
            &self,
            _address: Url,
            _callback: SubscriptionHandler,
        ) -> Result<Box<dyn Subscription>> {
            Err(MailboxError::unsupported_capability(self.protocol(), "subscriptions"))
        }

        async fn fetch(&self, _address: Url, options: FetchOptions) -> Result<Option<AckableMessage>> {
            crate::provider::reject_peek_in_fetch(&options)?;
            let message = self.queued.lock().unwrap().remove(0);
            Ok(Some(AckableMessage {
                message,
                ack: Box::new(|| Box::pin(async { Ok(()) })),
                nack: Box::new(|_| Box::pin(async { Ok(()) })),
            }))
        }

        async fn clear(&self) -> Result<()> {
            self.cleared.fetch_add(1, Ordering::SeqCst);
            self.queued.lock().unwrap().clear();
            Ok(())
        }

        fn generate_id(&self) -> String {
            "counter-id".to_string()
        }
    }

    #[tokio::test]
    async fn test_clear_runs_on_every_provider_and_skips_the_no_op_ones() -> Result<()> {
        // 没覆写 `clear()` 的 provider（传输型）必须是 no-op：清 broker 属于破坏性运维，库不代劳。
        let cleared = Arc::new(AtomicUsize::new(0));
        let counter_queue: Arc<Mutex<Vec<MailMessage>>> = Arc::new(Mutex::new(Vec::new()));
        let broker = remote("nostatus");
        let broker_sent = broker.sent.clone();

        let mut mailbox = Mailbox::new();
        mailbox.register_provider(Box::new(ClearCounterProvider {
            cleared: cleared.clone(),
            queued: counter_queue.clone(),
        }));
        mailbox.register_provider(Box::new(broker));

        mailbox.post(mail("counter:a/u", "counter:box/inbox", json!({ "n": 1 }))).await?;
        mailbox.post(mail("nostatus:a/u", "nostatus:box/inbox", json!({ "n": 1 }))).await?;

        mailbox.clear().await?;

        assert_eq!(cleared.load(Ordering::SeqCst), 1); // 每个 provider 各调一次
        assert!(counter_queue.lock().unwrap().is_empty()); // 覆写了的：本地消息没了
        assert_eq!(broker_sent.lock().unwrap().len(), 1); // 没覆写的：什么都没被动
        Ok(())
    }

    #[tokio::test]
    async fn test_clear_is_idempotent_and_fine_without_any_provider() -> Result<()> {
        let mailbox = mailbox_with(Box::new(MemoryProvider::create_isolated(None)));
        mailbox.post(mail("mem:rust-clear/sender", "mem:rust-clear/idempotent", json!({}))).await?;

        mailbox.clear().await?;
        mailbox.clear().await?; // 重复调不报错
        assert_eq!(
            mailbox.status("mem:rust-clear/idempotent".parse()?).await?.unread_count,
            Some(0)
        );

        Mailbox::new().clear().await?; // 空 Mailbox
        Ok(())
    }

    #[tokio::test]
    async fn test_clear_drops_the_backlog_but_keeps_subscriptions() -> Result<()> {
        // `clear()` 不拆订阅：拆它是 `stop()` / `unsubscribe()` 的事。在 clear 里顺手拆订阅，
        // 旧 `Subscription` 会变成「报 active 但再也收不到消息」的僵尸。
        let mailbox = mailbox_with(Box::new(MemoryProvider::create_isolated(None)));
        let queued: Url = "mem:rust-clear/keep-sub-queue".parse()?;
        let live: Url = "mem:rust-clear/keep-sub-live".parse()?;

        let hits = Arc::new(AtomicUsize::new(0));
        let mut sub = mailbox.subscribe(live.clone(), {
            let hits = hits.clone();
            Box::new(move |_message| {
                let hits = hits.clone();
                Box::pin(async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            })
        }).await?;

        mailbox.post(mail("mem:rust-clear/sender", queued.as_str(), json!({ "n": 1 }))).await?;
        assert_eq!(mailbox.status(queued.clone()).await?.unread_count, Some(1));

        mailbox.clear().await?;
        assert_eq!(mailbox.status(queued.clone()).await?.unread_count, Some(0));
        assert_eq!(sub.status(), SubscriptionStatus::Active);

        mailbox.post(mail("mem:rust-clear/sender", live.as_str(), json!({ "n": 2 }))).await?;
        assert!(wait_until(|| {
            let hits = hits.clone();
            async move { hits.load(Ordering::SeqCst) >= 1 }
        }).await, "清理之后旧订阅者必须照常收到新消息");

        sub.unsubscribe().await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_stop_keeps_the_backlog_while_stop_and_purge_drops_it() -> Result<()> {
        // 关闭通道 ≠ 销毁数据：`stop()` 后同一个 scope 上的新 Mailbox 仍看得见残留（老行为）。
        let scope = unique_scope("stop");
        let addr = "mem:rust-stop/keeps-queue";

        for purge in [false, true] {
            let mut first = Mailbox::new();
            first.register_provider(Box::new(MemoryProvider::for_scope(&scope, None)));
            first.start().await?;
            first.post(mail("mem:rust-stop/sender", addr, json!({ "n": 1 }))).await?;

            if purge {
                first.stop_and_purge().await?;
            } else {
                first.stop().await?;
            }

            let second = mailbox_with(Box::new(MemoryProvider::for_scope(&scope, None)));
            second.start().await?;
            let unread = second.status(addr.parse()?).await?.unread_count;
            assert_eq!(unread, Some(if purge { 0 } else { 1 }), "purge = {purge}");
            second.clear().await?; // 收尾：不把残留留给下一个用例
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_stop_and_purge_unsubscribes_then_drops_the_backlog() -> Result<()> {
        let mailbox = mailbox_with(Box::new(MemoryProvider::create_isolated(None)));
        let live = "mem:rust-purge/live";
        let queued = "mem:rust-purge/queue";

        let sub = mailbox.subscribe(live.parse()?, Box::new(|_| Box::pin(async { Ok(()) }))).await?;
        // 有订阅者的地址：走广播，不入队。
        mailbox.post(mail("mem:rust-purge/sender", live, json!({ "n": 0 }))).await?;
        assert_eq!(mailbox.status(live.parse()?).await?.unread_count, Some(0));

        mailbox.post(mail("mem:rust-purge/sender", queued, json!({ "n": 1 }))).await?;
        mailbox.post(mail("mem:rust-purge/sender", queued, json!({ "n": 2 }))).await?;
        let _taken = mailbox.fetch(queued.parse()?, FetchOptions { manual_ack: true, ..Default::default() }).await?;

        let status = mailbox.status(queued.parse()?).await?;
        assert_eq!(status.unread_count, Some(1));
        assert_eq!(status.in_flight_count, Some(1));

        mailbox.stop_and_purge().await?;

        assert_eq!(sub.status(), SubscriptionStatus::Closed); // 先拆订阅
        let status = mailbox.status(queued.parse()?).await?;
        assert_eq!(status.unread_count, Some(0));
        assert_eq!(status.in_flight_count, Some(0)); // in-flight 也算积压
        assert_eq!(mailbox.status(live.parse()?).await?.unread_count, Some(0));
        Ok(())
    }

    #[tokio::test]
    async fn test_stop_and_purge_tolerates_providers_without_a_clear_impl() -> Result<()> {
        // 基类 `clear()` 是 no-op：provider 不实现它，`stop_and_purge()` 也不该报错。
        let broker = remote("nostatus");
        let sent = broker.sent.clone();
        let mailbox = mailbox_with(Box::new(broker));
        mailbox.start().await?;

        mailbox.post(mail("nostatus:a/u", "nostatus:box/inbox", json!({ "n": 1 }))).await?;

        mailbox.clear().await?;
        mailbox.stop_and_purge().await?;
        assert_eq!(sent.lock().unwrap().len(), 1); // broker 里的消息没被销毁
        Ok(())
    }
}
