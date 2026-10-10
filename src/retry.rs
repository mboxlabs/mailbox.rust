//! 订阅重试的共享策略与记账工具。
//!
//! 这里只放「策略数学」——纯函数、可单测；拦截订阅失败的位置仍在各 provider 的
//! 订阅包装回调里（TS / Python 在基类，Rust 没有基类，见 `providers::memory`）。
//!
//! 三端（TS / Python / Rust）保持同一套语义：可重试判定（`MailboxError::is_retriable`）、
//! 退避公式、尝试计数方式都一致。

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// 订阅失败后的重试策略。
///
/// 注意：重试是 **opt-in** 的。默认（`None`）保持「失败只回队、不自动重投」的行为，
/// 因为 AMQP / SQS 这类传输层自带重投与死信，库再重投就是双重投递。
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// 总尝试次数，含首次。必须 >= 1；1 等价于不重试。
    pub max_attempts: u32,
    /// 首次重试前的等待时间。
    pub base_delay: Duration,
    /// 退避倍数，默认 2.0。
    pub factor: f64,
    /// 单次等待上限，默认 30 秒。
    pub max_delay: Duration,
}

impl RetryPolicy {
    pub fn new(max_attempts: u32, base_delay: Duration) -> Self {
        Self {
            max_attempts,
            base_delay,
            factor: 2.0,
            max_delay: Duration::from_secs(30),
        }
    }

    /// 归一化：夹住非法值，避免退避算出 0 次尝试或负等待。
    pub fn resolved(&self) -> Self {
        Self {
            max_attempts: self.max_attempts.max(1),
            base_delay: self.base_delay,
            factor: if self.factor < 1.0 { 1.0 } else { self.factor },
            max_delay: self.max_delay,
        }
    }
}

/// 第 `attempt` 次尝试失败之后，下一次重试要等多久。
///
/// `attempt` 是已经发生的尝试次数（首次失败即 1），因此第一次退避就是 `base_delay`。
/// 指数退避并夹在 `max_delay` 之内。
pub fn next_retry_delay(policy: &RetryPolicy, attempt: u32) -> Duration {
    let resolved = policy.resolved();
    let safe_attempt = attempt.max(1);
    let micros = resolved.base_delay.as_micros() as f64
        * resolved.factor.powi(safe_attempt as i32 - 1);
    let delay = Duration::from_micros(micros.max(0.0) as u64);
    if delay > resolved.max_delay {
        resolved.max_delay
    } else {
        delay
    }
}

/// 按「订阅者 + 消息」维度记录已经尝试过几次。
///
/// 键必须包含订阅者：订阅是广播，某个订阅者失败不代表其他订阅者也失败，重试次数不能混在一起算。
/// 条目带 TTL，避免长期运行的进程里台账无限增长。
pub struct AttemptLedger {
    entries: HashMap<(String, String), (u32, Instant)>,
    ttl: Duration,
}

impl AttemptLedger {
    pub fn new(ttl: Duration) -> Self {
        Self { entries: HashMap::new(), ttl }
    }

    /// 记录一次尝试，返回这是第几次。
    pub fn record(&mut self, subscription_id: &str, message_id: &str) -> u32 {
        self.gc();
        let key = (subscription_id.to_string(), message_id.to_string());
        let attempts = self.entries.get(&key).map(|(n, _)| *n).unwrap_or(0) + 1;
        self.entries.insert(key, (attempts, Instant::now()));
        attempts
    }

    /// 当前已尝试次数（未记录过则为 0）。
    pub fn count(&self, subscription_id: &str, message_id: &str) -> u32 {
        self.entries
            .get(&(subscription_id.to_string(), message_id.to_string()))
            .map(|(n, _)| *n)
            .unwrap_or(0)
    }

    /// 处理成功后清零，让同 id 的消息日后还能重试。
    pub fn reset(&mut self, subscription_id: &str, message_id: &str) {
        self.entries.remove(&(subscription_id.to_string(), message_id.to_string()));
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn gc(&mut self) {
        let now = Instant::now();
        self.entries.retain(|_, (_, updated_at)| now.duration_since(*updated_at) <= self.ttl);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_retry_delay_is_exponential_and_capped() {
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(10),
            factor: 2.0,
            max_delay: Duration::from_millis(50),
        };
        assert_eq!(next_retry_delay(&policy, 1), Duration::from_millis(10));
        assert_eq!(next_retry_delay(&policy, 2), Duration::from_millis(20));
        assert_eq!(next_retry_delay(&policy, 3), Duration::from_millis(40));
        // 夹在 max_delay 之内，不能无限增长
        assert_eq!(next_retry_delay(&policy, 4), Duration::from_millis(50));
    }

    #[test]
    fn resolved_policy_clamps_invalid_values() {
        let policy = RetryPolicy {
            max_attempts: 0,
            base_delay: Duration::from_millis(5),
            factor: 0.5,
            max_delay: Duration::from_secs(1),
        };
        let resolved = policy.resolved();
        assert_eq!(resolved.max_attempts, 1);
        assert!(resolved.factor >= 1.0);
    }

    #[test]
    fn ledger_counts_per_subscriber() {
        // 订阅是广播：重试次数必须按「订阅者 + 消息」算，不能混在一起
        let mut ledger = AttemptLedger::new(Duration::from_secs(300));
        assert_eq!(ledger.record("sub-1", "msg-1"), 1);
        assert_eq!(ledger.record("sub-1", "msg-1"), 2);
        assert_eq!(ledger.record("sub-2", "msg-1"), 1);
        ledger.reset("sub-1", "msg-1");
        assert_eq!(ledger.count("sub-1", "msg-1"), 0);
        assert_eq!(ledger.count("sub-2", "msg-1"), 1);
    }
}
