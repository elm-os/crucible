//! Notification channels and their delivery (contracts 2.11, 11.7 and 12.6).
//!
//! - [`Notification`] is what an outbox row says; the alert evaluation writes it in the transaction of the
//!   state change.
//! - [`outbox_job`] is the consumer of the outbox: at-least-once delivery with the runner's backoff of 5 s
//!   doubling to 1 h, and a channel that is turned off, with its reason, after [`DISABLE_AFTER`] failures in a
//!   row.
//! - [`Transport`] sends one notification to one channel. [`NetTransport`] is the production one: Slack
//!   incoming webhooks pinned to their host, webhooks signed with `X-Crucible-Signature`, both through the
//!   egress policy of `crucible-egress`, and email through the [`Mailer`].
//! - [`Channels`] is channel management for the API: saving a channel checks its target, the signing secret
//!   is shown once, and a test-send reports an outcome and never the receiver's words.

mod channel;
mod notification;
mod outbox;
mod render;
mod service;
mod transport;

pub use channel::{
    MAX_RECIPIENTS, NAME_MAX_CHARS, SLACK_HOST, Settings, Target, TargetError, URL_MAX_BYTES,
};
pub use notification::{NoticeKind, Notification, SeriesLabel};
pub use outbox::{DISABLE_AFTER, Notifier, OUTBOX_POLICY, POLL_EVERY, SecretError, outbox_job};
pub use render::{
    DELIVERY_HEADER, EVENT_HEADER, SIGNATURE_HEADER, signature, summary, verify_signature,
};
pub use service::{ChannelError, ChannelPatch, ChannelView, Channels, Saved};
pub use transport::{Delivery, MailError, MailMessage, Mailer, NetTransport, Transport};
