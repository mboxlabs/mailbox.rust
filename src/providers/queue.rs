use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use crate::message::Identifiable;

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
        if let Some(timeout) = ack_timeout {
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

    pub fn get_status(&self, topic: &str) -> usize {
        self.queues.get(topic).map(|q| q.len()).unwrap_or(0)
    }

    fn requeue_internal(&mut self, topic: String, message: T) {
        self.queues
            .entry(topic)
            .or_insert_with(VecDeque::new)
            .push_front(message);
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
