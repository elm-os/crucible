//! Identity on a node with Postgres (contract 11): sessions, members and API keys from the control plane.
//!
//! `crucible serve` builds [`Identity`] over the control store, keeps the key snapshot current (the key feed and
//! the 60 s `last_used_at` writes), and, while no owner exists, prints a one-time setup link. `crucible admin
//! init` creates the first owner from the command line, and `crucible admin reset-password` prints a reset link:
//! this version sends no email.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use crucible_auth::csrf::Origins;
use crucible_auth::service::{Identity, IdentityConfig};
use crucible_auth::{DbKeys, LookupKey, OutageMode, Sealer, StoreFeed, run_feed, run_last_used};
use crucible_config::{Config, PgOutageMode};
use crucible_control::ControlStore;
use crucible_env::Env;

use crate::control::Control;
use crate::store::MasterKey;

/// How often the first-run setup link is tried while Postgres does not answer.
const SETUP_RETRY: Duration = Duration::from_secs(5);

/// Identity over the control store of `control`.
pub(crate) fn build(
    env: &Env,
    config: &Config,
    control: &Control,
) -> anyhow::Result<Arc<Identity>> {
    let master = config.master_key.as_ref().context(
        "the control plane needs the master key (master_key_file or CRUCIBLE_MASTER_KEY)",
    )?;
    let master = MasterKey::parse(master.expose())?;
    let mode = match config.auth.pg_outage_mode {
        PgOutageMode::FailOpen => OutageMode::FailOpen {
            max: config.auth.fail_open_max.map(Duration::from_secs),
        },
        PgOutageMode::FailClosed => OutageMode::FailClosed,
    };
    let lookup = LookupKey::new(master.lookup_key());
    let keys = Arc::new(DbKeys::new(env, lookup.clone(), mode));
    let public_url = config.server.public_url.clone();
    let origins = Origins::new(public_url.as_deref(), &config.server.allowed_origins)
        .map_err(anyhow::Error::msg)
        .context("server.public_url or server.allowed_origins")?;
    let mut identity = IdentityConfig::new(origins, public_url);
    identity.session_outage_grace = Duration::from_secs(config.auth.session_outage_grace);
    identity.hash_permits = hash_permits();
    let identity = Identity::new(
        env,
        Arc::new(control.store.clone()),
        Sealer::new("master-0", master.seal_key()),
        lookup,
        keys,
        identity,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(Arc::new(identity))
}

/// min(cores, 4) concurrent password hashes (contract 11.4).
#[allow(clippy::disallowed_methods)] // The binary reads the host's core count.
fn hash_permits() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZero::get)
        .unwrap_or(1)
        .min(4)
}

/// Starts the key feed, the `last_used_at` writes and the first-run setup link.
pub(crate) fn start(env: &Env, identity: &Arc<Identity>, store: Arc<dyn ControlStore>) {
    let keys = Arc::clone(identity.keys());
    env.spawn(
        "key-feed",
        run_feed(
            env.clone(),
            Arc::clone(&keys),
            Arc::new(StoreFeed(Arc::clone(&store))),
        ),
    );
    env.spawn("key-last-used", run_last_used(env.clone(), keys, store));
    let identity = Arc::clone(identity);
    let clock = Arc::clone(&env.clock);
    env.spawn("first-run-setup", async move {
        loop {
            match identity.issue_setup_token().await {
                Ok(Some(link)) => {
                    let base = identity
                        .config()
                        .public_url
                        .clone()
                        .unwrap_or_default();
                    tracing::warn!(
                        "no owner exists yet: open {}/setup/?token={} to create the organization and its owner \
                         (the link works once and for 24 hours; a restart makes a new one), or run `crucible \
                         admin init`",
                        base.trim_end_matches('/'),
                        link.token.as_str()
                    );
                    return;
                }
                Ok(None) => return,
                Err(error) => {
                    tracing::debug!(%error, "checking for a first owner; retrying");
                    clock.sleep(SETUP_RETRY).await;
                }
            }
        }
    });
}

/// `crucible admin init`: the organization and its first owner.
pub(crate) async fn admin_init(
    config: &Config,
    org_name: &str,
    email: &str,
    name: &str,
    password: &str,
) -> anyhow::Result<()> {
    let (_prod, identity) = admin_identity(config)?;
    identity
        .init_owner(org_name, email, name, password)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    tracing::info!(
        email,
        "the organization and its owner are created; sign in at /login/"
    );
    Ok(())
}

/// `crucible admin reset-password`: a one-time reset link for the user with `email`, valid for 30 minutes.
pub(crate) async fn admin_reset_password(config: &Config, email: &str) -> anyhow::Result<String> {
    let (_prod, identity) = admin_identity(config)?;
    let link = identity
        .issue_reset(email)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let base = config.server.public_url.clone().unwrap_or_default();
    Ok(format!(
        "{}/reset/?token={}",
        base.trim_end_matches('/'),
        link.token.as_str()
    ))
}

fn admin_identity(config: &Config) -> anyhow::Result<(crucible_env_prod::Prod, Arc<Identity>)> {
    let postgres = config.postgres.as_ref().context(
        "this needs the control plane: set postgres.url (--postgres-url, CRUCIBLE_POSTGRES_URL)",
    )?;
    let prod = crucible_env_prod::build(
        tokio::runtime::Handle::current(),
        &crucible_env_prod::ProdConfig::default(),
    )
    .context("building the production environment")?;
    let control = Control::new(&prod.env, postgres)?;
    let identity = build(&prod.env, config, &control)?;
    Ok((prod, identity))
}
