//! Alerts on a node with Postgres (contracts 11.7 and 12.6): the alert and channel routes of the API, the
//! scheduler and the consumer of evaluations, and the consumer of the notification outbox.
//!
//! - Notifications leave through the egress policy of `egress.mode` and `egress.allow_cidrs`; email goes to
//!   the mail server of `smtp.*`, and without `smtp.host` email channels cannot be created.
//! - Evaluations read through the query service, which exists only once the telemetry store is up. Until
//!   then a read fails, the row is retried, and no state changes.

use std::sync::{Arc, OnceLock};

use anyhow::Context as _;
use async_trait::async_trait;
use crucible_alerts::{
    Alerts, Evaluator, Observed, QuerySource, ReadFailure, RuleSpec, SeriesSource,
};
use crucible_api::AlertsApi;
use crucible_auth::Sealer;
use crucible_config::{Config, SmtpTls};
use crucible_control::{ControlStore, Project};
use crucible_egress::{Cidr, Egress, EgressPolicy, Mode};
use crucible_env::{Env, Mailer, TlsRoots};
use crucible_env_prod::{SmtpMailer, SmtpSecurity, SmtpSettings};
use crucible_jobs::Job;
use crucible_notify::{Channels, NetTransport, Notifier, Transport, outbox_job};
use crucible_query::QueryService;
use crucible_records::TimeWindow;

use crate::control::Control;
use crate::store::MasterKey;

/// The read of evaluations and previews: the query service, once the telemetry store is up.
#[derive(Debug, Default)]
pub(crate) struct LateSource(OnceLock<QuerySource>);

impl LateSource {
    /// The telemetry store is up: evaluations read through `service` from now on.
    pub(crate) fn open(&self, service: QueryService) {
        // Set once per process; a second call would carry the same service.
        drop(self.0.set(QuerySource::new(service)));
    }
}

#[async_trait]
impl SeriesSource for LateSource {
    async fn read(
        &self,
        project: &Project,
        spec: &RuleSpec,
        window: TimeWindow,
    ) -> Result<Observed, ReadFailure> {
        match self.0.get() {
            Some(source) => source.read(project, spec, window).await,
            None => Err(ReadFailure(
                "the telemetry store is not up; set clickhouse.url (CRUCIBLE_CLICKHOUSE_URL), or wait for its \
                 startup checks"
                    .to_owned(),
            )),
        }
    }
}

/// Alerting on this node.
#[derive(Debug)]
pub(crate) struct Alerting {
    /// What the API serves.
    pub(crate) api: AlertsApi,
    /// Opened when the telemetry store is up.
    pub(crate) source: Arc<LateSource>,
    /// The scheduler, the evaluations consumer and the outbox consumer.
    pub(crate) jobs: Vec<Job>,
}

/// The host of a URL such as `http://clickhouse:8123` or `postgres://user@db:5432/crucible`.
fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?']).next()?;
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = match host_port.strip_prefix('[') {
        Some(v6) => v6.split(']').next()?,
        None => host_port.rsplit_once(':').map_or(host_port, |(h, _)| h),
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// The hosts of the object store that Crucible itself reaches: each endpoint's host, and with virtual-host URLs
/// the bucket's host under it, which is where those requests go.
fn object_store_hosts(config: &Config) -> Vec<String> {
    let Some(store) = &config.object_store else {
        return Vec::new();
    };
    let mut hosts = Vec::new();
    for endpoint in std::iter::once(&store.endpoint).chain(&store.public_endpoint) {
        if let Some(host) = host_of(endpoint) {
            if store.virtual_host {
                hosts.push(format!("{}.{host}", store.bucket.to_ascii_lowercase()));
            }
            hosts.push(host);
        }
    }
    hosts
}

/// The egress policy of the configuration. Strict mode also refuses the deployment's own databases and object
/// store.
fn policy(config: &Config) -> anyhow::Result<EgressPolicy> {
    let allow_cidrs = config
        .egress
        .allow_cidrs
        .iter()
        .map(|cidr| {
            cidr.parse::<Cidr>()
                .map_err(|e| anyhow::anyhow!("egress.allow_cidrs entry {cidr}: {e}"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let infrastructure = [
        config.clickhouse.as_ref().map(|c| c.url.as_str()),
        config.postgres.as_ref().map(|p| p.url.expose()),
    ]
    .into_iter()
    .flatten()
    .filter_map(host_of)
    .chain(object_store_hosts(config))
    .collect();
    Ok(EgressPolicy {
        mode: if config.egress.strict {
            Mode::Strict
        } else {
            Mode::Open
        },
        allow_cidrs,
        infrastructure,
    })
}

/// The mailer of `smtp.*`, and the domain of its `Message-ID`s.
fn mailer(config: &Config) -> anyhow::Result<(Option<Arc<dyn Mailer>>, String)> {
    let Some(smtp) = &config.smtp else {
        return Ok((None, "crucible.invalid".to_owned()));
    };
    let from = smtp.from.trim().trim_end_matches('>');
    let domain = from
        .rsplit_once('@')
        .map_or("crucible.invalid", |(_, d)| d)
        .to_owned();
    let settings = SmtpSettings {
        host: smtp.host.clone(),
        port: smtp.port,
        security: match smtp.tls {
            SmtpTls::StartTls => SmtpSecurity::StartTls,
            SmtpTls::Tls => SmtpSecurity::Tls,
            SmtpTls::None => SmtpSecurity::Plain,
        },
        credentials: smtp.username.clone().map(|user| {
            let password = smtp.password.as_ref().map(|p| p.expose().to_owned());
            (user, password.unwrap_or_default())
        }),
        from: smtp.from.clone(),
        hello: domain.clone(),
    };
    let mailer = SmtpMailer::new(&settings)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("the mail server settings (smtp.*)")?;
    Ok((Some(Arc::new(mailer)), domain))
}

/// Alerting over the control store of `control`.
pub(crate) fn build(env: &Env, config: &Config, control: &Control) -> anyhow::Result<Alerting> {
    let master = config.master_key.as_ref().context(
        "the control plane needs the master key (master_key_file or CRUCIBLE_MASTER_KEY)",
    )?;
    let master = MasterKey::parse(master.expose())?;
    let sealer = Arc::new(Sealer::new("master-0", master.seal_key()));
    let policy = policy(config)?;
    if policy.mode == Mode::Strict {
        tracing::info!(
            allow_cidrs = config.egress.allow_cidrs.len(),
            "egress.mode = strict: notifications go to public https targets and egress.allow_cidrs only"
        );
    }
    let (mailer, mail_domain) = mailer(config)?;
    let store: Arc<dyn ControlStore> = Arc::new(control.store.clone());
    let transport: Arc<dyn Transport> = Arc::new(NetTransport::new(
        env,
        Egress::new(env, policy.clone(), TlsRoots::WebPki),
        mailer,
        &mail_domain,
        config.server.public_url.clone(),
    ));
    let source = Arc::new(LateSource::default());
    let series: Arc<dyn SeriesSource> = Arc::clone(&source) as Arc<dyn SeriesSource>;
    let api = AlertsApi {
        alerts: Alerts::new(env, Arc::clone(&store), Some(Arc::clone(&series))),
        channels: Channels::new(
            env,
            store,
            Arc::clone(&sealer),
            Arc::clone(&transport),
            policy,
        ),
    };
    let mut jobs = Vec::from(crucible_alerts::jobs(Evaluator::new(series)));
    jobs.push(outbox_job(Notifier::new(transport, sealer)));
    Ok(Alerting { api, source, jobs })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_of_a_store_url_is_found_in_every_form() {
        assert_eq!(
            host_of("http://ClickHouse:8123").as_deref(),
            Some("clickhouse")
        );
        assert_eq!(
            host_of("postgres://crucible:pw@db.internal:5432/crucible?sslmode=require").as_deref(),
            Some("db.internal")
        );
        assert_eq!(
            host_of("https://[fd12::7]:8443/x").as_deref(),
            Some("fd12::7")
        );
        assert_eq!(host_of("http://10.0.0.4").as_deref(), Some("10.0.0.4"));
        assert_eq!(host_of("http://"), None);
    }

    /// The configuration of a deployment with Postgres and an object store, in egress `mode` and URL `style`.
    fn with_object_store(mode: &str, style: &str) -> Config {
        let text = format!(
            "[egress]\nmode = \"{mode}\"\n\n[postgres]\nurl = \"postgres://crucible@db.internal:5432/crucible\"\n\n\
             [object_store]\nendpoint = \"http://versitygw:7070\"\npublic_endpoint = \"https://Media.example.com:7443\"\n\
             bucket = \"crucible-media\"\naccess_key = \"key\"\nsecret_key = \"secret\"\nurl_style = \"{style}\"\n"
        );
        let env = [("CRUCIBLE_MASTER_KEY".to_owned(), "7f".repeat(32))];
        let layers = crucible_config::Layers {
            file: Some(("crucible.toml", &text)),
            env: &env,
            flags: &[],
        };
        let no_files = |_: &std::path::Path| Err(std::io::Error::other("no files"));
        crucible_config::resolve(&layers, &no_files).unwrap().config
    }

    /// Strict egress refuses the object store's endpoints, internal and public, as it refuses the databases, so a
    /// channel cannot target them; with virtual-host URLs the bucket's own host is refused too. The default mode
    /// is unchanged and lets a channel reach the same hosts.
    #[test]
    fn strict_egress_refuses_the_object_store() {
        let strict = policy(&with_object_store("strict", "path")).unwrap();
        for host in ["db.internal", "versitygw", "media.example.com"] {
            assert!(
                strict.infrastructure.iter().any(|own| own == host),
                "{host}: {:?}",
                strict.infrastructure
            );
        }
        for url in [
            "https://versitygw:7070/hook",
            "https://media.example.com:7443/hook",
            "https://MEDIA.example.com/other",
        ] {
            let refusal = strict.check_url(url).unwrap_err();
            assert_eq!(refusal.reason(), "own_infrastructure", "{url}");
            assert!(refusal.message().contains("egress.mode = open"), "{url}");
            assert!(refusal.message().contains("object store"), "{url}");
        }
        let hosted = policy(&with_object_store("strict", "virtual_host")).unwrap();
        for host in [
            "crucible-media.versitygw",
            "crucible-media.media.example.com",
        ] {
            assert!(
                hosted.infrastructure.iter().any(|own| own == host),
                "{host}: {:?}",
                hosted.infrastructure
            );
        }
        let open = policy(&with_object_store("open", "path")).unwrap();
        assert!(
            open.check_url("https://media.example.com:7443/hook")
                .is_ok()
        );
        assert!(open.check_url("http://versitygw:7070/hook").is_ok());
    }
}
