use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use crate::message::Identifiable;
use crate::utils::{copy_for_peek, normalize_peek_limit};

/// 手动 ACK 模式下未指定 `ack_timeout` 时的默认超时。
///
/// in-flight 消息必须有界：否则消费者崩溃后既取不回来、也不会
/// 在状态里可见，等于静默丢失。该缺省值与三态语义在 TS / Python / Rust 三端一致。
pub const DEFAULT_ACK_TIMEOUT: Duration = Duration::from_secs(30);

/// 解析 `ack_timeout` 三态：`None` → 默认超时；`Some(ZERO)` → 关闭 stale 扫描
/// （消息常驻 in-flight 直到 ack/nack）；`Some(t)` → 该超时。
/// 不能把“不传”当作“永不超时”，那会让未 ack 的消息永久泄漏。
fn resolve_ack_timeout(ack_timeout: Option<Duration>) -> Option<Duration> {
    match ack_timeout {
        None => Some(DEFAULT_ACK_TIMEOUT),
        Some(t) if t == Duration::ZERO => None,
        Some(t) => Some(t),
    }
}

#[derive(Debug, Clone)]
struct InFlightMessage<T> {
    message: T,
    timestamp: Instant,
    topic: String,
}

pub struct MailMessageQueue<T> {
    queues: HashMap<String, VecDeque<T>>,
    in_flight: HashMap<String, InFlightMessage<T>>,
}

impl<T> MailMessageQueue<T>
where T: Clone + Identifiable
{
    pub fn new() -> Self {
        Self {
            queues: HashMap::new(),
            in_flight: HashMap::new(),
        }
    }

    pub fn enqueue(&mut self, topic: String, message: T) {
        self.queues
            .entry(topic)
            .or_insert_with(VecDeque::new)
            .push_back(message);
    }

    pub fn dequeue(&mut self, topic: &str) -> Option<T> {
        self.queues.get_mut(topic)?.pop_front()
    }

    pub fn dequeue_for_ack(
        &mut self,
        topic: &str,
        ack_timeout: Option<Duration>
    ) -> Option<T> {
        if let Some(timeout) = resolve_ack_timeout(ack_timeout) {
            self.requeue_stale(topic, timeout);
        }

        let message = self.queues.get_mut(topic)?.pop_front()?;
        let id = message.id().to_string();

        self.in_flight.insert(id, InFlightMessage {
            message: message.clone(),
            timestamp: Instant::now(),
            topic: topic.to_string(),
        });

        Some(message)
    }

    pub fn ack(&mut self, message_id: &str) {
        self.in_flight.remove(message_id);
    }

    pub fn nack(&mut self, message_id: &str, requeue: bool) {
        if let Some(flight) = self.in_flight.remove(message_id) {
            if requeue {
                self.requeue_internal(flight.topic, flight.message);
            }
        }
    }

    /// 队列里等待投递的消息数。只有这个数可以当作“待处理”；
    /// in-flight 的消息未 ack 时会回到队列，用 [`in_flight_count`](Self::in_flight_count) 单独观测。
    pub fn get_status(&self, topic: &str) -> usize {
        self.queues.get(topic).map(|q| q.len()).unwrap_or(0)
    }

    /// 【只读】看一眼队列头部最多 `limit` 条消息，**不**改动任何投递状态。
    ///
    /// 诊断 / 审计需要「看一眼但不消费」。在这之前最接近的做法是 `fetch(..., manual_ack = true)`
    /// 然后不 ack（issue#4）：那会把消息弄进 in-flight（默认 30s 后重投），既不是只读，
    /// 也会干扰正常消费者的 `unread_count` / `in_flight_count`。
    ///
    /// 保证：
    /// - 不出队、不建 in-flight、不动 stale 扇描、不改任何时间戳；
    /// - 返回副本（见 [`crate::utils::copy_for_peek`]），调用方改不到队列里的对象；
    /// - 只涵盖**等待中**的消息；in-flight（已被消费者取走、尚未 ack）不在快照里 ——
    ///   它属于那个消费者，只能在 `status().in_flight_count` 里看到。
    ///
    /// @param topic 要快照的 topic。
    /// @param limit 最多返回几条；`None` 则整个队列。
    /// @returns 按 FIFO 顺序的消息副本数组；队列为空 / topic 未知时是空数组（不是 `None`）。
    pub fn peek(&self, topic: &str, limit: Option<usize>) -> Vec<T> {
        let limit = normalize_peek_limit(limit);
        let queue = match self.queues.get(topic) {
            Some(queue) if !queue.is_empty() => queue,
            _ => return Vec::new(),
        };

        let count = limit.unwrap_or(queue.len()).min(queue.len());
        // iter().take()：不取走、不反转、不重新排序；副本见 `copy_for_peek`。
        queue.iter().take(count).map(copy_for_peek).collect()
    }

    /// 这个 topic 在队列里有没有痕迹（等待中的消息或 in-flight）。
    /// 与 [`has`](Self::has)（按消息 id）不同：这是按 topic 的「存不存在」，用于 status 的 `exists`。
    pub fn has_topic(&self, topic: &str) -> bool {
        if self.queues.get(topic).map(|q| !q.is_empty()).unwrap_or(false) {
            return true;
        }
        self.in_flight.values().any(|flight| flight.topic == topic)
    }

    /// 当前有消息（等待中或 in-flight）的 topic 列表，按字典序排序
    /// （快照与测试断言需要确定性）。
    pub fn topics(&self) -> Vec<String> {
        let mut known: Vec<String> = self
            .queues
            .iter()
            .filter(|(_, queue)| !queue.is_empty())
            .map(|(topic, _)| topic.clone())
            .collect();
        known.extend(self.in_flight.values().map(|flight| flight.topic.clone()));
        known.sort();
        known.dedup();
        known
    }

    /// 已交给消费者、尚未 ack/nack 的消息数。
    pub fn in_flight_count(&self, topic: &str) -> usize {
        self.in_flight.values().filter(|flight| flight.topic == topic).count()
    }

    /// 公开入口：把消息放回队列头部。用于订阅者处理失败后的回队。
    ///
    /// 去重：同一个 `id` 已经在该 topic 队列里、或已被某个消费者取走（in-flight）时，
    /// 不再插入副本。一个地址有多个订阅者时，每个失败的订阅者都会触发一次回队；
    /// 不去重就会在队列里留下 N 份同一消息（实测：2 个订阅者失败 → unread_count 为 2）。
    ///
    /// 返回 `true` 表示消息确实回到队列；`false` 表示因重复而跳过。
    pub fn requeue(&mut self, topic: String, message: T) -> bool {
        self.requeue_internal(topic, message)
    }

    fn requeue_internal(&mut self, topic: String, message: T) -> bool {
        let id = message.id().to_string();
        if self.in_flight.contains_key(&id) {
            return false; // 已有消费者在处理，再插一份就是重复投递
        }
        if let Some(queue) = self.queues.get(&topic) {
            if queue.iter().any(|queued| queued.id() == id) {
                return false; // 已在队列里等待，不需要第二份
            }
        }
        self.queues
            .entry(topic)
            .or_insert_with(VecDeque::new)
            .push_front(message);
        true
    }

    /// 某个消息是否已在队列中等待或已被取走（in-flight）。与 TS / Python 的 `has` 对齐。
    pub fn has(&self, message_id: &str) -> bool {
        if self.in_flight.contains_key(message_id) {
            return true;
        }
        self.queues
            .values()
            .any(|queue| queue.iter().any(|queued| queued.id() == message_id))
    }

    fn requeue_stale(&mut self, topic: &str, timeout: Duration) {
        let now = Instant::now();
        let mut stale_ids = Vec::new();

        for (id, flight) in &self.in_flight {
            if flight.topic == topic && now.duration_since(flight.timestamp) > timeout {
                stale_ids.push(id.clone());
            }
        }

        for id in stale_ids {
            if let Some(flight) = self.in_flight.remove(&id) {
                self.requeue_internal(flight.topic, flight.message);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug)]
    struct TestMessage {
        id: String,
    }

    impl Identifiable for TestMessage {
        fn id(&self) -> &str {
            &self.id
        }
    }

    fn msg(id: &str) -> TestMessage {
        TestMessage { id: id.to_string() }
    }

    #[test]
    fn resolve_ack_timeout_maps_the_three_states() {
        // 不传 ≠ 永不超时；Some(ZERO) 才是显式关闭。
        assert_eq!(resolve_ack_timeout(None), Some(DEFAULT_ACK_TIMEOUT));
        assert_eq!(resolve_ack_timeout(Some(Duration::ZERO)), None);
        assert_eq!(resolve_ack_timeout(Some(Duration::from_millis(5))), Some(Duration::from_millis(5)));
    }

    #[test]
    fn stale_in_flight_message_is_requeued_after_its_timeout() {
        let mut q = MailMessageQueue::new();
        q.enqueue("t".to_string(), msg("m1"));

        let first = q.dequeue_for_ack("t", Some(Duration::from_millis(10))).unwrap();
        assert_eq!(first.id, "m1");
        assert_eq!(q.get_status("t"), 0);
        assert_eq!(q.in_flight_count("t"), 1);

        std::thread::sleep(Duration::from_millis(25));

        let recovered = q.dequeue_for_ack("t", Some(Duration::from_millis(10))).unwrap();
        assert_eq!(recovered.id, "m1");
        assert_eq!(q.in_flight_count("t"), 1);
    }

    #[test]
    fn zero_timeout_keeps_the_message_in_flight() {
        let mut q = MailMessageQueue::new();
        q.enqueue("t".to_string(), msg("m1"));

        q.dequeue_for_ack("t", Some(Duration::ZERO));
        std::thread::sleep(Duration::from_millis(20));

        assert!(q.dequeue_for_ack("t", Some(Duration::ZERO)).is_none());
        assert_eq!(q.in_flight_count("t"), 1);
    }

    #[test]
    fn in_flight_count_is_scoped_per_topic() {
        let mut q = MailMessageQueue::new();
        q.enqueue("t1".to_string(), msg("m1"));
        q.enqueue("t2".to_string(), msg("m2"));

        q.dequeue_for_ack("t1", Some(Duration::from_secs(30)));

        assert_eq!(q.in_flight_count("t1"), 1);
        assert_eq!(q.in_flight_count("t2"), 0);
        assert_eq!(q.get_status("t1"), 0);
    }

    #[test]
    fn duplicate_requeue_is_skipped() {
        // 同一 id 重复回队只保留一份：否则 N 个订阅者各自失败会在队列里留下 N 份副本。
        let mut q = MailMessageQueue::new();
        q.enqueue("t".to_string(), msg("dup"));

        assert!(!q.requeue("t".to_string(), msg("dup")));
        assert!(!q.requeue("t".to_string(), msg("dup")));
        assert_eq!(q.get_status("t"), 1);
    }

    #[test]
    fn requeue_skips_a_message_that_is_in_flight() {
        // 回队给一个正在被 fetch 处理的消息会同时投递两份。
        let mut q = MailMessageQueue::new();
        q.enqueue("t".to_string(), msg("busy"));
        q.dequeue_for_ack("t", Some(Duration::from_secs(30)));

        assert!(!q.requeue("t".to_string(), msg("busy")));
        assert_eq!(q.get_status("t"), 0);
        assert_eq!(q.in_flight_count("t"), 1);
    }

    #[test]
    fn same_id_can_be_requeued_again_after_being_taken_out() {
        // 去重不能把「已处理完」也当成重复，否则重试永远进不了队列。
        let mut q = MailMessageQueue::new();
        q.enqueue("t".to_string(), msg("again"));

        q.dequeue_for_ack("t", Some(Duration::from_secs(30)));
        q.nack("again", true);
        assert_eq!(q.get_status("t"), 1);

        q.dequeue_for_ack("t", Some(Duration::from_secs(30)));
        q.nack("again", true);
        assert_eq!(q.get_status("t"), 1);
        assert!(q.has("again"));
    }

    #[test]
    fn peek_leaves_no_trace_and_returns_the_head_in_fifo_order() {
        let mut q = MailMessageQueue::new();
        q.enqueue("t".to_string(), msg("msg-1"));
        q.enqueue("t".to_string(), msg("msg-2"));

        let snapshot = q.peek("t", None);
        assert_eq!(snapshot.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["msg-1", "msg-2"]);

        // 关键：快照完全不留痕迹。
        assert_eq!(q.get_status("t"), 2);
        assert_eq!(q.in_flight_count("t"), 0);
        assert!(q.has("msg-1"));
        // 反复 peek 稳定，且消息仍能被正常取走。
        assert_eq!(q.peek("t", None).len(), 2);
        assert_eq!(q.dequeue("t").unwrap().id, "msg-1");
    }

    #[test]
    fn peek_of_an_empty_queue_or_unknown_topic_is_an_empty_vec() {
        let q: MailMessageQueue<TestMessage> = MailMessageQueue::new();
        assert!(q.peek("t", None).is_empty());
        assert!(q.peek("never-seen", Some(3)).is_empty());
    }

    #[test]
    fn peek_covers_the_head_only_and_does_not_consume() {
        let mut q = MailMessageQueue::new();
        for id in ["msg-1", "msg-2", "msg-3"] {
            q.enqueue("t".to_string(), msg(id));
        }

        assert!(q.peek("t", Some(0)).is_empty());
        assert_eq!(
            q.peek("t", Some(2)).iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["msg-1", "msg-2"]
        );
        assert_eq!(q.peek("t", Some(usize::MAX)).len(), 3);
        assert_eq!(q.get_status("t"), 3); // limit 不消耗消息
    }

    #[test]
    fn peek_does_not_look_at_in_flight_messages() {
        let mut q = MailMessageQueue::new();
        q.enqueue("t".to_string(), msg("msg-1"));
        q.enqueue("t".to_string(), msg("msg-2"));
        q.dequeue_for_ack("t", Some(Duration::from_secs(30)));

        assert_eq!(q.peek("t", None).iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["msg-2"]);
        assert_eq!(q.get_status("t"), 1);
        assert_eq!(q.in_flight_count("t"), 1);

        q.nack("msg-1", true);
        assert_eq!(
            q.peek("t", None).iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["msg-1", "msg-2"]
        );
    }

    #[test]
    fn repeated_peek_does_not_run_the_stale_scan() {
        // 只读操作不能触发 stale 重投：in-flight 属于那个消费者。
        let mut q = MailMessageQueue::new();
        q.enqueue("t".to_string(), msg("m1"));
        q.dequeue_for_ack("t", Some(Duration::from_millis(1)));

        std::thread::sleep(Duration::from_millis(25));
        assert!(q.peek("t", None).is_empty());
        assert_eq!(q.in_flight_count("t"), 1);

        q.ack("m1");
        assert_eq!(q.in_flight_count("t"), 0);
    }

    #[test]
    fn peek_hands_out_copies_so_the_observer_cannot_reach_the_queue() {
        // Rust 的 `Clone` 是深拷贝，所以上面那句在 Rust 端是保证，不是 best-effort。
        let mut q = MailMessageQueue::new();
        q.enqueue("t".to_string(), TestMessage { id: "m1".to_string() });

        let mut copy = q.peek("t", None).remove(0);
        copy.id = "should not leak".to_string();

        assert_eq!(q.peek("t", None)[0].id, "m1");
        assert_eq!(q.dequeue("t").unwrap().id, "m1");
    }

    #[test]
    fn has_topic_survives_the_trip_through_in_flight() {
        let mut q = MailMessageQueue::new();
        assert!(!q.has_topic("t"));

        q.enqueue("t".to_string(), msg("m1"));
        assert!(q.has_topic("t"));

        q.dequeue_for_ack("t", Some(Duration::from_secs(30)));
        assert!(q.has_topic("t")); // 只是离开队列，并未处理完

        q.ack("m1");
        assert!(!q.has_topic("t"));
    }

    #[test]
    fn topics_lists_only_topics_holding_messages_sorted() {
        let mut q = MailMessageQueue::new();
        q.enqueue("t2".to_string(), msg("m2"));
        q.enqueue("t1".to_string(), msg("m1"));
        q.enqueue("t1".to_string(), msg("m3"));
        q.enqueue("t0".to_string(), msg("m0"));
        q.dequeue("t0"); // 空队列不再算「有消息」

        assert_eq!(q.topics(), vec!["t1".to_string(), "t2".to_string()]);

        q.dequeue_for_ack("t1", Some(Duration::from_secs(30)));
        q.dequeue("t1"); // 队列扇干，但 m1 还在 in-flight
        assert_eq!(q.topics(), vec!["t1".to_string(), "t2".to_string()]); // in-flight 也算痕迹
        q.ack("m1");
        assert_eq!(q.topics(), vec!["t2".to_string()]);
    }

    #[test]
    fn ack_and_nack_update_in_flight_count() {
        let mut q = MailMessageQueue::new();
        q.enqueue("t".to_string(), msg("m1"));
        q.enqueue("t".to_string(), msg("m2"));

        let a = q.dequeue_for_ack("t", Some(Duration::from_secs(30))).unwrap();
        let b = q.dequeue_for_ack("t", Some(Duration::from_secs(30))).unwrap();
        assert_eq!(q.in_flight_count("t"), 2);

        q.ack(&a.id);
        assert_eq!(q.in_flight_count("t"), 1);

        q.nack(&b.id, true);
        assert_eq!(q.in_flight_count("t"), 0);
        assert_eq!(q.get_status("t"), 1);
        assert_eq!(q.dequeue("t").unwrap().id, "m2"); // nack(requeue) 回到队首
    }
}
