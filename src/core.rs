//! Provider 基类的投递记账 —— TS `MailboxProvider` / Python `provider.MailboxProvider`
//! 基类里那套「隐式 ack、错误处理、重试、结算」在 Rust 里的对应物。
//!
//! Rust 没有继承，所以共享逻辑不是基类而是一个可复用的 [`ProviderCore`]：具体 provider
//! 持有一个 `Arc<ProviderCore>`，把订阅注册、退订钩子和 ack/nack 的实现交给它，就能得到与
//! 另外两端完全一致的投递语义。自己直调用户回调 = 这次投递没人记账，失败既不回队也不死信。
//!
//! 核心是**投递级**记账（issue#2）：`subscribe` 是广播，一条消息 = N 次投递，每次投递单独
//! 结算；「整条消息要不要回队」由发送侧的结算 barrier 在全部投递落定后按
//! [`crate::delivery::should_requeue_after_broadcast`] 统一判定。

use async_trait::async_trait;
use futures::channel::oneshot;
use futures::future::{self, BoxFuture};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use url::Url;

use crate::delivery::{
    self, delivery_key, subscription_id_of_delivery, DeliveryResult, FailureReason,
    DELIVERY_KEY_SEPARATOR,
};
use crate::error::Result;
use crate::message::MailMessage;
use crate::provider::{Subscription, SubscriptionHandler, SubscriptionStatus};
use crate::retry::{next_retry_delay, AttemptLedger, RetryPolicy};

/// 尝试台账的 TTL：长期运行的进程里不能让台账无限增长。
pub const DEFAULT_ATTEMPT_LEDGER_TTL: Duration = Duration::from_secs(300);

/// 注册给具体 provider 的订阅监听器：返回本次投递的**最终结算**（含整条重试链）。
pub type DeliveryListener =
    Arc<dyn Fn(MailMessage) -> BoxFuture<'static, DeliveryResult> + Send + Sync>;

/// provider 侧的退订实现（从自己的总线 / broker 上摘掉监听器）。
pub type UnsubscribeHook = Box<dyn Fn() -> BoxFuture<'static, Result<()>> + Send + Sync>;
/// 隐式 ACK 的实现（内存型 provider 通常是空实现）。
pub type AckHook = Arc<dyn Fn(MailMessage) -> BoxFuture<'static, Result<()>> + Send + Sync>;
/// NACK 的实现。广播型 provider 里它只会在整条广播结算后被告知一次。
pub type NackHook = Arc<dyn Fn(MailMessage, bool) -> BoxFuture<'static, Result<()>> + Send + Sync>;
/// 死信钩子：广播已结算、有投递失败但消息**不**回队时用（部分失败 / 全部不可重试）。
pub type DeadLetterHook = Arc<dyn Fn(MailMessage, Vec<DeliveryResult>) + Send + Sync>;
/// 重试次数耗尽的钩子。
pub type RetryExhaustedHook = Arc<dyn Fn(MailMessage, u32) + Send + Sync>;

/// 构造 [`ProviderCore`] 的配置。
pub struct ProviderCoreOptions {
    /// 订阅失败后的重试策略。`None` 表示不重试（默认）：可重试失败只把消息退回队列，
    /// 由 `fetch()` 兜底。重试是 opt-in 的，因为 AMQP / SQS 这类传输自带重投与死信。
    pub retry: Option<RetryPolicy>,
    /// 这个 provider 的 `subscribe` 是不是广播（一条消息投递给 N 个订阅者）。
    ///
    /// 广播型（`true`，如 MemoryProvider）：单次投递失败**不再**自动 `nack(requeue)` 整条消息 ——
    /// 已成功订阅者的副作用不该再执行一次。回不回队由发送侧的结算 barrier 统一判定
    /// （全部投递都失败才回队）。
    ///
    /// 队列 / 传输型（`false`，默认）：每次投递对应 broker 里的一份 ack，保持原有
    /// per-delivery `nack` 行为，交给传输层重投或死信。
    pub broadcast: bool,
    /// 单次投递尝试的结算超时。`Duration::ZERO` 关闭（调用方自己负责长任务）。
    pub delivery_settle_timeout: Duration,
    /// 尝试台账的 TTL。
    pub attempt_ledger_ttl: Duration,
    pub ack: Option<AckHook>,
    pub nack: Option<NackHook>,
}

impl Default for ProviderCoreOptions {
    fn default() -> Self {
        Self {
            retry: None,
            broadcast: false,
            delivery_settle_timeout: delivery::DEFAULT_DELIVERY_SETTLE_TIMEOUT,
            attempt_ledger_ttl: DEFAULT_ATTEMPT_LEDGER_TTL,
            ack: None,
            nack: None,
        }
    }
}

struct ManagedSubscription {
    address: Url,
    /// 用户传入的原始回调。失败处理与重试由 core 驱动（见 `run_delivery`）。
    on_receive: Arc<SubscriptionHandler>,
    /// provider 提供的退订实现，由 `attach_unsubscribe` 回填。
    unsubscribe: Option<UnsubscribeHook>,
}

/// 一次尚未结算的投递（某个订阅者对某条消息的处理，可能正卡在重试退避里）。
/// 只有结算之后，广播的发送侧才知道这次投递是成功还是失败。
struct PendingDelivery {
    message: MailMessage,
    /// 投递身份（`subscription_id + message_id`）。内部 map 键可能带并发序号后缀。
    delivery_id: String,
    subscription_id: String,
    /// 本次投递的结算通道。广播的 barrier 等的就是它。
    settle: Option<oneshot::Sender<DeliveryResult>>,
    /// 投递条目序号：迟到的定时器（退避 / 看门狗）凭它作废，不会打到复用同键的新投递。
    generation: u64,
    /// 退避重投令牌 + 「是否有定时器在等」标记。
    retry_token: u64,
    retry_scheduled: bool,
    /// 看门狗令牌：只在尝试进行中计时，退避期间靠推进令牌关掉。
    watchdog_token: u64,
}

#[derive(Default)]
struct CoreState {
    subscriptions: HashMap<String, ManagedSubscription>,
    /// 按投递身份记账尚未结算的投递。
    pending: HashMap<String, PendingDelivery>,
    /// 已登记结算 barrier 的消息 id → 登记层数（同 id 并发广播时不能互相抹掉）。
    barriers: HashMap<String, u32>,
    /// 已经告过警的「漏接 barrier」消息 id，避免一条消息警 N 次。
    barrierless_warned: HashSet<String>,
}

/// 具体 provider 共享的投递记账内核。用 `Arc` 持有：投递与定时器任务都要跨任务引用它。
pub struct ProviderCore {
    protocol: String,
    broadcast: bool,
    state: RwLock<CoreState>,
    attempts: Mutex<AttemptLedger>,
    retry: RwLock<Option<RetryPolicy>>,
    settle_timeout: RwLock<Duration>,
    ack: AckHook,
    nack: NackHook,
    dead_letter: RwLock<Option<DeadLetterHook>>,
    retry_exhausted: RwLock<Option<RetryExhaustedHook>>,
    generation: AtomicU64,
}

impl ProviderCore {
    pub fn new(protocol: impl Into<String>, options: ProviderCoreOptions) -> Arc<Self> {
        let noop_ack = |_message: MailMessage| -> BoxFuture<'static, Result<()>> {
            Box::pin(async { Ok(()) })
        };
        let noop_nack = |_message: MailMessage, _requeue: bool| -> BoxFuture<'static, Result<()>> {
            Box::pin(async { Ok(()) })
        };
        Arc::new(Self {
            protocol: protocol.into(),
            broadcast: options.broadcast,
            state: RwLock::new(CoreState::default()),
            attempts: Mutex::new(AttemptLedger::new(options.attempt_ledger_ttl)),
            retry: RwLock::new(options.retry.map(|policy| policy.resolved())),
            settle_timeout: RwLock::new(options.delivery_settle_timeout),
            ack: options.ack.unwrap_or_else(|| Arc::new(noop_ack)),
            nack: options.nack.unwrap_or_else(|| Arc::new(noop_nack)),
            dead_letter: RwLock::new(None),
            retry_exhausted: RwLock::new(None),
            generation: AtomicU64::new(1),
        })
    }

    pub fn protocol(&self) -> &str {
        &self.protocol
    }

    /// 这个 provider 的 `subscribe` 是不是广播。
    pub fn is_broadcast(&self) -> bool {
        self.broadcast
    }

    /// 开启 / 关闭订阅重试。传 `None` 回到默认行为（失败只回队、不自动重投）。
    pub fn set_retry_policy(&self, policy: Option<RetryPolicy>) {
        *self.retry.write().unwrap() = policy.map(|policy| policy.resolved());
    }

    pub fn retry_policy(&self) -> Option<RetryPolicy> {
        *self.retry.read().unwrap()
    }

    /// 单次投递尝试的结算超时；`Duration::ZERO` 表示关闭看门狗。
    pub fn set_delivery_settle_timeout(&self, timeout: Duration) {
        *self.settle_timeout.write().unwrap() = timeout;
    }

    pub fn delivery_settle_timeout(&self) -> Duration {
        *self.settle_timeout.read().unwrap()
    }

    /// 死信钩子：不回队的那些失败投递在这里可见（写死信队列 / 告警）。
    pub fn set_dead_letter_hook(&self, hook: Option<DeadLetterHook>) {
        *self.dead_letter.write().unwrap() = hook;
    }

    pub fn set_retry_exhausted_hook(&self, hook: Option<RetryExhaustedHook>) {
        *self.retry_exhausted.write().unwrap() = hook;
    }

    /// 当前尚未结算的投递数量（诊断 / 测试用）。
    pub fn pending_delivery_count(&self) -> usize {
        self.state.read().unwrap().pending.len()
    }

    pub fn subscription_count(&self) -> usize {
        self.state.read().unwrap().subscriptions.len()
    }

    // --- 订阅登记（provider 与 core 的接线处）---

    /// 登记一个订阅：core 保存用户回调并返回要注册到 provider 总线上的监听器。
    ///
    /// 返回的监听器必须在 core 之外再注册一次（`attach_unsubscribe` 补上退订实现），
    /// 因为「怎么把监听器摘下来」是 provider 自己的事。
    pub fn register_subscription(
        self: &Arc<Self>,
        address: Url,
        on_receive: SubscriptionHandler,
    ) -> (String, DeliveryListener) {
        let id = uuid::Uuid::new_v4().to_string();
        let listener = self.build_listener(&id);
        let mut state = self.state.write().unwrap();
        state.subscriptions.insert(
            id.clone(),
            ManagedSubscription {
                address,
                on_receive: Arc::new(on_receive),
                unsubscribe: None,
            },
        );
        (id, listener)
    }

    /// 补上退订实现（`register_subscription` 之后、返回 `Subscription` 之前调用）。
    pub fn attach_unsubscribe(&self, subscription_id: &str, hook: UnsubscribeHook) {
        if let Some(sub) = self.state.write().unwrap().subscriptions.get_mut(subscription_id) {
            sub.unsubscribe = Some(hook);
        }
    }

    /// 给订阅者返回的句柄。`unsubscribe()` 会同时取消该订阅尚未结算的投递。
    pub fn subscription_object(self: &Arc<Self>, subscription_id: String) -> Box<dyn Subscription> {
        Box::new(CoreSubscription {
            core: self.clone(),
            id: subscription_id,
        })
    }

    /// 某个订阅是否仍活跃。
    pub fn is_subscribed(&self, subscription_id: &str) -> bool {
        self.state.read().unwrap().subscriptions.contains_key(subscription_id)
    }

    /// 订阅地址（退订后为 `None`）。
    pub fn subscription_address(&self, subscription_id: &str) -> Option<Url> {
        self.state
            .read()
            .unwrap()
            .subscriptions
            .get(subscription_id)
            .map(|sub| sub.address.clone())
    }

    /// 构造注册到 provider 总线上的监听器：投递入口 = 结算 future。
    pub fn build_listener(self: &Arc<Self>, subscription_id: &str) -> DeliveryListener {
        let core = self.clone();
        let subscription_id = subscription_id.to_string();
        Arc::new(move |message: MailMessage| {
            core.clone().start_delivery(&subscription_id, message)
        })
    }

    // --- 投递入口 ---

    ///
    /// 把一条消息投递给本 provider 的某个**本地订阅者**，走与 `subscribe` 完全相同的投递记账
    /// （隐式 ack、错误处理、重试、结算）。生产者型 provider（自己组装消息再分发）必须走这里，
    /// 不要直接调用户回调 —— 那样这次投递没人记账，失败也不会回队。
    pub fn deliver_to_local_subscription(
        self: &Arc<Self>,
        subscription_id: &str,
        message: MailMessage,
    ) -> BoxFuture<'static, DeliveryResult> {
        self.clone().start_delivery(subscription_id, message)
    }

    /// 投递入口：注册一个待结算投递，然后跑第一次尝试。
    /// 返回的 future 在投递最终结算时 resolve（永不 panic）—— 广播的 barrier 等的就是它。
    ///
    /// 内部记账键由 `reserve_delivery_key` 分配：`post()` 允许自定义消息 id，同一个订阅者对
    /// 同 id 的并发投递如果共用一个键，后者会覆盖前者的结算入口 —— 那条投递永不结算，
    /// 对应的 barrier 永不落定，消息静默消失。
    fn start_delivery(
        self: Arc<Self>,
        subscription_id: &str,
        message: MailMessage,
    ) -> BoxFuture<'static, DeliveryResult> {
        let (key, settlement) = self.reserve_delivery_key(subscription_id, message.clone());
        self.run_delivery(subscription_id, message.clone(), &key);

        let subscription_id = subscription_id.to_string();
        Box::pin(async move {
            settlement.await.unwrap_or_else(|_| {
                // 结算通道断了（provider 提前丢掉了这次投递）：按可重试失败处理，
                // 宁可多兜一次，也不能让消息静默消失。
                DeliveryResult::failed(&subscription_id, &message.id, FailureReason::Retriable)
            })
        })
    }

    /// 内部记账键：同 `(subscription_id, message_id)` 的并发投递各自独立。
    fn reserve_delivery_key(
        &self,
        subscription_id: &str,
        message: MailMessage,
    ) -> (String, oneshot::Receiver<DeliveryResult>) {
        let base = delivery_key(subscription_id, &message.id);
        let mut state = self.state.write().unwrap();
        let mut key = base.clone();
        let mut suffix = 1u32;
        while state.pending.contains_key(&key) {
            key = format!("{}{}{}", base, DELIVERY_KEY_SEPARATOR, suffix);
            suffix += 1;
        }

        let (settle, settlement) = oneshot::channel();
        let generation = self.generation.fetch_add(1, Ordering::Relaxed);
        state.pending.insert(
            key.clone(),
            PendingDelivery {
                message,
                delivery_id: base,
                subscription_id: subscription_id.to_string(),
                settle: Some(settle),
                generation,
                retry_token: 0,
                retry_scheduled: false,
                watchdog_token: 0,
            },
        );
        (key, settlement)
    }

    /// 跑一次尝试。成功 → 隐式 ack + 结算 `ok`；失败 → 交给 `handle_delivery_error`。
    fn run_delivery(self: &Arc<Self>, subscription_id: &str, message: MailMessage, key: &str) {
        let handler = self
            .state
            .read()
            .unwrap()
            .subscriptions
            .get(subscription_id)
            .map(|sub| sub.on_receive.clone());

        let handler = match handler {
            Some(handler) => handler,
            None => {
                // 等待期间被退订 / provider 已关闭：不再调用回调，按取消结算。
                self.settle_and_fallback(key, FailureReason::Cancelled);
                return;
            }
        };

        // 看门狗只给「一次尝试」计时：卡死的回调会拖住整条广播的结算判定（消息既不回队也不死信）。
        self.arm_watchdog(key);

        let core = self.clone();
        let subscription_id = subscription_id.to_string();
        let key = key.to_string();

        spawn(async move {
            let attempt = handler(message.clone()).await;
            match attempt {
                Ok(()) => {
                    // On successful execution, implicitly acknowledge the message.
                    core.ack(message.clone()).await;
                    core.attempts.lock().unwrap().reset(&subscription_id, &message.id);
                    core.settle_delivery(&key, None);
                }
                Err(error) => {
                    core.handle_delivery_error(&subscription_id, message, &key, error)
                        .await;
                }
            }
        });
    }

    /// 退避到期：重投给失败的那个订阅者。
    fn fire_retry(self: &Arc<Self>, subscription_id: String, message: MailMessage, key: &str, generation: u64, token: u64) {
        {
            let mut state = self.state.write().unwrap();
            let pending = match state.pending.get_mut(key) {
                Some(pending) if pending.generation == generation => pending,
                // 退避期间已结算（退订 / 关闭）
                _ => return,
            };
            if pending.retry_token != token {
                return; // 定时器作废（期间已重投 / 已取消）
            }
            pending.retry_scheduled = false;
        }

        if !self.is_subscribed(&subscription_id) {
            // 等待期间退订了：不重投给已消失的订阅者，按取消结算（消息由兜底路径接手）。
            self.settle_and_fallback(key, FailureReason::Cancelled);
            return;
        }
        self.run_delivery(&subscription_id, message, key);
    }

    // --- 失败处理与重试 ---

    /// 订阅回调失败时的统一处理（投递级）。
    ///
    /// 默认（无重试策略）：可重试 → 结算 `Retriable`；不可重试 → 结算 `Fatal`（死信）。
    /// 开启重试后：可重试且未超上限 → 只重投给失败的那个订阅者（不广播给其他已成功的订阅者，
    /// 否则它们的副作用会被执行两次）；超上限 → 清零台账、调重试耗尽钩子，再结算 `Retriable`。
    ///
    /// 广播型 provider 不在这里 `nack`：整条消息回不回队由发送侧在所有投递结算后统一判定
    /// （见 `should_requeue_after_broadcast`）。没接 barrier 的广播 provider 由
    /// `fallback_for_missing_barrier` 兜底（整条回队 + 告警），否则可重试失败就静默消失了。
    /// 非广播型 provider 每次投递对应 broker 的一次 ack，保持原有 per-delivery `nack(true)`。
    async fn handle_delivery_error(
        self: &Arc<Self>,
        subscription_id: &str,
        message: MailMessage,
        key: &str,
        error: crate::error::MailboxError,
    ) {
        eprintln!("[{}] Error processing message {}: {}", self.protocol, message.id, error);

        let is_retriable = error.is_retriable();
        let policy = self.retry_policy();

        if is_retriable {
            if let Some(policy) = policy {
                let attempt = self.attempts.lock().unwrap().record(subscription_id, &message.id);

                if attempt < policy.max_attempts && self.is_subscribed(subscription_id) {
                    let delay = next_retry_delay(&policy, attempt);
                    let (generation, token) = {
                        let mut state = self.state.write().unwrap();
                        match state.pending.get_mut(key) {
                            // 已经结算（例如退订），不重投
                            None => return,
                            Some(pending) => {
                                pending.retry_token += 1;
                                pending.retry_scheduled = true;
                                // 退避等待不是“卡死的尝试”：看门狗只在尝试进行中计时，退避期间关掉，
                                // 否则一个比看门狗还长的退避会被自己掐断。
                                pending.watchdog_token += 1;
                                (pending.generation, pending.retry_token)
                            }
                        }
                    };

                    let core = self.clone();
                    let subscription_id = subscription_id.to_string();
                    let key = key.to_string();
                    spawn(async move {
                        sleep(delay).await;
                        core.fire_retry(subscription_id, message, &key, generation, token);
                    });
                    return;
                }

                if attempt >= policy.max_attempts {
                    self.attempts.lock().unwrap().reset(subscription_id, &message.id);
                    let hook = self.retry_exhausted.read().unwrap().clone();
                    if let Some(hook) = hook {
                        hook(message.clone(), attempt);
                    }
                }
            }

            if !self.broadcast {
                // Crucial: NACK the message first to prevent reprocessing, then handle other tasks.
                self.nack(message.clone(), true).await;
            }
        }

        self.settle_and_fallback(
            key,
            if is_retriable {
                FailureReason::Retriable
            } else {
                FailureReason::Fatal
            },
        );
    }

    // --- 结算 ---

    /// 结算一条投递；广播型 provider 若这条消息没接 barrier，走兜底。
    fn settle_and_fallback(self: &Arc<Self>, key: &str, reason: FailureReason) {
        let result = self.settle_delivery(key, Some(reason));
        if let Some(result) = result {
            if self.broadcast {
                let message = result.message.clone();
                self.fallback_for_missing_barrier(message, result.result);
            }
        }
    }

    /// 放弃一条投递（退避中退订 / 看门狗超时）：按取消结算，消息交给兜底路径。
    fn abandon_delivery(self: &Arc<Self>, key: &str, reason: FailureReason) {
        let result = match self.settle_delivery(key, Some(reason)) {
            Some(result) => result,
            None => return,
        };
        if self.broadcast {
            let message = result.message.clone();
            self.fallback_for_missing_barrier(message, result.result);
            return;
        }
        // 非广播（传输型）：每次投递对应 broker 的一次 ack，交回传输层重投。
        let core = self.clone();
        let message = result.message.clone();
        spawn(async move { core.nack(message, true).await });
    }

    /// 结算一条投递（幂等：已结算过则返回 `None`，不会重复投）。
    fn settle_delivery(&self, key: &str, reason: Option<FailureReason>) -> Option<SettledDelivery> {
        let pending = self.state.write().unwrap().pending.remove(key)?;

        let result = if reason.is_none() {
            DeliveryResult::succeeded(&pending.subscription_id, &pending.message.id)
        } else {
            DeliveryResult::failed(
                &pending.subscription_id,
                &pending.message.id,
                // reason 为 Some 时已在调用点确定；缺省按可重试兜底
                reason.unwrap_or(FailureReason::Retriable),
            )
        };
        // 投递身份用登记时的值（并发同 id 的键带后缀，身份不带）。
        let result = DeliveryResult {
            delivery_id: pending.delivery_id.clone(),
            ..result
        };

        if let Some(settle) = pending.settle {
            let _ = settle.send(result.clone());
        }

        Some(SettledDelivery {
            message: pending.message,
            result,
        })
    }

    /// 取消某个订阅者尚未结算的投递（退订 / 关闭）。
    ///
    /// 只取消「等待重投」的投递（有退避定时器）。正在执行的回调不替它下结论：
    /// 提前结算成 `Cancelled` 会被 barrier 误判为“无人成功”→ 整条消息回队 →
    /// 已经执行过的副作用被 `fetch()` 消费者再跑一次（重复投递，换了个入口）。
    /// 卡死的回调由看门狗兜底。
    fn cancel_pending_deliveries(self: &Arc<Self>, subscription_id: &str) {
        let prefix = delivery_key(subscription_id, "");
        let keys: Vec<String> = self
            .state
            .read()
            .unwrap()
            .pending
            .iter()
            .filter(|(key, pending)| key.starts_with(&prefix) && pending.retry_scheduled)
            .map(|(key, _)| key.clone())
            .collect();

        for key in keys {
            // 作废退避定时器（令牌推进），然后按取消结算。
            if let Some(pending) = self.state.write().unwrap().pending.get_mut(&key) {
                pending.retry_token += 1;
                pending.retry_scheduled = false;
            }
            self.abandon_delivery(&key, FailureReason::Cancelled);
        }
    }

    // --- 结算 barrier ---

    /// 广播的结算 barrier：把 `publish` 返回的每次投递绑成一个后台任务，全部投递
    /// （含重试链）落定后统一判定要不要 `nack(requeue)`。给广播型 provider 的发送侧用。
    ///
    /// 不阻塞 `post()`：`post()` 在派发完成时就返回，判定在投递结算后才发生。
    /// “按成功订阅者数 > 0 跳过回队”在 fire-and-forget 广播上会误判 ——
    /// 先失败的订阅者可能比慢而成功的订阅者先结算。
    ///
    /// barrier 必须在**任何 await 之前**同步登记（即在 provider 的发送路径里直接调用），
    /// 否则同步失败的投递会先看到“无 barrier”。
    pub fn track_broadcast_settlement(
        self: &Arc<Self>,
        message: MailMessage,
        deliveries: Vec<BoxFuture<'static, DeliveryResult>>,
    ) {
        // 登记 barrier：core 靠它区分「有人在判定」和「provider 漏接了 barrier」。
        // 必须在任何 await 之前同步登记，否则派发时同步失败的投递会先看到“无 barrier”。
        let barrier = self.register_broadcast_barrier(&message.id);

        let core = self.clone();
        spawn(async move {
            // 判定任务结束时释放 barrier（panic / 提前 return 也算）：barrier 不放，
            // 后续同 id 消息的“漏接 barrier”兜底判定就永远不会触发。
            let _barrier = barrier;

            let settled = future::join_all(deliveries)
                .await
                .into_iter()
                .map(Ok)
                .collect();
            let results = delivery::to_delivery_results(settled);
            let requeue = delivery::should_requeue_after_broadcast(&results);

            if !requeue {
                let failed: Vec<DeliveryResult> =
                    results.into_iter().filter(|result| !result.ok).collect();
                if !failed.is_empty() {
                    let hook = core.dead_letter.read().unwrap().clone();
                    if let Some(hook) = hook {
                        hook(message.clone(), failed);
                    }
                }
            }
            core.nack(message.clone(), requeue).await;
        });
    }

    /// 把一条消息的 barrier 登记一层，返回的 guard 在 drop 时释放一层。
    fn register_broadcast_barrier(self: &Arc<Self>, message_id: &str) -> BarrierGuard {
        let mut state = self.state.write().unwrap();
        *state.barriers.entry(message_id.to_string()).or_insert(0) += 1;
        BarrierGuard {
            core: self.clone(),
            message_id: message_id.to_string(),
        }
    }

    fn release_broadcast_barrier(&self, message_id: &str) {
        let mut state = self.state.write().unwrap();
        let depth = state.barriers.get(message_id).copied().unwrap_or(0);
        if depth > 1 {
            state.barriers.insert(message_id.to_string(), depth - 1);
        } else {
            state.barriers.remove(message_id);
        }
    }

    /// 这条消息当前有没有正在判定的 barrier（同 id 并发广播时按层数计）。
    fn barrier_depth(&self, message_id: &str) -> u32 {
        self.state.read().unwrap().barriers.get(message_id).copied().unwrap_or(0)
    }

    ///
    /// 广播型 provider 的兜底：投递已结算，但这条消息没有 barrier —— 说明发送侧漏接了
    /// `track_broadcast_settlement`，没人做回队判定。宁可整条回队让 `fetch()` 兜底
    /// （“不得静默丢失”），也不能让消息消失；同时告警提示 provider 作者。
    ///
    /// 推迟一个任务 tick 再判定：派发里同步失败的投递跑在发送侧登记 barrier 之前。
    fn fallback_for_missing_barrier(self: &Arc<Self>, message: MailMessage, result: DeliveryResult) {
        if result.ok || self.barrier_depth(&message.id) > 0 {
            return;
        }
        let core = self.clone();
        spawn(async move {
            #[cfg(not(target_arch = "wasm32"))]
            tokio::task::yield_now().await;
            core.fallback_after_tick(message, result);
        });
    }

    fn fallback_after_tick(self: &Arc<Self>, message: MailMessage, result: DeliveryResult) {
        if self.barrier_depth(&message.id) > 0 {
            return; // barrier 已登记（同一轮内），判定交给它
        }
        self.warn_missing_barrier(&message.id);
        if result.reason == Some(FailureReason::Fatal) {
            let hook = self.dead_letter.read().unwrap().clone();
            if let Some(hook) = hook {
                hook(message, vec![result]);
            }
            return;
        }
        let core = self.clone();
        spawn(async move { core.nack(message, true).await });
    }

    fn warn_missing_barrier(&self, message_id: &str) {
        let mut state = self.state.write().unwrap();
        if state.barrierless_warned.contains(message_id) {
            return;
        }
        if state.barrierless_warned.len() > 500 {
            state.barrierless_warned.clear(); // 不让告警名单无限增长
        }
        state.barrierless_warned.insert(message_id.to_string());
        drop(state);
        eprintln!(
            "[{}] A broadcast delivery of message {} settled without a settlement barrier. \
             A broadcast provider must call `ProviderCore::track_broadcast_settlement(message, deliveries)` \
             in its send path (synchronously, before any await) or retriable failures will fall back to whole-message requeue.",
            self.protocol, message_id
        );
    }

    // --- 结算看门狗 ---

    /// 给当前这次尝试装一个结算看门狗。只在尝试开始时计时，退避等待期间不计时，
    /// 所以不会掐断健康的重试链。
    fn arm_watchdog(self: &Arc<Self>, key: &str) {
        // wasm32 下 tokio 未启用 `time` feature：没有可用的计时器，看门狗退化为不启用。
        #[cfg(target_arch = "wasm32")]
        {
            let _ = key;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let timeout = self.delivery_settle_timeout();
            if timeout.is_zero() {
                return; // 显式关闭：长任务由调用方自己负责
            }
            let (generation, token) = {
                let mut state = self.state.write().unwrap();
                match state.pending.get_mut(key) {
                    None => return,
                    Some(pending) => {
                        pending.watchdog_token += 1;
                        (pending.generation, pending.watchdog_token)
                    }
                }
            };

            let core = self.clone();
            let key = key.to_string();
            spawn(async move {
                sleep(timeout).await;
                core.on_delivery_stall(&key, generation, token);
            });
        }
    }

    /// 看门狗到期：回调还在跑（卡死 / 超长）。按取消结算，消息回到兜底路径；
    /// 回调事后成功不会重复结算（见 `settle_delivery` 的一次性守卫）。
    ///
    /// 只在能被看门狗触发的目标上存在（wasm32 无计时器，见 `arm_watchdog`）。
    #[cfg(not(target_arch = "wasm32"))]
    fn on_delivery_stall(self: &Arc<Self>, key: &str, generation: u64, token: u64) {
        let (delivery_id, timeout) = {
            let state = self.state.read().unwrap();
            match state.pending.get(key) {
                Some(pending) if pending.generation == generation && pending.watchdog_token == token => {
                    (pending.delivery_id.clone(), self.delivery_settle_timeout())
                }
                // 已结算或已进入退避：不是“卡死的尝试”
                _ => return,
            }
        };
        eprintln!(
            "[{}] Delivery {} did not settle within {:?} (subscriber callback still running). \
             Settling it as cancelled so the message can be requeued; raise `set_delivery_settle_timeout` \
             (Duration::ZERO disables) if this delivery is expected to take longer.",
            self.protocol, delivery_id, timeout
        );
        self.abandon_delivery(key, FailureReason::Cancelled);
    }

    // --- ack / nack（provider 提供实现）---

    pub fn ack(&self, message: MailMessage) -> BoxFuture<'static, ()> {
        let hook = self.ack.clone();
        let protocol = self.protocol.clone();
        Box::pin(async move {
            if let Err(error) = hook(message.clone()).await {
                eprintln!("[{}] CRITICAL: Failed to ACK message {}. {}", protocol, message.id, error);
            }
        })
    }

    /// NACK。广播型 provider 里它只会在整条广播结算后被调用一次（`requeue` 由判定给出）。
    /// 失败不会打断投递记账，但一定会告警：回队失败 = 消息可能重复或丢失。
    pub fn nack(&self, message: MailMessage, requeue: bool) -> BoxFuture<'static, ()> {
        let hook = self.nack.clone();
        let protocol = self.protocol.clone();
        Box::pin(async move {
            if let Err(error) = hook(message.clone(), requeue).await {
                eprintln!(
                    "[{}] CRITICAL: Failed to NACK message {}. Risk of message duplication. {}",
                    protocol, message.id, error
                );
            }
        })
    }

    // --- 生命周期 ---

    /// 关闭：取消全部活跃订阅，再取消「等待重投」的投递，最后清零台账。
    ///
    /// 正在执行的回调不替它下结论（它可能马上就要成功）：由它自己结算，卡死的由看门狗兜。
    pub fn close(self: &Arc<Self>) -> BoxFuture<'static, ()> {
        let core = self.clone();
        Box::pin(async move {
            let hooks: Vec<(String, UnsubscribeHook)> = {
                let mut state = core.state.write().unwrap();
                state
                    .subscriptions
                    .drain()
                    .filter_map(|(id, sub)| sub.unsubscribe.map(|hook| (id, hook)))
                    .collect()
            };
            for (_id, hook) in hooks {
                if let Err(error) = hook().await {
                    eprintln!("[{}] Error unsubscribing during close: {}", core.protocol, error);
                }
            }

            // 取消不等于消息可以消失 —— 见 `cancel_pending_deliveries`。
            let subscription_ids: Vec<String> = core
                .state
                .read()
                .unwrap()
                .pending
                .keys()
                .map(|key| subscription_id_of_delivery(key))
                .collect();
            for subscription_id in subscription_ids {
                core.cancel_pending_deliveries(&subscription_id);
            }
            core.attempts.lock().unwrap().clear();
        })
    }
}

struct SettledDelivery {
    message: MailMessage,
    result: DeliveryResult,
}

/// barrier 计数的 RAII 守卫：drop 即释放一层（见 `track_broadcast_settlement`）。
struct BarrierGuard {
    core: Arc<ProviderCore>,
    message_id: String,
}

impl Drop for BarrierGuard {
    fn drop(&mut self) {
        self.core.release_broadcast_barrier(&self.message_id);
    }
}

struct CoreSubscription {
    core: Arc<ProviderCore>,
    id: String,
}

#[async_trait]
impl Subscription for CoreSubscription {
    fn id(&self) -> &str {
        &self.id
    }

    fn status(&self) -> SubscriptionStatus {
        if self.core.is_subscribed(&self.id) {
            SubscriptionStatus::Active
        } else {
            SubscriptionStatus::Closed
        }
    }

    fn address(&self) -> Option<Url> {
        self.core.subscription_address(&self.id)
    }

    async fn unsubscribe(&mut self) -> Result<()> {
        let hook = {
            let mut state = self.core.state.write().unwrap();
            state.subscriptions.remove(&self.id).and_then(|sub| sub.unsubscribe)
        };
        // 先摘订阅，再取消待重投：正在执行的回调不会被替它下结论。
        self.core.cancel_pending_deliveries(&self.id);
        if let Some(hook) = hook {
            hook().await?;
        }
        Ok(())
    }
}

/// 后台任务：投递、退避定时器、看门狗都靠它，绝不阻塞 `post()`。
#[cfg(not(target_arch = "wasm32"))]
fn spawn<F: Future<Output = ()> + Send + 'static>(future: F) {
    tokio::spawn(future);
}

#[cfg(target_arch = "wasm32")]
fn spawn<F: Future<Output = ()> + 'static>(future: F) {
    wasm_bindgen_futures::spawn_local(future);
}

/// 退避等待。wasm32 下 tokio 未启用 `time` feature，退化为不等待（仍受 `max_attempts` 约束）。
async fn sleep(delay: Duration) {
    #[cfg(not(target_arch = "wasm32"))]
    tokio::time::sleep(delay).await;
    #[cfg(target_arch = "wasm32")]
    let _ = delay;
}
