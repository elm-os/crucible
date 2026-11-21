//! The consumer of the notification outbox (contract 11.7): one claimed row is one notification for one
//! channel.
//!
//! - A delivered notification completes its row and clears the channel's failure count.
//! - A failed attempt counts against the channel and gives the row back; the runner retries it after 5 s,
//!   doubling to 1 h.
//! - After [`DISABLE_AFTER`] failures in a row the channel is turned off with the reason, in one statement.
//!   Rows for a channel that is off, or gone, complete without a send: the project's admins see the reason on
//!   the channel and turn it on again once the receiver is fixed.
//! - Rows for a channel of a deleted project complete without a send (contract 13.4).
//!
//! A row is delivered at least once. Every attempt of a row carries the row's id as its delivery id, so a
//! receiver can drop a repeat.

use std::sync::Arc;
use std::time::Duration;

use crucible_auth::Sealer;
use crucible_control::{Channel, ControlStore, Queue, Secret, TxError};
use crucible_jobs::{Cadence, ClaimCtx, ClaimPolicy, Job, JobError, JobResult};
use crucible_records::{ChannelId, ProjectId, Uuid};

use crate::notification::Notification;
use crate::transport::Transport;

/// Failures in a row after which a channel is turned off.
pub const DISABLE_AFTER: u32 = 50;
/// How often a node looks for due rows.
pub const POLL_EVERY: Duration = Duration::from_secs(5);
/// How the outbox is claimed: a 60 s claim, which covers the 5 s connect and 10 s response limits, and as
/// many attempts as a channel has failures before it is turned off, so a row is never parked while its
/// channel is still on.
pub const OUTBOX_POLICY: ClaimPolicy = ClaimPolicy {
    lease_for: Duration::from_secs(60),
    max_attempts: DISABLE_AFTER,
};

/// The associated data that binds a channel's sealed secret to its row and project.
fn aad(secret_id: Uuid, project_id: ProjectId) -> Vec<u8> {
    format!("{secret_id}\0channel\0{project_id}").into_bytes()
}

/// Seals a channel's secret for the `secrets` table.
pub(crate) fn seal(
    sealer: &Sealer,
    rng: &dyn crucible_env::Rng,
    secret_id: Uuid,
    project_id: ProjectId,
    plaintext: &[u8],
) -> Secret {
    let sealed = sealer.seal(rng, &aad(secret_id, project_id), plaintext);
    Secret {
        id: secret_id,
        scope: project_id.to_string(),
        kind: "channel".to_owned(),
        key_id: sealed.key_id,
        nonce: sealed.nonce,
        ciphertext: sealed.ciphertext,
    }
}

/// Why a channel's secret could not be read.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum SecretError {
    /// The store failed.
    #[error(transparent)]
    Store(#[from] TxError),
    /// The row is gone or does not open under this master key.
    #[error(
        "the channel's secret does not open under this master key; restore the master key it was saved under, \
         or save the channel's URL or secret again"
    )]
    Unreadable,
}

/// The opened secret of a channel, when it has one.
pub(crate) async fn open(
    sealer: &Sealer,
    store: &dyn ControlStore,
    channel: &Channel,
) -> Result<Option<Vec<u8>>, SecretError> {
    let Some(secret_id) = channel.secret_id else {
        return Ok(None);
    };
    let secret = store
        .secrets()
        .get(secret_id)
        .await?
        .ok_or(SecretError::Unreadable)?;
    let sealed = crucible_auth::Sealed {
        key_id: secret.key_id,
        nonce: secret.nonce,
        ciphertext: secret.ciphertext,
    };
    sealer
        .open(&sealed, &aad(secret_id, channel.project_id))
        .map(Some)
        .map_err(|_| SecretError::Unreadable)
}

/// What the outbox consumer works with.
#[derive(Clone, Debug)]
pub struct Notifier {
    transport: Arc<dyn Transport>,
    sealer: Arc<Sealer>,
}

impl Notifier {
    /// A consumer that sends through `transport` and opens channel secrets with `sealer`.
    pub fn new(transport: Arc<dyn Transport>, sealer: Arc<Sealer>) -> Self {
        Self { transport, sealer }
    }

    /// Handles one claimed row.
    pub async fn handle(&self, ctx: ClaimCtx) -> JobResult {
        let Some(notification) = Notification::from_payload(&ctx.claim.payload) else {
            tracing::warn!(id = %ctx.claim.id, "an outbox row does not hold a notification; it is dropped");
            return Ok(());
        };
        let channel_id = ChannelId::from_uuid(notification.channel_id);
        let Some(channel) = ctx.store.channels().get(channel_id).await? else {
            tracing::debug!(id = %ctx.claim.id, %channel_id, "the channel of a notification is gone");
            return Ok(());
        };
        if !channel.enabled {
            tracing::debug!(
                id = %ctx.claim.id, %channel_id,
                "the channel of a notification is turned off; the notification is not sent"
            );
            return Ok(());
        }
        // A deleted project sends nothing from the moment it is deleted; its channels go with its purge.
        let live = ctx
            .store
            .projects()
            .get(channel.project_id)
            .await?
            .is_some_and(|project| project.deleted_at.is_none());
        if !live {
            tracing::debug!(
                id = %ctx.claim.id, %channel_id,
                "the project of a notification's channel is deleted; the notification is not sent"
            );
            return Ok(());
        }
        let code = match open(&self.sealer, ctx.store.as_ref(), &channel).await {
            Ok(secret) => {
                let delivery = self
                    .transport
                    .deliver(&channel, secret.as_deref(), &notification, ctx.claim.id)
                    .await;
                if delivery.ok {
                    // A count that stays is harmless: the next delivery clears it.
                    if let Err(error) = ctx.store.channels().record_success(channel_id).await {
                        tracing::debug!(%channel_id, %error, "clearing the failure count failed");
                    }
                    return Ok(());
                }
                delivery.error_code.unwrap_or("failed")
            }
            Err(SecretError::Store(error)) => return Err(error.into()),
            Err(SecretError::Unreadable) => "secret_unreadable",
        };
        let reason = format!(
            "{DISABLE_AFTER} deliveries in a row failed; the last error was `{code}`. Fix the receiver, send a test, \
             then turn the channel on again"
        );
        match ctx
            .store
            .channels()
            .record_failure(channel_id, &reason, DISABLE_AFTER)
            .await
        {
            Ok(true) => tracing::warn!(
                %channel_id, code,
                "a notification channel was turned off after {DISABLE_AFTER} failed deliveries in a row"
            ),
            Ok(false) => {}
            Err(error) => tracing::debug!(%channel_id, %error, "counting the failure failed"),
        }
        Err(JobError::Failed(format!("the delivery failed: {code}")))
    }
}

/// The outbox consumer as a job of the registry (docs/architecture/adding-a-job.md).
pub fn outbox_job(notifier: Notifier) -> Job {
    Job::claimed(
        "notification-outbox",
        Queue::NotificationOutbox,
        Cadence::every(POLL_EVERY),
        OUTBOX_POLICY,
        move |ctx| {
            let notifier = notifier.clone();
            async move { notifier.handle(ctx).await }
        },
    )
}
