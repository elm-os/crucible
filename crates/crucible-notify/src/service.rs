//! Channel management (contract 12.6): what the API's channel routes do.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crucible_auth::Sealer;
use crucible_control::{Channel, ChannelKind, ControlStore, JsonDoc, TxError};
use crucible_egress::EgressPolicy;
use crucible_env::{Env, Instant};
use crucible_records::{ChannelId, ProjectId, Uuid};
use serde_json::json;

use crate::channel::{Settings, Target, TargetError, check_name, check_target, stored};
use crate::notification::{NoticeKind, Notification};
use crate::outbox::{SecretError, open, seal};
use crate::transport::{Delivery, Transport};

/// Why a channel request failed.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum ChannelError {
    /// No such channel in the project.
    #[error("no such channel")]
    NotFound,
    /// The channel changed since the version the caller holds.
    #[error("the channel changed since it was read")]
    VersionMismatch,
    /// The request is not allowed.
    #[error(transparent)]
    Target(#[from] TargetError),
    /// The channel's secret cannot be read.
    #[error(transparent)]
    Secret(SecretError),
    /// The channel was tested [`TESTS_PER_MINUTE`] times in the last minute.
    #[error("a channel can be tested 6 times a minute; try again in {retry_after_secs} s")]
    RateLimited {
        /// Seconds until a test is accepted again.
        retry_after_secs: u32,
    },
    /// The store failed.
    #[error(transparent)]
    Store(#[from] TxError),
}

/// Test-sends one channel accepts per minute on a node (contract 11.9).
pub const TESTS_PER_MINUTE: usize = 6;

/// When each channel was last tested on this node.
type Tests = Arc<Mutex<BTreeMap<ChannelId, VecDeque<Instant>>>>;

impl From<SecretError> for ChannelError {
    fn from(error: SecretError) -> Self {
        match error {
            SecretError::Store(error) => Self::Store(error),
            unreadable @ SecretError::Unreadable => Self::Secret(unreadable),
        }
    }
}

/// A channel as it is shown: the row and its settings, without its secret.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ChannelView {
    /// The row.
    pub channel: Channel,
    /// The URL of a webhook channel.
    pub url: Option<String>,
    /// The Slack URL, shortened to its host and last characters.
    pub url_hint: Option<String>,
    /// The recipients of an email channel.
    pub to: Vec<String>,
}

/// A saved channel, with the signing secret when the save made one. The secret is shown this once.
#[derive(Clone, PartialEq, Eq)]
pub struct Saved {
    /// The channel.
    pub view: ChannelView,
    /// The new signing secret of a webhook channel.
    pub secret: Option<String>,
}

impl std::fmt::Debug for Saved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Saved")
            .field("view", &self.view)
            .field("secret", &self.secret.as_ref().map(|_| ".."))
            .finish()
    }
}

/// What a change to a channel may set.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ChannelPatch {
    /// A new name.
    pub name: Option<String>,
    /// Turns the channel on or off. Turning it on clears the failure count and the reason it was turned off.
    pub enabled: Option<bool>,
    /// A new target of the channel's own kind.
    pub target: Option<Target>,
    /// Replaces the signing secret of a webhook channel; the new one is returned once.
    pub rotate_secret: bool,
}

/// Channel management over the control store.
#[derive(Clone, Debug)]
pub struct Channels {
    env: Env,
    store: Arc<dyn ControlStore>,
    sealer: Arc<Sealer>,
    transport: Arc<dyn Transport>,
    policy: EgressPolicy,
    tests: Tests,
}

fn view(channel: Channel) -> Result<ChannelView, ChannelError> {
    let settings = Settings::of(&channel)?;
    Ok(ChannelView {
        url: settings.url,
        url_hint: settings.hint,
        to: settings.to,
        channel,
    })
}

impl Channels {
    /// Channel management that checks targets against `policy` and test-sends through `transport`.
    pub fn new(
        env: &Env,
        store: Arc<dyn ControlStore>,
        sealer: Arc<Sealer>,
        transport: Arc<dyn Transport>,
        policy: EgressPolicy,
    ) -> Self {
        Self {
            env: env.clone(),
            store,
            sealer,
            transport,
            policy,
            tests: Tests::default(),
        }
    }

    /// Counts a test of `id` now, or says how long to wait.
    fn admit_test(&self, id: ChannelId) -> Result<(), ChannelError> {
        const MINUTE: Duration = Duration::from_secs(60);
        let now = self.env.clock.now();
        let mut tests = self.tests.lock().unwrap_or_else(PoisonError::into_inner);
        tests.retain(|_, times| {
            while times
                .front()
                .is_some_and(|at| now.saturating_duration_since(*at) >= MINUTE)
            {
                times.pop_front();
            }
            !times.is_empty()
        });
        let times = tests.entry(id).or_default();
        if times.len() >= TESTS_PER_MINUTE {
            let waited = times
                .front()
                .map_or(MINUTE, |at| now.saturating_duration_since(*at));
            let left = MINUTE.saturating_sub(waited).as_secs().max(1);
            return Err(ChannelError::RateLimited {
                retry_after_secs: u32::try_from(left).unwrap_or(60),
            });
        }
        times.push_back(now);
        Ok(())
    }

    async fn find(&self, project_id: ProjectId, id: ChannelId) -> Result<Channel, ChannelError> {
        match self.store.channels().get(id).await? {
            Some(channel) if channel.project_id == project_id => Ok(channel),
            _ => Err(ChannelError::NotFound),
        }
    }

    /// The channels of a project.
    pub async fn list(&self, project_id: ProjectId) -> Result<Vec<ChannelView>, ChannelError> {
        self.store
            .channels()
            .list(project_id)
            .await?
            .into_iter()
            .map(view)
            .collect()
    }

    /// One channel.
    pub async fn get(
        &self,
        project_id: ProjectId,
        id: ChannelId,
    ) -> Result<ChannelView, ChannelError> {
        view(self.find(project_id, id).await?)
    }

    /// A new signing secret: `whsec_` and 32 random bytes in hex.
    fn signing_secret(&self) -> String {
        let mut bytes = [0u8; 32];
        self.env.rng.fill_bytes(&mut bytes);
        let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        format!("whsec_{hex}")
    }

    /// Creates a channel. A webhook channel gets a signing secret, which is returned this once.
    pub async fn create(
        &self,
        project_id: ProjectId,
        name: &str,
        target: Target,
    ) -> Result<Saved, ChannelError> {
        let name = check_name(name)?;
        let target = check_target(target, &self.policy, self.transport.has_mailer())?;
        let (config, own_secret) = stored(&target);
        let shown = (target.kind() == ChannelKind::Webhook).then(|| self.signing_secret());
        let secret = own_secret.or_else(|| shown.clone()).map(|plaintext| {
            seal(
                &self.sealer,
                self.env.rng.as_ref(),
                self.env.new_id(),
                project_id,
                plaintext.as_bytes(),
            )
        });
        let channel = Channel {
            id: ChannelId::from_uuid(self.env.new_id()),
            project_id,
            name,
            kind: target.kind(),
            config,
            secret_id: secret.as_ref().map(|secret| secret.id),
            enabled: true,
            disabled_reason: None,
            failures: 0,
            last_test: None,
            version: 1,
        };
        let tx = self.store.begin().await?;
        if let Some(secret) = &secret {
            tx.secrets().insert(secret).await?;
        }
        tx.channels().insert(&channel).await?;
        tx.commit().await?;
        Ok(Saved {
            view: view(channel)?,
            secret: shown,
        })
    }

    /// Changes a channel whose stored version is `expected`.
    pub async fn update(
        &self,
        project_id: ProjectId,
        id: ChannelId,
        expected: u64,
        patch: ChannelPatch,
    ) -> Result<Saved, ChannelError> {
        let mut channel = self.find(project_id, id).await?;
        if channel.version != expected {
            return Err(ChannelError::VersionMismatch);
        }
        if let Some(name) = &patch.name {
            channel.name = check_name(name)?;
        }
        match patch.enabled {
            Some(true) if !channel.enabled => {
                channel.enabled = true;
                channel.failures = 0;
                channel.disabled_reason = None;
            }
            Some(false) => {
                channel.enabled = false;
                channel.disabled_reason = None;
            }
            _ => {}
        }
        let old_secret = channel.secret_id;
        let mut plaintext = None;
        let mut shown = None;
        if let Some(target) = patch.target {
            if target.kind() != channel.kind {
                return Err(TargetError::Invalid {
                    field: "kind",
                    message:
                        "the kind of a channel cannot change; create a channel of the other kind"
                            .to_owned(),
                }
                .into());
            }
            let target = check_target(target, &self.policy, self.transport.has_mailer())?;
            let (config, own_secret) = stored(&target);
            channel.config = config;
            plaintext = own_secret;
        }
        if patch.rotate_secret {
            if channel.kind != ChannelKind::Webhook {
                return Err(TargetError::Invalid {
                    field: "rotate_secret",
                    message: "only a webhook channel has a signing secret".to_owned(),
                }
                .into());
            }
            shown = Some(self.signing_secret());
            plaintext.clone_from(&shown);
        }
        let secret = plaintext.map(|plaintext| {
            seal(
                &self.sealer,
                self.env.rng.as_ref(),
                self.env.new_id(),
                project_id,
                plaintext.as_bytes(),
            )
        });
        if let Some(secret) = &secret {
            channel.secret_id = Some(secret.id);
        }
        let tx = self.store.begin().await?;
        if let Some(secret) = &secret {
            tx.secrets().insert(secret).await?;
            if let Some(old) = old_secret {
                tx.secrets().delete(old).await?;
            }
        }
        if !tx.channels().update_if_version(&channel, expected).await? {
            tx.rollback().await;
            return Err(ChannelError::VersionMismatch);
        }
        tx.commit().await?;
        channel.version = expected + 1;
        Ok(Saved {
            view: view(channel)?,
            secret: shown,
        })
    }

    /// Deletes a channel whose stored version is `expected`, with its secret. Notifications still in the
    /// outbox for it are dropped when they come due.
    pub async fn delete(
        &self,
        project_id: ProjectId,
        id: ChannelId,
        expected: u64,
    ) -> Result<(), ChannelError> {
        let channel = self.find(project_id, id).await?;
        let tx = self.store.begin().await?;
        if !tx.channels().delete_if_version(id, expected).await? {
            tx.rollback().await;
            return Err(ChannelError::VersionMismatch);
        }
        if let Some(secret) = channel.secret_id {
            tx.secrets().delete(secret).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// One synchronous test notification (contract 12.6). The outcome is stored on the channel and returned;
    /// it says whether the receiver accepted the test and never what the receiver answered. A test does not
    /// count against the channel, and works on a channel that is turned off, so a fix can be checked before
    /// the channel is turned on again.
    pub async fn test(
        &self,
        project_id: ProjectId,
        id: ChannelId,
    ) -> Result<Delivery, ChannelError> {
        let channel = self.find(project_id, id).await?;
        self.admit_test(id)?;
        let secret = open(&self.sealer, self.store.as_ref(), &channel).await?;
        let now = self.env.clock.wall_now();
        let notification = Notification {
            channel_id: id.uuid(),
            project_id: project_id.uuid(),
            rule_id: Uuid::default(),
            rule_name: format!("Test of the channel \"{}\"", channel.name),
            kind: NoticeKind::Test,
            series: Vec::new(),
            measure: String::new(),
            aggregation: String::new(),
            comparison: String::new(),
            threshold: String::new(),
            value: None,
            window_secs: 0,
            at_micros: now.micros(),
            eval_seq: 0,
        };
        let delivery = self
            .transport
            .deliver(
                &channel,
                secret.as_deref(),
                &notification,
                self.env.new_id(),
            )
            .await;
        let outcome = json!({
            "ok": delivery.ok,
            "status_class": delivery.status_class,
            "latency_ms": delivery.latency_ms,
            "error_code": delivery.error_code,
            "at": now.to_string(),
        });
        let outcome = JsonDoc::parse(&outcome.to_string())?;
        self.store.channels().set_last_test(id, &outcome).await?;
        Ok(delivery)
    }
}
