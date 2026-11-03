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
pub use message::{
    FetchOptions, MailMessage, MailboxAddressSnapshot, MailboxSnapshot, MailboxStatus, OutgoingMail,
};
pub use provider::{
    reject_peek_in_fetch, AckableMessage, MailboxProvider, Subscription, SubscriptionStatus,
    ADDRESS_ENUMERATION_CAPABILITY, PEEK_CAPABILITY,
};
pub use utils::{
    assert_lossless_mailbox_address, copy_for_peek, get_canonical_mailbox_address_identifier,
    normalize_peek_limit, parse_mailbox_address,
};
pub use mailbox::Mailbox;
pub use core::{
    DeliveryListener, ProviderCore, ProviderCoreOptions, DEFAULT_ATTEMPT_LEDGER_TTL,
};
pub use delivery::{
    delivery_key, message_id_of_delivery, should_requeue_after_broadcast, subscription_id_of_delivery,
    to_delivery_results, DeliveryResult, FailureReason, DEFAULT_DELIVERY_SETTLE_TIMEOUT,
};
pub use retry::{AttemptLedger, RetryPolicy, next_retry_delay};
