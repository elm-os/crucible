//! The mail server of the deployment (contract 2.11). Production speaks SMTP through the adapter in
//! `crucible-env-prod`, which owns its sockets; the simulation models an outbox with seeded failures.

use std::fmt;

use async_trait::async_trait;
use crucible_records::Timestamp;

/// An email to send.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MailMessage {
    /// The recipients.
    pub to: Vec<String>,
    /// The subject.
    pub subject: String,
    /// The plain-text body.
    pub text: String,
    /// The HTML body, when there is one.
    pub html: Option<String>,
    /// The `Message-ID`, from `Rng`.
    pub message_id: String,
    /// The `Date`, from `Clock`.
    pub date: Timestamp,
}

/// Why an email was not accepted by the mail server.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
#[error("{reason}")]
pub struct MailError {
    /// Whether sending again later can succeed.
    pub transient: bool,
    /// What failed, for the log. Never shown to users.
    pub reason: String,
}

/// Sends email.
#[async_trait]
pub trait Mailer: fmt::Debug + Send + Sync {
    /// Hands one message to the mail server.
    async fn send(&self, message: MailMessage) -> Result<(), MailError>;
}
