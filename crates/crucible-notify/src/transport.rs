//! Sending one notification to one channel, and what came of it. The production [`NetTransport`] calls Slack
//! and webhooks through the egress policy and hands email to the [`Mailer`]; the simulation has its own.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use crucible_control::{Channel, ChannelKind};
use crucible_egress::{Egress, EgressError};
use crucible_env::{Env, MailMessage, Mailer};
use crucible_records::Uuid;
use http::HeaderName;
use http::header::CONTENT_TYPE;

use crate::channel::Settings;
use crate::notification::Notification;
use crate::render::{
    DELIVERY_HEADER, EVENT_HEADER, SIGNATURE_HEADER, email, signature, slack_body, webhook_body,
};

/// What came of one attempt (contract 12.6). It never carries anything the receiver said.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Delivery {
    /// Whether the receiver accepted the notification.
    pub ok: bool,
    /// The class of the HTTP status: `2xx` to `5xx`. `None` for email and when there was no answer.
    pub status_class: Option<&'static str>,
    /// The stable reason of a failure.
    pub error_code: Option<&'static str>,
    /// How long the attempt took.
    pub latency_ms: u64,
}

impl Delivery {
    fn failed(error_code: &'static str) -> Self {
        Self {
            ok: false,
            status_class: None,
            error_code: Some(error_code),
            latency_ms: 0,
        }
    }
}

/// Sends notifications.
#[async_trait]
pub trait Transport: fmt::Debug + Send + Sync {
    /// One attempt to send `notification` to `channel`. `secret` is the channel's opened secret: the Slack
    /// URL or the webhook signing key. `delivery_id` is the same at every retry of one notification.
    async fn deliver(
        &self,
        channel: &Channel,
        secret: Option<&[u8]>,
        notification: &Notification,
        delivery_id: Uuid,
    ) -> Delivery;

    /// Whether email can be sent at all.
    fn has_mailer(&self) -> bool;
}

/// The production transport: Slack and webhooks through the egress policy, email through the mailer.
#[derive(Clone, Debug)]
pub struct NetTransport {
    env: Env,
    egress: Egress,
    mailer: Option<Arc<dyn Mailer>>,
    mail_domain: String,
    public_url: Option<String>,
}

const fn status_class(status: u16) -> &'static str {
    match status {
        100..=199 => "1xx",
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        _ => "5xx",
    }
}

impl NetTransport {
    /// A transport that calls through `egress` and mails through `mailer`. `mail_domain` is the domain of
    /// the `Message-ID`s; `public_url` is the base of the links in notifications, when the deployment has one.
    pub fn new(
        env: &Env,
        egress: Egress,
        mailer: Option<Arc<dyn Mailer>>,
        mail_domain: &str,
        public_url: Option<String>,
    ) -> Self {
        Self {
            env: env.clone(),
            egress,
            mailer,
            mail_domain: mail_domain.to_owned(),
            public_url,
        }
    }

    /// The egress policy the transport calls under.
    pub const fn egress(&self) -> &Egress {
        &self.egress
    }

    async fn post(&self, url: &str, headers: &[(HeaderName, String)], body: String) -> Delivery {
        match self.egress.post(url, headers, Bytes::from(body)).await {
            Ok(reply) => {
                let class = status_class(reply.status);
                Delivery {
                    ok: class == "2xx",
                    status_class: Some(class),
                    error_code: match class {
                        "2xx" => None,
                        // The policy follows no redirect: the receiver must be given by its final URL.
                        "3xx" => Some("redirect_not_followed"),
                        "4xx" => Some("receiver_rejected"),
                        _ => Some("receiver_failed"),
                    },
                    latency_ms: 0,
                }
            }
            Err(error) => {
                if let EgressError::Refused(refusal) = &error {
                    tracing::warn!(
                        reason = refusal.reason(),
                        "a notification target was refused: {refusal}"
                    );
                }
                Delivery::failed(error.code())
            }
        }
    }

    async fn attempt(
        &self,
        channel: &Channel,
        secret: Option<&[u8]>,
        notification: &Notification,
        delivery_id: Uuid,
    ) -> Delivery {
        let public_url = self.public_url.as_deref();
        let json = (CONTENT_TYPE, "application/json".to_owned());
        match channel.kind {
            ChannelKind::Slack => {
                let Some(url) = secret.and_then(|secret| std::str::from_utf8(secret).ok()) else {
                    return Delivery::failed("secret_missing");
                };
                self.post(url, &[json], slack_body(notification, public_url))
                    .await
            }
            ChannelKind::Webhook => {
                let (Ok(settings), Some(secret)) = (Settings::of(channel), secret) else {
                    return Delivery::failed("secret_missing");
                };
                let Some(url) = settings.url else {
                    return Delivery::failed("target_missing");
                };
                let now = self.env.clock.wall_now();
                let body = webhook_body(notification, delivery_id, now, public_url);
                let headers = [
                    json,
                    (
                        HeaderName::from_static(SIGNATURE_HEADER),
                        signature(secret, now.micros().div_euclid(1_000_000), &body),
                    ),
                    (
                        HeaderName::from_static(DELIVERY_HEADER),
                        delivery_id.to_string(),
                    ),
                    (
                        HeaderName::from_static(EVENT_HEADER),
                        format!("alert.{}", notification.kind.as_str()),
                    ),
                ];
                self.post(&url, &headers, body).await
            }
            ChannelKind::Email => {
                let (Ok(settings), Some(mailer)) = (Settings::of(channel), &self.mailer) else {
                    return Delivery::failed("smtp_not_configured");
                };
                let (subject, text) = email(notification, public_url);
                let message = MailMessage {
                    to: settings.to,
                    subject,
                    text,
                    html: None,
                    message_id: format!("<{}@{}>", self.env.new_id(), self.mail_domain),
                    date: self.env.clock.wall_now(),
                };
                match mailer.send(message).await {
                    Ok(()) => Delivery {
                        ok: true,
                        status_class: None,
                        error_code: None,
                        latency_ms: 0,
                    },
                    Err(error) => {
                        tracing::debug!(%error, transient = error.transient, "the mail server refused a message");
                        Delivery::failed(if error.transient {
                            "smtp_unavailable"
                        } else {
                            "smtp_rejected"
                        })
                    }
                }
            }
        }
    }
}

#[async_trait]
impl Transport for NetTransport {
    async fn deliver(
        &self,
        channel: &Channel,
        secret: Option<&[u8]>,
        notification: &Notification,
        delivery_id: Uuid,
    ) -> Delivery {
        let started = self.env.clock.now();
        let mut delivery = self
            .attempt(channel, secret, notification, delivery_id)
            .await;
        let took = self.env.clock.now().saturating_duration_since(started);
        delivery.latency_ms = u64::try_from(took.as_millis()).unwrap_or(u64::MAX);
        delivery
    }

    fn has_mailer(&self) -> bool {
        self.mailer.is_some()
    }
}
