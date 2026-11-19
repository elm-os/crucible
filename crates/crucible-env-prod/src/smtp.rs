//! The production [`Mailer`] (contract 2.11): SMTP through `lettre`, which owns its sockets. One connection
//! per message, with rustls and the bundled web roots when TLS is on.

use std::fmt;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use crucible_env::{MailError, MailMessage, Mailer};
use lettre::message::Mailbox;
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::transport::smtp::extension::ClientId;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

/// How long one command of the SMTP dialogue may take.
const TIMEOUT: Duration = Duration::from_secs(10);

/// How the connection to the mail server is secured.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SmtpSecurity {
    /// Upgrade a plain connection with STARTTLS; a server without it is refused.
    StartTls,
    /// TLS from the first byte.
    Tls,
    /// No TLS: for a relay on the same host or a trusted network.
    Plain,
}

/// Where and how mail is sent.
#[derive(Clone)]
pub struct SmtpSettings {
    /// The mail server's host.
    pub host: String,
    /// The mail server's port.
    pub port: u16,
    /// How the connection is secured.
    pub security: SmtpSecurity,
    /// The user name and the password, when the server asks for them.
    pub credentials: Option<(String, String)>,
    /// The sender address.
    pub from: String,
    /// The name this client gives in `EHLO`: the domain of the sender address.
    pub hello: String,
}

impl fmt::Debug for SmtpSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SmtpSettings")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("security", &self.security)
            .field("credentials", &self.credentials.as_ref().map(|_| ".."))
            .field("from", &self.from)
            .finish_non_exhaustive()
    }
}

/// The SMTP mailer.
pub struct SmtpMailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
}

impl fmt::Debug for SmtpMailer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SmtpMailer")
            .field("from", &self.from)
            .finish_non_exhaustive()
    }
}

fn permanent(reason: impl fmt::Display) -> MailError {
    MailError {
        transient: false,
        reason: reason.to_string(),
    }
}

impl SmtpMailer {
    /// A mailer for `settings`. Nothing is connected until a message is sent.
    pub fn new(settings: &SmtpSettings) -> Result<Self, MailError> {
        let from: Mailbox = settings
            .from
            .parse()
            .map_err(|e| permanent(format!("smtp.from is not an email address: {e}")))?;
        let tls = || {
            TlsParameters::new(settings.host.clone())
                .map_err(|e| permanent(format!("the TLS settings of smtp.host: {e}")))
        };
        let mut builder =
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(settings.host.as_str())
                .port(settings.port)
                .hello_name(ClientId::Domain(settings.hello.clone()))
                .timeout(Some(TIMEOUT))
                .tls(match settings.security {
                    SmtpSecurity::StartTls => Tls::Required(tls()?),
                    SmtpSecurity::Tls => Tls::Wrapper(tls()?),
                    SmtpSecurity::Plain => Tls::None,
                });
        if let Some((user, password)) = &settings.credentials {
            builder = builder.credentials(Credentials::new(user.clone(), password.clone()));
        }
        Ok(Self {
            transport: builder.build(),
            from,
        })
    }
}

#[async_trait]
impl Mailer for SmtpMailer {
    async fn send(&self, message: MailMessage) -> Result<(), MailError> {
        let micros = u64::try_from(message.date.micros()).unwrap_or(0);
        let mut email = Message::builder()
            .from(self.from.clone())
            .subject(message.subject)
            .message_id(Some(message.message_id))
            .date(SystemTime::UNIX_EPOCH + Duration::from_micros(micros))
            .header(ContentType::TEXT_PLAIN);
        for to in &message.to {
            email = email.to(to
                .parse()
                .map_err(|e| permanent(format!("a recipient is not an email address: {e}")))?);
        }
        let email = email.body(message.text).map_err(permanent)?;
        match self.transport.send(email).await {
            Ok(_) => Ok(()),
            // A refusal the server calls permanent (5xx) is not retried as it is; everything else, a lost
            // connection included, may pass later.
            Err(error) => Err(MailError {
                transient: !error.is_permanent(),
                reason: error.to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(from: &str) -> SmtpSettings {
        SmtpSettings {
            host: "mail.example.com".to_owned(),
            port: 587,
            security: SmtpSecurity::StartTls,
            credentials: Some(("user".to_owned(), "hunter2".to_owned())),
            from: from.to_owned(),
            hello: "example.com".to_owned(),
        }
    }

    #[test]
    fn a_mailer_is_built_without_connecting_and_never_prints_the_password() {
        let settings = settings("Crucible <crucible@example.com>");
        assert!(SmtpMailer::new(&settings).is_ok());
        assert!(!format!("{settings:?}").contains("hunter2"));
        let error = SmtpMailer::new(&self::settings("not an address")).unwrap_err();
        assert!(!error.transient && error.reason.contains("smtp.from"));
    }

    #[tokio::test]
    async fn a_server_that_is_not_there_is_a_transient_failure() {
        let mut settings = settings("crucible@example.com");
        settings.host = "127.0.0.1".to_owned();
        settings.port = 1;
        settings.security = SmtpSecurity::Plain;
        let mailer = SmtpMailer::new(&settings).unwrap();
        let error = mailer
            .send(MailMessage {
                to: vec!["ada@example.com".to_owned()],
                subject: "s".to_owned(),
                text: "t".to_owned(),
                html: None,
                message_id: "<1@example.com>".to_owned(),
                date: crucible_records::Timestamp::from_micros(1_800_000_000_000_000),
            })
            .await
            .unwrap_err();
        assert!(error.transient, "{error}");
    }
}
