//! 投递（delivery）身份的共享记账工具。issue#2 的修复核心。
//!
//! `subscribe` 是广播：一条消息 = N 次投递。失败处理若只有「消息」身份（`nack(message)`），
//! 部分失败时就会把已成功者处理过的整条消息退回队列 —— 消息级和投递级两层语义不一致。
//!
//! 这里定义投递级的结算结果，以及「广播结算后要不要回队」的唯一判定。
//! 三端（TS / Python / Rust）保持同一语义（见 TS 的 `src/lib/delivery.ts`）。
//!
//! 时间单位与 Rust 其余 API 一致，用 [`std::time::Duration`]（TS 端用毫秒，Python 用秒）。

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// 单次投递**尝试**的默认结算超时。
///
/// 卡死的订阅回调会让广播的结算 barrier 永不落定：消息既不回队也不死信，`unread_count`
/// 里也看不见（等于静默消失）。看门狗给「一次尝试」一个上界：超时后按取消结算，消息回到
/// 兜底路径。它只给单次尝试计时，所以不会掐断健康的重试链或退避等待。
///
/// `Duration::ZERO` 关闭（调用方自己负责长任务）。
pub const DEFAULT_DELIVERY_SETTLE_TIMEOUT: Duration = Duration::from_secs(30);

/// 投递身份的分隔符，避免 id 拼接歧义（与 TS / Python 的 `\u0000` 一致）。
pub const DELIVERY_KEY_SEPARATOR: char = '\0';

/// 投递失败分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FailureReason {
    /// 可重试错误，但重试预算已用尽（或被取消）—— 值得让 `fetch()` 兜底。
    Retriable,
    /// 不可重试 —— 按死信处理，不回队（避免无限重试同一条坏消息）。
    Fatal,
    /// 待重投被取消（退订 / 关闭）—— 等同 `Retriable` 兜底。
    Cancelled,
}

/// 一次投递的最终结算结果。
///
/// 一次投递 = 某个订阅者对某条消息的一次完整处理，**包含整条重试链**：
/// 只有当回调成功、或彻底失败（预算耗尽 / 不可重试 / 被取消）时才结算。
/// 广播的发送侧据此判断「有没有订阅者成功」。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryResult {
    /// 投递身份：`subscription_id + message_id`，与 [`crate::retry::AttemptLedger`] 同一粒度。
    pub delivery_id: String,
    pub subscription_id: String,
    pub message_id: String,
    /// `true` = 回调成功结算；`false` = 这次投递最终失败。
    pub ok: bool,
    /// 失败分类，仅在 `ok == false` 时有意义。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<FailureReason>,
}

impl DeliveryResult {
    pub fn succeeded(subscription_id: &str, message_id: &str) -> Self {
        Self {
            delivery_id: delivery_key(subscription_id, message_id),
            subscription_id: subscription_id.to_string(),
            message_id: message_id.to_string(),
            ok: true,
            reason: None,
        }
    }

    pub fn failed(subscription_id: &str, message_id: &str, reason: FailureReason) -> Self {
        Self {
            delivery_id: delivery_key(subscription_id, message_id),
            subscription_id: subscription_id.to_string(),
            message_id: message_id.to_string(),
            ok: false,
            reason: Some(reason),
        }
    }
}

/// 投递身份。分隔符用 `\0`，避免 id 拼接歧义。
pub fn delivery_key(subscription_id: &str, message_id: &str) -> String {
    format!(
        "{}{}{}",
        subscription_id, DELIVERY_KEY_SEPARATOR, message_id
    )
}

/// 从投递身份里取出 subscription_id。
pub fn subscription_id_of_delivery(delivery_key: &str) -> String {
    match delivery_key.split_once(DELIVERY_KEY_SEPARATOR) {
        Some((subscription_id, _)) => subscription_id.to_string(),
        None => delivery_key.to_string(),
    }
}

/// 从投递身份里取出 message_id。
pub fn message_id_of_delivery(delivery_key: &str) -> String {
    match delivery_key.split_once(DELIVERY_KEY_SEPARATOR) {
        Some((_, message_id)) => message_id.to_string(),
        None => String::new(),
    }
}

/// 把广播监听器的结算值归一化成 `DeliveryResult`。
///
/// `settled` 与 TS 的 `Promise.allSettled` / Python 的 `gather(return_exceptions=True)` 对应：
/// `Err(())` 表示这次投递的结算通道在落定前就断了（监听器被丢弃、provider 提前退出）。
/// 走 [`crate::core::ProviderCore`] 的投递一定返回正常的 `DeliveryResult`；第三方 provider
/// 自己接的监听器可能没有结算 —— 断掉的结算按可重试失败处理（宁可多兜一次，
/// [`crate::providers::queue::MailMessageQueue::requeue`] 的去重守卫会挡住重复）。
pub fn to_delivery_results(settled: Vec<Result<DeliveryResult, ()>>) -> Vec<DeliveryResult> {
    settled
        .into_iter()
        .enumerate()
        .map(|(index, settled)| match settled {
            Ok(result) => normalize(result),
            Err(()) => DeliveryResult {
                delivery_id: format!("listener{}{}", DELIVERY_KEY_SEPARATOR, index),
                subscription_id: String::new(),
                message_id: String::new(),
                ok: false,
                reason: Some(FailureReason::Retriable),
            },
        })
        .collect()
}

/// 结算结果自洽：成功不带 reason，失败必须带。
fn normalize(result: DeliveryResult) -> DeliveryResult {
    DeliveryResult {
        reason: if result.ok {
            None
        } else {
            result.reason.or(Some(FailureReason::Retriable))
        },
        ..result
    }
}

/// 广播结算后，是否把整条消息退回队列。
///
/// 判定只看投递，不看「这个 topic 还有没有活跃订阅者」—— 后者会把全部订阅者都失败的情况
/// 变成静默丢失。正确条件：
///
/// 1. **没有任何投递成功** —— 有成功者时整条回队就是跨投递模式重复投递；
/// 2. **至少一次投递值得重试**（`Retriable` / `Cancelled`）—— 全是 `Fatal` 就按死信丢弃。
pub fn should_requeue_after_broadcast(results: &[DeliveryResult]) -> bool {
    if results.is_empty() {
        return false;
    }
    if results.iter().any(|result| result.ok) {
        return false; // 已有订阅者处理过，回队 = 副作用再执行一次
    }
    results
        .iter()
        .any(|result| result.reason != Some(FailureReason::Fatal))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delivery(reason: Option<FailureReason>, ok: bool) -> DeliveryResult {
        DeliveryResult {
            delivery_id: delivery_key("sub1", "msg1"),
            subscription_id: "sub1".to_string(),
            message_id: "msg1".to_string(),
            ok,
            reason,
        }
    }

    #[test]
    fn delivery_key_splits_back_into_its_two_halves() {
        // 投递身份 = 订阅者 + 消息；不同订阅者对同一条消息是两个身份
        let key = delivery_key("sub1", "msg1");
        assert_eq!(subscription_id_of_delivery(&key), "sub1");
        assert_eq!(message_id_of_delivery(&key), "msg1");
        assert_ne!(delivery_key("a", "m"), delivery_key("b", "m"));
    }

    #[test]
    fn only_a_broadcast_nobody_succeeded_is_requeued() {
        // 没有投递 → 不回队
        assert!(!should_requeue_after_broadcast(&[]));

        // 有投递成功 → 不回队（部分失败不重复投递）
        assert!(!should_requeue_after_broadcast(&[
            delivery(None, true),
            delivery(Some(FailureReason::Retriable), false),
        ]));

        // 全部失败且至少一次可重试 → 回队（fetch 兜底）
        assert!(should_requeue_after_broadcast(&[
            delivery(Some(FailureReason::Retriable), false),
            delivery(Some(FailureReason::Fatal), false),
        ]));

        // 取消的重投等同可重试
        assert!(should_requeue_after_broadcast(&[delivery(
            Some(FailureReason::Cancelled),
            false
        )]));

        // 全部不可重试 → 死信，不回队
        assert!(!should_requeue_after_broadcast(&[
            delivery(Some(FailureReason::Fatal), false),
            delivery(Some(FailureReason::Fatal), false),
        ]));
    }

    #[test]
    fn a_broken_settlement_channel_counts_as_retriable() {
        // 宁可多兜一次：结算通道断了 ≠ 消息可以消失
        let results = to_delivery_results(vec![
            Ok(DeliveryResult::failed("s", "m", FailureReason::Fatal)),
            Err(()),
        ]);
        assert_eq!(results[0].reason, Some(FailureReason::Fatal));
        assert!(!results[1].ok);
        assert_eq!(results[1].reason, Some(FailureReason::Retriable));
        assert!(should_requeue_after_broadcast(&results));
    }

    #[test]
    fn a_success_never_carries_a_reason() {
        let result = to_delivery_results(vec![Ok(DeliveryResult {
            delivery_id: "x".to_string(),
            subscription_id: "s".to_string(),
            message_id: "m".to_string(),
            ok: true,
            reason: Some(FailureReason::Fatal), // 自相矛盾的输入要被归一化掉
        })])
        .remove(0);
        assert!(result.ok);
        assert_eq!(result.reason, None);
    }

    #[test]
    fn the_default_settle_timeout_exists_and_is_bounded() {
        assert!(!DEFAULT_DELIVERY_SETTLE_TIMEOUT.is_zero());
    }
}
