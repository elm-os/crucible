//! Identity on a node with Postgres (contract 11): sessions, members and API keys from the control plane.
//!
//! `crucible serve` builds [`Identity`] over the control store, keeps the key snapshot current (the key feed and
//! the 60 s `last_used_at` writes), makes the sign-in providers of the configuration match their rows, and, while
//! no owner exists, prints a one-time setup link. `crucible admin init` creates the first owner from the command
//! line, `crucible admin reset-password` prints a reset link (the server sends no email for it), and `crucible
//! admin enable-password-login` turns password sign-in back on when no provider signs anyone in.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use crucible_auth::csrf::Origins;
use crucible_auth::service::{AuthError, Identity, IdentityConfig, ProviderInput, Seeded};
use crucible_auth::{DbKeys, LookupKey, OutageMode, Sealer, StoreFeed, run_feed, run_last_used};
use crucible_config::{Config, PgOutageMode, SignInKind, SignInProvider};
use crucible_control::{ControlStore, KeyScope, ProviderKind};
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
    identity.require_https = config.auth.require_https;
    if !identity.origins.public_is_https() {
        tracing::info!(
            "sign-in works over plain HTTP and the session cookie is not Secure; set server.public_url to the \
             server's https:// address to use Secure cookies, and security.require_https = true to refuse plain HTTP"
        );
    }
    identity.hash_permits = hash_permits();
    identity.providers = config.auth.providers.iter().map(provider_input).collect();
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

/// A sign-in provider of the configuration, as identity seeds it.
fn provider_input(provider: &SignInProvider) -> ProviderInput {
    let (kind, button) = match provider.kind {
        SignInKind::Github => (ProviderKind::Github, "Continue with GitHub"),
        SignInKind::Google => (ProviderKind::Google, "Continue with Google"),
        SignInKind::Oidc => (ProviderKind::Oidc, "Continue with single sign-on"),
    };
    ProviderInput {
        kind,
        display_name: provider
            .display_name
            .clone()
            .unwrap_or_else(|| button.to_owned()),
        issuer: provider.issuer.clone(),
        client_id: provider.client_id.clone(),
        client_secret: provider.client_secret.expose().to_owned(),
        scopes: provider.scopes.clone(),
        allowed_domains: provider.allowed_domains.clone(),
    }
}

/// min(cores, 4) concurrent password hashes (contract 11.4).
#[allow(clippy::disallowed_methods)] // The binary reads the host's core count.
fn hash_permits() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZero::get)
        .unwrap_or(1)
        .min(4)
}

/// Starts the key feed, the `last_used_at` writes and the first-run setup link, after the Postgres migrations
/// when `migrate` asks for them. Each retries until Postgres answers, so a node starts while Postgres is down,
/// and each stops at the shutdown signal.
pub(crate) fn start(env: &Env, identity: &Arc<Identity>, control: &Control, migrate: bool) {
    let store: Arc<dyn ControlStore> = Arc::new(control.store.clone());
    if !migrate {
        start_tasks(env, identity, store);
        return;
    }
    let (env2, identity, control) = (env.clone(), Arc::clone(identity), control.clone());
    env.spawn("postgres-migrate", async move {
        let mut shutdown = env2.shutdown();
        loop {
            // The migrations run in one transaction, which a dropped connection rolls back.
            let migrated = tokio::select! {
                biased;
                () = &mut shutdown => return,
                migrated = control.migrate_postgres(&env2) => migrated,
            };
            let Err(error) = migrated else { break };
            tracing::warn!(
                error = format!("{error:#}"),
                "the Postgres migrations did not run; retrying"
            );
            tokio::select! {
                biased;
                () = &mut shutdown => return,
                () = env2.clock.sleep(SETUP_RETRY) => {}
            }
        }
        start_tasks(&env2, &identity, store);
    });
}

/// The key feed, the `last_used_at` writes and the first-run setup link, over `store`.
pub(crate) fn start_tasks(env: &Env, identity: &Arc<Identity>, store: Arc<dyn ControlStore>) {
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
    start_seeding(env, identity);
    let identity = Arc::clone(identity);
    let setup_env = env.clone();
    env.spawn("first-run-setup", async move {
        let mut shutdown = setup_env.shutdown();
        loop {
            let issued = tokio::select! {
                biased;
                () = &mut shutdown => return,
                issued = identity.issue_setup_token() => issued,
            };
            match issued {
                Ok(Some(link)) => {
                    // Without a public URL the link is the server's own address, for example
                    // http://127.0.0.1:8080, followed by this path.
                    let base = identity
                        .config()
                        .public_url
                        .clone()
                        .unwrap_or_else(|| "<this server's address>".to_owned());
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
                    tokio::select! {
                        biased;
                        () = &mut shutdown => return,
                        () = setup_env.clock.sleep(SETUP_RETRY) => {}
                    }
                }
            }
        }
    });
}

/// Makes the rows of the configuration's sign-in providers match it, retrying until Postgres answers, then says so
/// when nobody can sign in, or when a provider the configuration no longer declares stays on as the last way in,
/// with the command that turns password sign-in back on.
fn start_seeding(env: &Env, identity: &Arc<Identity>) {
    let (seed_env, identity) = (env.clone(), Arc::clone(identity));
    env.spawn("sign-in-providers", async move {
        let mut shutdown = seed_env.shutdown();
        loop {
            let seeded = tokio::select! {
                biased;
                () = &mut shutdown => return,
                seeded = identity.seed_providers() => seeded,
            };
            match seeded {
                Ok(changed) => {
                    let mut kept = false;
                    for (kind, change) in changed {
                        if change == Seeded::KeptOn {
                            kept = true;
                            tracing::error!(
                                "the configuration no longer declares [auth_{kind}], but password sign-in is off and \
                                 no other sign-in provider is offered, so its row stays on. This server cannot sign \
                                 anyone in through it, because its client secret was in that section: put the section \
                                 back and restart, or turn password sign-in on (`crucible admin \
                                 enable-password-login`, or Settings > Organization) and restart, which turns the row \
                                 off"
                            );
                        } else {
                            tracing::info!(provider = %kind, ?change, "a sign-in provider of the configuration");
                        }
                    }
                    if !kept && let Ok(true) = identity.nobody_can_sign_in().await {
                        tracing::warn!(
                            "nobody can sign in: password sign-in is off and the sign-in page offers no provider. Run \
                             `crucible admin enable-password-login` to turn password sign-in back on"
                        );
                    }
                    return;
                }
                Err(AuthError::Invalid { field, message }) => {
                    tracing::error!(
                        "a sign-in provider of the configuration is not added: {field}: {message}; correct its \
                         [auth_github], [auth_google] or [auth_oidc] settings and restart"
                    );
                    return;
                }
                Err(error) => {
                    tracing::debug!(%error, "making the configuration's sign-in providers match; retrying");
                    tokio::select! {
                        biased;
                        () = &mut shutdown => return,
                        () = seed_env.clock.sleep(SETUP_RETRY) => {}
                    }
                }
            }
        }
    });
}

/// `crucible admin enable-password-login`: turns password sign-in back on, for a deployment whose sign-in providers
/// no longer sign anyone in. Answers what to tell the operator.
pub(crate) async fn admin_enable_password_login(config: &Config) -> anyhow::Result<&'static str> {
    let (_prod, identity) = admin_identity(config)?;
    match identity.enable_password_login().await {
        Ok(true) => Ok(
            "password sign-in is on: sign in at /login/ with your email and password. Someone who joined through a \
             sign-in provider has no password yet: `crucible admin reset-password <email>` prints a link that sets one",
        ),
        Ok(false) => Ok("password sign-in is already on; nothing changed"),
        Err(AuthError::NotFound) => Err(anyhow::anyhow!(
            "no organization exists yet, and password sign-in is on until one does: create it with the setup link \
             that `crucible serve` prints, or with `crucible admin init`"
        )),
        Err(other) => Err(anyhow::anyhow!("{other}")),
    }
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

/// `crucible admin create-key`: a key for the project with this slug or id, printed as TOML lines (`project_id`,
/// `public_key`, `secret_key`). The secret key is shown once.
pub(crate) async fn admin_create_key(
    config: &Config,
    project: &str,
    scopes: &[String],
    note: Option<String>,
    import: bool,
) -> anyhow::Result<String> {
    let scopes = if scopes.is_empty() {
        None
    } else {
        let parsed = scopes
            .iter()
            .map(|scope| {
                KeyScope::parse(scope.trim()).with_context(|| {
                    format!("unknown scope {scope:?}; use ingest, read, write, delete or sql")
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Some(parsed)
    };
    let (_prod, identity) = admin_identity(config)?;
    let found = identity
        .project_by_reference(project)
        .await
        .map_err(|e| match e {
            AuthError::NotFound => anyhow::anyhow!(
                "no project has the slug or id {project:?}; the first project's slug is `default`, and Settings lists \
                 the others"
            ),
            other => anyhow::anyhow!("{other}"),
        })?;
    let created = identity
        .create_key(found.id, scopes, note, None, import)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(format!(
        "project_id = \"{}\"\npublic_key = \"{}\"\nsecret_key = \"{}\"\n",
        found.id, created.key.public_key, created.secret
    ))
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

#[cfg(test)]
mod tests {
    use crucible_config::Secret;

    use super::*;

    #[test]
    fn a_configured_provider_without_button_text_gets_its_kind_s() {
        let configured = |kind, display_name: Option<&str>| SignInProvider {
            kind,
            display_name: display_name.map(str::to_owned),
            issuer: (kind == SignInKind::Oidc).then(|| "https://login.example.com".to_owned()),
            client_id: "client".to_owned(),
            client_secret: Secret::new("secret"),
            scopes: vec!["groups".to_owned()],
            allowed_domains: vec!["example.com".to_owned()],
        };
        let buttons: Vec<_> = [SignInKind::Github, SignInKind::Google, SignInKind::Oidc]
            .into_iter()
            .map(|kind| provider_input(&configured(kind, None)).display_name)
            .collect();
        assert_eq!(
            buttons,
            [
                "Continue with GitHub",
                "Continue with Google",
                "Continue with single sign-on"
            ]
        );
        let input = provider_input(&configured(SignInKind::Oidc, Some("Continue with Acme")));
        assert_eq!(
            (
                input.kind,
                input.display_name.as_str(),
                input.issuer.as_deref(),
                input.client_secret.as_str(),
                input.scopes.as_slice(),
                input.allowed_domains.as_slice()
            ),
            (
                ProviderKind::Oidc,
                "Continue with Acme",
                Some("https://login.example.com"),
                "secret",
                ["groups".to_owned()].as_slice(),
                ["example.com".to_owned()].as_slice()
            )
        );
    }
}
