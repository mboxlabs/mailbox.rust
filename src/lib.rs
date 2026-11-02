pub mod error;
pub mod message;
pub mod provider;
pub mod mailbox;
pub mod core;
pub mod delivery;
pub mod retry;
pub mod utils;
pub mod providers;

pub use error::MailboxError;
pub use message::{MailMessage, OutgoingMail, MailboxStatus, FetchOptions};
pub use provider::{AckableMessage, MailboxProvider, Subscription, SubscriptionStatus};
pub use mailbox::Mailbox;
pub use core::{
    DeliveryListener, ProviderCore, ProviderCoreOptions, DEFAULT_ATTEMPT_LEDGER_TTL,
};
pub use delivery::{
    delivery_key, message_id_of_delivery, should_requeue_after_broadcast, subscription_id_of_delivery,
    to_delivery_results, DeliveryResult, FailureReason, DEFAULT_DELIVERY_SETTLE_TIMEOUT,
};
pub use retry::{AttemptLedger, RetryPolicy, next_retry_delay};
