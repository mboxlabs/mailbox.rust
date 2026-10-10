use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use crate::message::Identifiable;

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
    /// in-flight 的消息未 ack 时会回到队列，用 [`in_flight_count`] 单独观测。
    pub fn get_status(&self, topic: &str) -> usize {
        self.queues.get(topic).map(|q| q.len()).unwrap_or(0)
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
