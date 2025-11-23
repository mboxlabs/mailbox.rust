pub mod error;
pub mod message;
pub mod provider;
pub mod mailbox;
pub mod utils;
pub mod providers;

pub use error::MailboxError;
pub use message::{MailMessage, OutgoingMail, MailboxStatus, FetchOptions};
pub use provider::{MailboxProvider, Subscription, AckableMessage};
pub use mailbox::Mailbox;
