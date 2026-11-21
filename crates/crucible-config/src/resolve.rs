//! Layering and validation: file, then environment, then flags.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use crate::settings::{SETTINGS, Setting, find, nearest};
use crate::types::{
    Auth, ClickHouse, Config, ConfigErrors, Data, Egress, Mcp, ObjectStore, PgOutageMode, Postgres,
    Secret, Server, SignInKind, SignInProvider, Smtp, SmtpTls, Source, Sql, WalBackend,
};

/// The three layers, lowest first.
#[derive(Clone, Copy, Debug)]
pub struct Layers<'a> {
    /// The configuration file: its name for messages, and its text.
    pub file: Option<(&'a str, &'a str)>,
    /// Environment variables as `(name, value)`. Names outside `CRUCIBLE_*` are ignored, and an empty value
    /// counts as unset.
    pub env: &'a [(String, String)],
    /// Command-line flags as `(dotted key, value)`.
    pub flags: &'a [(&'a str, String)],
}

/// A configuration that loaded.
#[derive(Clone, Debug)]
pub struct Loaded {
    /// The typed configuration.
    pub config: Config,
    /// What looks like a mistake but does not stop the server, for example an unknown `CRUCIBLE_*` variable.
    pub warnings: Vec<String>,
    values: Values,
    /// The secrets that were read from their `_file` twin.
    from_file: BTreeSet<&'static str>,
}

type Values = BTreeMap<&'static str, (String, Source)>;

/// The settings whose TOML value is a number.
const NUMBERS: &[&str] = &[
    "recent.max_bytes",
    "cpu.permits",
    "mcp.calls_per_second",
    "mcp.burst",
    "sql.statements_per_hour",
    "clickhouse.idle_timeout",
    "postgres.pool_size",
    "auth.fail_open_max",
    "auth.session_outage_grace",
    "smtp.port",
    "media.max_bytes",
];

impl Loaded {
    /// Where the value of `key` came from; `None` when the setting is unset.
    pub fn source(&self, key: &str) -> Option<&Source> {
        self.values.get(key).map(|(_, source)| source)
    }

    /// The effective configuration as TOML, each value with its source. Secrets are redacted.
    pub fn print(&self) -> String {
        let mut out = String::new();
        let mut section = "";
        for setting in SETTINGS {
            let Some((value, source)) = self.values.get(setting.key) else {
                continue;
            };
            if self.from_file.contains(setting.key) {
                // The `_file` twin is printed instead.
                continue;
            }
            if setting.section() != section {
                section = setting.section();
                if !out.is_empty() {
                    out.push('\n');
                }
                let _ = writeln!(out, "[{section}]");
            }
            let shown = if setting.secret {
                "\"<redacted>\"".to_owned()
            } else if setting.key == "postgres.url" {
                quoted(&without_password(value))
            } else if (NUMBERS.contains(&setting.key) && value.parse::<u64>().is_ok())
                || (setting.key == "security.require_https" && value.parse::<bool>().is_ok())
            {
                value.clone()
            } else {
                quoted(value)
            };
            let _ = writeln!(out, "{} = {shown}  # {source}", setting.name());
        }
        out
    }
}

/// A TOML basic string.
fn quoted(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04X}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A connection URL with the password of its user-info part replaced.
fn without_password(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return "<redacted>".to_owned();
    };
    // The user-info part ends at the last `@` before the path.
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    match authority.rsplit_once('@') {
        Some((userinfo, host)) => match userinfo.split_once(':') {
            Some((user, _)) => format!("{scheme}://{user}:<redacted>@{host}{tail}"),
            None => url.to_owned(),
        },
        None => url.to_owned(),
    }
}

fn sections() -> Vec<&'static str> {
    let mut sections: Vec<_> = SETTINGS
        .iter()
        .map(Setting::section)
        .filter(|section| !section.is_empty())
        .collect();
    sections.dedup();
    sections
}

/// Why `key` is not a setting, and what the nearest setting is. Never quotes a value.
fn unknown_key(key: &str, file: &str) -> String {
    let (section, name) = key.split_once('.').unwrap_or(("", key));
    if !section.is_empty() && !sections().contains(&section) {
        let hint = nearest(section, sections().into_iter())
            .map(|known| format!(" (did you mean [{known}]?)"))
            .unwrap_or_default();
        return format!(
            "{file}: unknown section [{section}]{hint}. The sections are [{}].",
            sections().join("], [")
        );
    }
    if !section.is_empty() && find(name).is_some() {
        return format!(
            "{file}: `{name}` is a top-level key, not a key of [{section}]: move it above the first [section] line"
        );
    }
    if let Some(known) = nearest(key, SETTINGS.iter().map(|setting| setting.key)) {
        return format!("{file}: unknown key `{key}`; did you mean `{known}`?");
    }
    let keys: Vec<_> = SETTINGS
        .iter()
        .filter(|setting| setting.section() == section)
        .map(Setting::name)
        .collect();
    let place = if section.is_empty() {
        "the top-level keys".to_owned()
    } else {
        format!("the keys of [{section}]")
    };
    format!(
        "{file}: unknown key `{key}`; {place} are {}",
        keys.join(", ")
    )
}

fn read_file_layer(name: &str, text: &str, values: &mut Values, errors: &mut Vec<String>) {
    let table = match toml::from_str::<toml::Table>(text) {
        Ok(table) => table,
        Err(error) => {
            // The parser's own rendering quotes the offending line, which may hold a secret.
            let line = error
                .span()
                .map(|span| text.get(..span.start).unwrap_or("").matches('\n').count() + 1);
            let place = line.map(|line| format!(" line {line}")).unwrap_or_default();
            errors.push(format!("{name}{place}: {}", error.message()));
            return;
        }
    };
    let mut leaf = |key: String, value: &toml::Value| {
        let Some(setting) = find(&key) else {
            errors.push(unknown_key(&key, name));
            return;
        };
        let text = match value {
            toml::Value::String(text) => text.clone(),
            toml::Value::Integer(number) => number.to_string(),
            toml::Value::Boolean(flag) => flag.to_string(),
            _ => {
                errors.push(format!(
                    "{name}: `{key}` must be a string, a whole number or true or false"
                ));
                return;
            }
        };
        values.insert(setting.key, (text, Source::File));
    };
    for (key, value) in &table {
        match value {
            toml::Value::Table(section) => {
                for (name, value) in section {
                    leaf(format!("{key}.{name}"), value);
                }
            }
            value => leaf(key.clone(), value),
        }
    }
}

/// Replaces each secret by the content of its `_file` twin where the twin is set in the same layer, in a
/// higher one, or alone.
fn read_secret_files(
    values: &mut Values,
    read_file: &dyn Fn(&Path) -> io::Result<String>,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> BTreeSet<&'static str> {
    let mut from_file = BTreeSet::new();
    for setting in SETTINGS.iter().filter(|setting| setting.secret) {
        let twin = format!("{}_file", setting.key);
        let Some((path, file_source)) = values.get(twin.as_str()).cloned() else {
            continue;
        };
        if let Some((_, value_source)) = values.get(setting.key) {
            if value_source.rank() > file_source.rank() {
                continue;
            }
            if value_source.rank() == file_source.rank() {
                warnings.push(format!(
                    "both `{}` and `{twin}` are set ({value_source} and {file_source}); the file is used. \
                     Set only one",
                    setting.key
                ));
            }
        }
        match read_file(Path::new(&path)) {
            Ok(text) => {
                let text = text.trim_end_matches(['\r', '\n']).to_owned();
                values.insert(setting.key, (text, file_source));
                from_file.insert(setting.key);
            }
            Err(error) => errors.push(format!(
                "cannot read `{twin}` {path} ({file_source}): {error}"
            )),
        }
    }
    from_file
}

struct Typed<'a> {
    values: &'a Values,
    errors: &'a mut Vec<String>,
}

impl<'a> Typed<'a> {
    fn text(&self, key: &str) -> Option<&'a str> {
        let values: &'a Values = self.values;
        values.get(key).map(|(value, _)| value.as_str())
    }

    /// The parsed value of a setting that is not a secret; a value that does not parse is an error that
    /// says what is expected and where the value came from.
    fn parsed<T: FromStr>(&mut self, key: &str, expected: &str) -> Option<T> {
        let (value, source) = self.values.get(key)?;
        let parsed = value.parse().ok();
        if parsed.is_none() {
            self.errors.push(format!(
                "`{key}` = {} is not {expected} ({source})",
                quoted(value)
            ));
        }
        parsed
    }

    fn secret(&self, key: &str) -> Option<Secret> {
        self.text(key).map(Secret::new)
    }
}

fn typed(values: &Values, errors: &mut Vec<String>, warnings: &mut Vec<String>) -> Option<Config> {
    let mut t = Typed { values, errors };
    let address = "an address such as 0.0.0.0:8080";
    let listen = t.parsed("server.listen", address);
    let ingest_listen = t.parsed("server.ingest_listen", address);
    let grpc_listen = t.parsed("server.grpc_listen", address);
    let metrics_listen = t.parsed("server.metrics_listen", address);
    let wal_backend = match t.text("data.wal_backend") {
        Some("auto") => Some(WalBackend::Auto),
        Some("portable") => Some(WalBackend::Portable),
        Some("uring") => Some(WalBackend::Uring),
        other => {
            if let (Some(value), Some((_, source))) = (other, t.values.get("data.wal_backend")) {
                t.errors.push(format!(
                    "`data.wal_backend` = {} is not one of auto, portable, uring ({source})",
                    quoted(value)
                ));
            }
            None
        }
    };
    let recent_max_bytes = t.parsed::<u64>("recent.max_bytes", "a whole number of bytes");
    let recent_ok = t.text("recent.max_bytes").is_none() || recent_max_bytes.is_some();
    let cpu_permits = t.parsed::<u32>("cpu.permits", "a whole number of blocking jobs");
    if cpu_permits == Some(0) {
        t.errors.push(
            "`cpu.permits` must be at least 1; leave it unset for the default of 8 per CPU"
                .to_owned(),
        );
    }
    let cpu_ok = t.text("cpu.permits").is_none() || cpu_permits.is_some_and(|n| n > 0);
    let idle_timeout = t
        .parsed::<u64>("clickhouse.idle_timeout", "a whole number of seconds")
        .map(Duration::from_secs);
    let idle_ok = t.text("clickhouse.idle_timeout").is_none() || idle_timeout.is_some();
    let calls_per_second = t.parsed::<u32>("mcp.calls_per_second", "a whole number of calls");
    let burst = t.parsed::<u32>("mcp.burst", "a whole number of calls");
    if burst == Some(0) {
        t.errors.push(
            "`mcp.burst` must be at least 1; set `mcp.calls_per_second` to 0 to turn the limit off"
                .to_owned(),
        );
    }
    let burst = burst.filter(|burst| *burst > 0);
    let statements_per_hour =
        t.parsed::<u64>("sql.statements_per_hour", "a whole number of statements");
    if statements_per_hour == Some(0) {
        t.errors.push(
            "`sql.statements_per_hour` must be at least 1; a run through the API is three statements"
                .to_owned(),
        );
    }
    let statements_per_hour = statements_per_hour.filter(|statements| *statements > 0);
    let pool_size = t.parsed::<usize>("postgres.pool_size", "a number of connections");
    if pool_size == Some(0) {
        t.errors
            .push("`postgres.pool_size` must be at least 1".to_owned());
    }
    let pool_size = pool_size.filter(|size| *size > 0);

    let mut master_key = t.secret("master_key");
    let mut master_key_ok = true;
    if let Some(key) = &master_key {
        let key = key.expose().trim();
        if key.len() != 64 || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            let problem = "`master_key` must be 64 hexadecimal characters (32 bytes); `openssl rand -hex 32` makes one";
            if t.text("clickhouse.url").is_some() || t.text("postgres.url").is_some() {
                master_key_ok = false;
                t.errors.push(problem.to_owned());
            } else {
                // Nothing uses the key without a telemetry store, and the first `serve` did not read it then.
                warnings.push(format!("{problem}. It is ignored: nothing uses it yet"));
                master_key = None;
            }
        }
    }

    let mut clickhouse_ok = true;
    let clickhouse = match t.text("clickhouse.url") {
        None => None,
        Some(url) => {
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                clickhouse_ok = false;
                t.errors.push(format!(
                    "`clickhouse.url` = {} must start with http:// or https://",
                    quoted(url)
                ));
            }
            let password = t.secret("clickhouse.password");
            if password.is_none() {
                t.errors.push(
                    "`clickhouse.url` needs the admin password: set `clickhouse.password_file` \
                     (--clickhouse-password-file, CRUCIBLE_CLICKHOUSE_PASSWORD_FILE) or CRUCIBLE_CLICKHOUSE_PASSWORD"
                        .to_owned(),
                );
            }
            if master_key.is_none() {
                t.errors.push(
                    "`clickhouse.url` needs the master key, from which the service passwords are derived: set \
                     `master_key_file` (--master-key-file, CRUCIBLE_MASTER_KEY_FILE) or CRUCIBLE_MASTER_KEY"
                        .to_owned(),
                );
            }
            match (password, &master_key, t.text("clickhouse.user")) {
                (Some(password), Some(master_key), Some(user)) => Some(ClickHouse {
                    url: url.to_owned(),
                    user: user.to_owned(),
                    password,
                    master_key: master_key.clone(),
                    idle_timeout,
                }),
                _ => {
                    clickhouse_ok = false;
                    None
                }
            }
        }
    };

    let mut postgres_ok = true;
    let postgres = match t.text("postgres.url") {
        None => {
            if t.text("postgres.password").is_some() {
                postgres_ok = false;
                t.errors.push(
                    "`postgres.password` is set without `postgres.url` (CRUCIBLE_POSTGRES_URL); set the URL or \
                     remove the password"
                        .to_owned(),
                );
            }
            None
        }
        Some(url) => {
            // The URL may carry a password, so the message does not quote it.
            if !(url.starts_with("postgres://") || url.starts_with("postgresql://")) {
                postgres_ok = false;
                t.errors.push(
                    "`postgres.url` must look like postgres://user@host:5432/database".to_owned(),
                );
            }
            if t.text("server.keys_file").is_some() {
                postgres_ok = false;
                t.errors.push(
                    "`server.keys_file` (--keys-file) is for a server without Postgres: with `postgres.url` set, API \
                     keys live in Postgres. Make them in Settings or with `crucible admin create-key --project \
                     default`, and remove the keys file setting"
                        .to_owned(),
                );
            }
            if master_key.is_none() && master_key_ok {
                postgres_ok = false;
                t.errors.push(
                    "`postgres.url` needs the master key, which seals stored secrets and keys API-key lookups: set \
                     `master_key_file` (--master-key-file, CRUCIBLE_MASTER_KEY_FILE) or CRUCIBLE_MASTER_KEY; \
                     `openssl rand -hex 32` makes one"
                        .to_owned(),
                );
            }
            pool_size.map(|pool_size| Postgres {
                url: Secret::new(url),
                password: t.secret("postgres.password"),
                pool_size,
            })
        }
    };

    let pg_outage_mode = match t.text("auth.pg_outage_mode") {
        Some("fail_open") => Some(PgOutageMode::FailOpen),
        Some("fail_closed") => Some(PgOutageMode::FailClosed),
        other => {
            if let (Some(value), Some((_, source))) = (other, t.values.get("auth.pg_outage_mode")) {
                t.errors.push(format!(
                    "`auth.pg_outage_mode` = {} is not one of fail_open, fail_closed ({source})",
                    quoted(value)
                ));
            }
            None
        }
    };
    let require_https = match t.text("security.require_https") {
        Some("true") => Some(true),
        Some("false") => Some(false),
        other => {
            if let (Some(value), Some((_, source))) =
                (other, t.values.get("security.require_https"))
            {
                t.errors.push(format!(
                    "`security.require_https` = {} is not true or false ({source})",
                    quoted(value)
                ));
            }
            None
        }
    };
    if require_https == Some(true)
        && !t
            .text("server.public_url")
            .is_some_and(|url| url.starts_with("https://"))
    {
        t.errors.push(
            "`security.require_https` = true needs `server.public_url` to be an https:// URL; set it, or set \
             `security.require_https` to false"
                .to_owned(),
        );
    }
    let fail_open_max = t.parsed::<u64>("auth.fail_open_max", "a whole number of seconds");
    let fail_open_ok = t.text("auth.fail_open_max").is_none() || fail_open_max.is_some();
    let session_outage_grace =
        t.parsed::<u64>("auth.session_outage_grace", "a whole number of seconds");
    let mut allowed_origins = Vec::new();
    let origins = t
        .text("server.allowed_origins")
        .unwrap_or_default()
        .to_owned();
    for origin in origins.split(',').map(str::trim).filter(|o| !o.is_empty()) {
        if origin.starts_with("http://") || origin.starts_with("https://") {
            allowed_origins.push(origin.to_owned());
        } else {
            t.errors.push(format!(
                "`server.allowed_origins` entry {} must be an http:// or https:// origin",
                quoted(origin)
            ));
        }
    }
    let public_url = t.text("server.public_url").map(str::to_owned);
    if let Some(url) = &public_url
        && !(url.starts_with("http://") || url.starts_with("https://"))
    {
        t.errors.push(format!(
            "`server.public_url` = {} must start with http:// or https://",
            quoted(url)
        ));
    }

    let strict = match t.text("egress.mode") {
        Some("open") => Some(false),
        Some("strict") => Some(true),
        other => {
            if let (Some(value), Some((_, source))) = (other, t.values.get("egress.mode")) {
                t.errors.push(format!(
                    "`egress.mode` = {} is not one of open, strict ({source})",
                    quoted(value)
                ));
            }
            None
        }
    };
    let mut allow_cidrs = Vec::new();
    let cidrs = t.text("egress.allow_cidrs").unwrap_or_default().to_owned();
    for cidr in cidrs.split(',').map(str::trim).filter(|c| !c.is_empty()) {
        if is_cidr(cidr) {
            allow_cidrs.push(cidr.to_owned());
        } else {
            t.errors.push(format!(
                "`egress.allow_cidrs` entry {} must be an address range such as 10.20.0.0/16 or fd12::/48",
                quoted(cidr)
            ));
        }
    }
    let smtp_tls = match t.text("smtp.tls") {
        Some("starttls") => Some(SmtpTls::StartTls),
        Some("tls") => Some(SmtpTls::Tls),
        Some("none") => Some(SmtpTls::None),
        other => {
            if let (Some(value), Some((_, source))) = (other, t.values.get("smtp.tls")) {
                t.errors.push(format!(
                    "`smtp.tls` = {} is not one of starttls, tls, none ({source})",
                    quoted(value)
                ));
            }
            None
        }
    };
    let smtp_port = t.parsed::<u16>("smtp.port", "a port number");
    let mut smtp_ok = t.text("smtp.port").is_none() || smtp_port.is_some();
    let smtp_from = t.text("smtp.from").map(str::to_owned);
    match (t.text("smtp.host"), &smtp_from) {
        (Some(_), None) => {
            t.errors.push(
                "`smtp.host` needs the sender address: set `smtp.from` (CRUCIBLE_SMTP_FROM), for example \
                 crucible@example.com"
                    .to_owned(),
            );
            smtp_ok = false;
        }
        (Some(_), Some(from)) if !from.contains('@') => {
            t.errors.push(format!(
                "`smtp.from` = {} must be an email address",
                quoted(from)
            ));
            smtp_ok = false;
        }
        (None, _) => {
            for key in ["smtp.from", "smtp.username", "smtp.password", "smtp.port"] {
                if t.text(key).is_some() {
                    warnings.push(format!(
                        "`{key}` is set but `smtp.host` is not, so no mail is sent; set `smtp.host` \
                         (CRUCIBLE_SMTP_HOST)"
                    ));
                }
            }
        }
        _ => {}
    }

    let media_max_bytes = t.parsed::<u64>("media.max_bytes", "a whole number of bytes");
    if media_max_bytes.is_some_and(|max| max == 0 || max > MAX_MEDIA_BYTES) {
        t.errors.push(format!(
            "`media.max_bytes` must be between 1 and {MAX_MEDIA_BYTES} bytes (5 GiB, the most one upload to an \
             object store may hold)"
        ));
    }
    let media_max_bytes = media_max_bytes.filter(|max| (1..=MAX_MEDIA_BYTES).contains(max));
    let (object_store, object_store_ok) = object_store(&mut t);
    let (providers, providers_ok) = sign_in_providers(&mut t, warnings);

    if !(recent_ok
        && cpu_ok
        && idle_ok
        && master_key_ok
        && clickhouse_ok
        && postgres_ok
        && fail_open_ok
        && smtp_ok
        && object_store_ok
        && providers_ok)
    {
        return None;
    }
    let smtp = match (t.text("smtp.host"), smtp_from) {
        (Some(host), Some(from)) => {
            let tls = smtp_tls?;
            Some(Smtp {
                host: host.to_owned(),
                port: smtp_port.unwrap_or(match tls {
                    SmtpTls::StartTls => 587,
                    SmtpTls::Tls => 465,
                    SmtpTls::None => 25,
                }),
                tls,
                username: t.text("smtp.username").map(str::to_owned),
                password: t.secret("smtp.password"),
                from,
            })
        }
        _ => {
            smtp_tls?;
            None
        }
    };
    Some(Config {
        server: Server {
            listen: listen?,
            ingest_listen: ingest_listen?,
            grpc_listen: grpc_listen?,
            metrics_listen: metrics_listen?,
            ui_dir: t.text("server.ui_dir").map(Into::into),
            keys_file: t.text("server.keys_file").map(Into::into),
            public_url,
            allowed_origins,
        },
        data: Data {
            dir: t.text("data.dir")?.into(),
            wal_backend: wal_backend?,
        },
        recent_max_bytes,
        cpu_permits,
        mcp: Mcp {
            calls_per_second: calls_per_second?,
            burst: burst?,
        },
        sql: Sql {
            statements_per_hour: statements_per_hour?,
        },
        master_key,
        clickhouse,
        postgres: match t.text("postgres.url") {
            Some(_) => Some(postgres?),
            None => None,
        },
        auth: Auth {
            pg_outage_mode: pg_outage_mode?,
            fail_open_max,
            session_outage_grace: session_outage_grace?,
            require_https: require_https?,
            providers,
        },
        egress: Egress {
            strict: strict?,
            allow_cidrs,
        },
        smtp,
        object_store,
        media_max_bytes: media_max_bytes?,
    })
}

/// The most `media.max_bytes` may be: 5 GiB, the largest single upload an object store takes.
const MAX_MEDIA_BYTES: u64 = 5 << 30;

/// The `[object_store]` section, and whether it is free of problems. Media need the control plane too, which
/// records the objects and their uses.
fn object_store(t: &mut Typed<'_>) -> (Option<ObjectStore>, bool) {
    let Some(endpoint) = t.text("object_store.endpoint").map(str::to_owned) else {
        let mut ok = true;
        for key in [
            "object_store.public_endpoint",
            "object_store.bucket",
            "object_store.access_key",
            "object_store.secret_key",
        ] {
            if t.text(key).is_some() {
                ok = false;
                t.errors.push(format!(
                    "`{key}` is set but `object_store.endpoint` is not, so media stay off; set \
                     `object_store.endpoint` (CRUCIBLE_OBJECT_STORE_ENDPOINT) or remove `{key}`"
                ));
            }
        }
        return (None, ok);
    };
    let mut ok = true;
    for (key, value) in [
        ("object_store.endpoint", Some(endpoint.as_str())),
        (
            "object_store.public_endpoint",
            t.text("object_store.public_endpoint"),
        ),
    ] {
        if let Some(value) = value
            && !(value.starts_with("http://") || value.starts_with("https://"))
        {
            ok = false;
            t.errors.push(format!(
                "`{key}` = {} must start with http:// or https://",
                quoted(value)
            ));
        }
    }
    let virtual_host = match t.text("object_store.url_style") {
        Some("path") | None => false,
        Some("virtual_host") => true,
        Some(other) => {
            ok = false;
            let other = quoted(other);
            t.errors.push(format!(
                "`object_store.url_style` = {other} is not one of path, virtual_host"
            ));
            false
        }
    };
    if t.text("postgres.url").is_none() {
        ok = false;
        t.errors.push(
            "`object_store.endpoint` needs the control plane, which records media: set `postgres.url` \
             (CRUCIBLE_POSTGRES_URL), or remove the [object_store] settings"
                .to_owned(),
        );
    }
    let bucket = t.text("object_store.bucket").map(str::to_owned);
    let access_key = t.text("object_store.access_key").map(str::to_owned);
    let secret_key = t.secret("object_store.secret_key");
    for (key, missing, variable) in [
        (
            "object_store.bucket",
            bucket.is_none(),
            "CRUCIBLE_OBJECT_STORE_BUCKET",
        ),
        (
            "object_store.access_key",
            access_key.is_none(),
            "CRUCIBLE_OBJECT_STORE_ACCESS_KEY",
        ),
        (
            "object_store.secret_key",
            secret_key.is_none(),
            "CRUCIBLE_OBJECT_STORE_SECRET_KEY_FILE or CRUCIBLE_OBJECT_STORE_SECRET_KEY",
        ),
    ] {
        if missing {
            ok = false;
            t.errors.push(format!(
                "`object_store.endpoint` needs `{key}`: set it ({variable})"
            ));
        }
    }
    let store = match (bucket, access_key, secret_key, ok) {
        (Some(bucket), Some(access_key), Some(secret_key), true) => Some(ObjectStore {
            endpoint,
            public_endpoint: t.text("object_store.public_endpoint").map(str::to_owned),
            region: t
                .text("object_store.region")
                .unwrap_or("us-east-1")
                .to_owned(),
            bucket,
            access_key,
            secret_key,
            virtual_host,
            prefix: t
                .text("object_store.prefix")
                .unwrap_or_default()
                .trim_matches('/')
                .to_owned(),
        }),
        _ => None,
    };
    (store, ok)
}

/// The sign-in providers of `[auth_github]`, `[auth_google]` and `[auth_oidc]`, and whether they are free of
/// problems. A section turns on with its client id; it then needs the client secret, OpenID Connect its issuer, and
/// every one the control plane, which keeps sign-in.
fn sign_in_providers(t: &mut Typed<'_>, warnings: &mut Vec<String>) -> (Vec<SignInProvider>, bool) {
    let mut providers = Vec::new();
    let mut ok = true;
    for (kind, section, label) in [
        (SignInKind::Github, "auth_github", "GitHub"),
        (SignInKind::Google, "auth_google", "Google"),
        (SignInKind::Oidc, "auth_oidc", "OpenID Connect"),
    ] {
        let key = |name: &str| format!("{section}.{name}");
        let variable = |name: &str| format!("CRUCIBLE_{}_{}", section, name).to_uppercase();
        let mut names = vec!["client_secret", "display_name", "allowed_domains"];
        if kind == SignInKind::Oidc {
            names.extend(["issuer", "scopes"]);
        }
        let Some(client_id) = t.text(&key("client_id")).map(str::trim).map(str::to_owned) else {
            for name in names {
                if t.text(&key(name)).is_some() {
                    ok = false;
                    t.errors.push(format!(
                        "`{}` is set but `{}` is not, so {label} sign-in stays off; set `{}` ({}) or remove `{}`",
                        key(name),
                        key("client_id"),
                        key("client_id"),
                        variable("client_id"),
                        key(name)
                    ));
                }
            }
            continue;
        };
        let before = t.errors.len();
        if client_id.is_empty() {
            t.errors.push(format!(
                "`{}` is empty; give the client id the provider issued",
                key("client_id")
            ));
        }
        let client_secret = t.secret(&key("client_secret"));
        match &client_secret {
            None => t.errors.push(format!(
                "`{}` needs the client secret: set `{}` ({}) or {}",
                key("client_id"),
                key("client_secret_file"),
                variable("client_secret_file"),
                variable("client_secret")
            )),
            Some(secret) if secret.expose().trim().is_empty() => t.errors.push(format!(
                "`{}` is empty; give the client secret the provider issued",
                key("client_secret")
            )),
            Some(_) => {}
        }
        let issuer = match (kind, t.text(&key("issuer"))) {
            (SignInKind::Oidc, None) => {
                t.errors.push(format!(
                    "`{}` needs the provider's issuer: set `{}` ({}), for example https://login.example.com",
                    key("client_id"),
                    key("issuer"),
                    variable("issuer")
                ));
                None
            }
            (SignInKind::Oidc, Some(issuer))
                if !(issuer.starts_with("https://") || issuer.starts_with("http://")) =>
            {
                t.errors.push(format!(
                    "`{}` = {} must start with https:// or http://",
                    key("issuer"),
                    quoted(issuer)
                ));
                None
            }
            (_, issuer) => issuer.map(str::to_owned),
        };
        let mut allowed_domains = Vec::new();
        let domains = t.text(&key("allowed_domains")).unwrap_or_default();
        for entry in domains.split(',').map(str::trim).filter(|d| !d.is_empty()) {
            let domain = entry.trim_start_matches('@').to_ascii_lowercase();
            if domain.is_empty() || domain.contains(['@', ' ', '/']) {
                t.errors.push(format!(
                    "`{}` entry {} must be a bare domain such as example.com",
                    key("allowed_domains"),
                    quoted(entry)
                ));
            } else {
                allowed_domains.push(domain);
            }
        }
        if t.text("postgres.url").is_none() {
            t.errors.push(format!(
                "`{}` needs the control plane, which keeps sign-in: set `postgres.url` (CRUCIBLE_POSTGRES_URL), or \
                 remove the [{section}] settings",
                key("client_id")
            ));
        }
        let scopes = t
            .text(&key("scopes"))
            .unwrap_or_default()
            .split([',', ' '])
            .filter(|scope| !scope.is_empty())
            .map(str::to_owned)
            .collect();
        match client_secret {
            Some(client_secret) if t.errors.len() == before => providers.push(SignInProvider {
                kind,
                display_name: t
                    .text(&key("display_name"))
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned),
                issuer,
                client_id,
                client_secret,
                scopes,
                allowed_domains,
            }),
            _ => ok = false,
        }
    }
    if !providers.is_empty() && t.text("server.public_url").is_none() {
        warnings.push(
            "the sign-in providers of the configuration are offered only once `server.public_url` is set \
             (CRUCIBLE_SERVER_PUBLIC_URL): their callback URLs are built from it"
                .to_owned(),
        );
    }
    (providers, ok)
}

/// Whether `text` is `address/prefix` with a prefix that fits the address family.
fn is_cidr(text: &str) -> bool {
    let Some((address, prefix)) = text.split_once('/') else {
        return false;
    };
    match (address.parse::<std::net::IpAddr>(), prefix.parse::<u8>()) {
        (Ok(std::net::IpAddr::V4(_)), Ok(prefix)) => prefix <= 32,
        (Ok(std::net::IpAddr::V6(_)), Ok(prefix)) => prefix <= 128,
        _ => false,
    }
}

/// Resolves the layers into a configuration. `read_file` reads the files that `_file` settings name. Every
/// problem is collected, so one run reports them all.
pub fn resolve(
    layers: &Layers<'_>,
    read_file: &dyn Fn(&Path) -> io::Result<String>,
) -> Result<Loaded, ConfigErrors> {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut values: Values = SETTINGS
        .iter()
        .filter_map(|setting| Some((setting.key, (setting.default?.to_owned(), Source::Default))))
        .collect();

    if let Some((name, text)) = layers.file {
        read_file_layer(name, text, &mut values, &mut errors);
    }

    let variables: Vec<_> = SETTINGS
        .iter()
        .map(|setting| (setting.variable(), setting))
        .collect();
    for (name, value) in layers.env {
        if !name.starts_with("CRUCIBLE_") || name == crate::CONFIG_FILE_VARIABLE {
            continue;
        }
        match variables.iter().find(|(variable, _)| variable == name) {
            // An empty variable is unset, as compose files pass them; only a secret may be empty.
            Some((_, setting)) if value.is_empty() && !setting.secret => {}
            None if value.is_empty() => {}
            Some((_, setting)) => {
                values.insert(setting.key, (value.clone(), Source::Env(name.clone())));
            }
            None => {
                let hint = nearest(
                    name,
                    variables.iter().map(|(variable, _)| variable.as_str()),
                )
                .map(|known| format!("; did you mean {known}?"))
                .unwrap_or_default();
                warnings.push(format!(
                    "the environment variable {name} is not a Crucible setting and is ignored{hint}"
                ));
            }
        }
    }

    for (key, value) in layers.flags {
        match find(key) {
            Some(setting) => {
                values.insert(setting.key, (value.clone(), Source::Flag));
            }
            None => errors.push(format!("a flag sets `{key}`, which is not a setting")),
        }
    }

    let from_file = read_secret_files(&mut values, read_file, &mut errors, &mut warnings);
    let config = typed(&values, &mut errors, &mut warnings);
    match config {
        Some(config) if errors.is_empty() => Ok(Loaded {
            config,
            warnings,
            values,
            from_file,
        }),
        _ => {
            if errors.is_empty() {
                errors.push("the configuration is incomplete".to_owned());
            }
            Err(ConfigErrors(errors))
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    fn no_files(path: &Path) -> io::Result<String> {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} is not there", path.display()),
        ))
    }

    fn files(path: &Path) -> io::Result<String> {
        match path.to_str() {
            Some("/run/secrets/clickhouse") => Ok("s3cret-from-file\r\n".to_owned()),
            Some("/run/secrets/master") => Ok(format!("{KEY}\n")),
            _ => no_files(path),
        }
    }

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn load(
        file: Option<&str>,
        env: &[(String, String)],
        flags: &[(&str, String)],
    ) -> Result<Loaded, ConfigErrors> {
        let layers = Layers {
            file: file.map(|text| ("crucible.toml", text)),
            env,
            flags,
        };
        resolve(&layers, &files)
    }

    #[test]
    fn the_defaults_are_the_contract_ports_and_no_stores() {
        let loaded = load(None, &[], &[]).unwrap();
        let config = &loaded.config;
        assert_eq!(config.server.listen.to_string(), "0.0.0.0:8080");
        assert_eq!(config.server.ingest_listen.to_string(), "0.0.0.0:4318");
        assert_eq!(config.server.grpc_listen.to_string(), "0.0.0.0:4317");
        assert_eq!(config.server.metrics_listen.to_string(), "127.0.0.1:9464");
        assert_eq!(config.data.dir, Path::new("data"));
        assert_eq!(config.data.wal_backend, WalBackend::Auto);
        assert_eq!(config.server.ui_dir, None);
        assert_eq!(config.server.keys_file, None);
        assert_eq!(config.recent_max_bytes, None);
        assert_eq!(
            config.mcp,
            Mcp {
                calls_per_second: 50,
                burst: 200
            }
        );
        assert_eq!(config.sql.statements_per_hour, 1_800);
        assert_eq!(config.clickhouse, None);
        assert_eq!(config.postgres, None);
        assert_eq!(config.master_key, None);
        assert!(loaded.warnings.is_empty());
        assert_eq!(loaded.source("server.listen"), Some(&Source::Default));
        assert_eq!(loaded.source("clickhouse.url"), None);
    }

    #[test]
    fn the_file_sets_every_section() {
        let file = format!(
            r#"
master_key = "{KEY}"

[server]
listen = "127.0.0.1:9000"
ui_dir = "ui/out"

[data]
dir = "/var/lib/crucible"
wal_backend = "portable"

[recent]
max_bytes = 1048576

[clickhouse]
url = "http://clickhouse:8123"
password = "pw"
idle_timeout = 50

[postgres]
url = "postgres://crucible@postgres:5432/crucible"
password_file = "/run/secrets/clickhouse"
pool_size = 4
"#
        );
        let config = load(Some(&file), &[], &[]).unwrap().config;
        assert_eq!(config.server.listen.to_string(), "127.0.0.1:9000");
        assert_eq!(config.server.ui_dir.as_deref(), Some(Path::new("ui/out")));
        assert_eq!(config.server.keys_file, None);
        assert_eq!(config.data.dir, Path::new("/var/lib/crucible"));
        assert_eq!(config.data.wal_backend, WalBackend::Portable);
        assert_eq!(config.recent_max_bytes, Some(1 << 20));
        let clickhouse = config.clickhouse.unwrap();
        assert_eq!(clickhouse.url, "http://clickhouse:8123");
        assert_eq!(clickhouse.user, "crucible_admin");
        assert_eq!(clickhouse.password.expose(), "pw");
        assert_eq!(clickhouse.master_key.expose(), KEY);
        assert_eq!(clickhouse.idle_timeout, Some(Duration::from_secs(50)));
        let postgres = config.postgres.unwrap();
        assert_eq!(
            postgres.url.expose(),
            "postgres://crucible@postgres:5432/crucible"
        );
        assert_eq!(postgres.password.unwrap().expose(), "s3cret-from-file");
        assert_eq!(postgres.pool_size, 4);
    }

    #[test]
    fn the_environment_overrides_the_file_and_flags_override_both() {
        let file = "[server]\nlisten = \"127.0.0.1:1\"\ningest_listen = \"127.0.0.1:2\"\nmetrics_listen = \"127.0.0.1:3\"\n";
        let env = env(&[
            ("CRUCIBLE_SERVER_INGEST_LISTEN", "127.0.0.1:20"),
            ("CRUCIBLE_SERVER_METRICS_LISTEN", "127.0.0.1:30"),
            ("PATH", "/bin"),
        ]);
        let flags = [("server.metrics_listen", "127.0.0.1:300".to_owned())];
        let loaded = load(Some(file), &env, &flags).unwrap();
        assert_eq!(loaded.config.server.listen.to_string(), "127.0.0.1:1");
        assert_eq!(
            loaded.config.server.ingest_listen.to_string(),
            "127.0.0.1:20"
        );
        assert_eq!(
            loaded.config.server.metrics_listen.to_string(),
            "127.0.0.1:300"
        );
        assert_eq!(loaded.source("server.listen"), Some(&Source::File));
        assert_eq!(
            loaded.source("server.ingest_listen"),
            Some(&Source::Env("CRUCIBLE_SERVER_INGEST_LISTEN".to_owned()))
        );
        assert_eq!(loaded.source("server.metrics_listen"), Some(&Source::Flag));
    }

    #[test]
    fn the_variables_and_flags_of_the_first_serve_keep_working() {
        // What `serve` took before this crate: the URL and the user by flag or variable, the secrets by a
        // file flag or a value variable.
        let env = env(&[
            ("CRUCIBLE_CLICKHOUSE_URL", "http://clickhouse:8123"),
            ("CRUCIBLE_CLICKHOUSE_PASSWORD", "from-env"),
            ("CRUCIBLE_MASTER_KEY", KEY),
        ]);
        let clickhouse = load(None, &env, &[]).unwrap().config.clickhouse.unwrap();
        assert_eq!(clickhouse.password.expose(), "from-env");
        assert_eq!(clickhouse.master_key.expose(), KEY);
        // A file flag wins over the value variable.
        let flags = [
            (
                "clickhouse.password_file",
                "/run/secrets/clickhouse".to_owned(),
            ),
            ("master_key_file", "/run/secrets/master".to_owned()),
            ("clickhouse.user", "admin2".to_owned()),
        ];
        let loaded = load(None, &env, &flags).unwrap();
        let clickhouse = loaded.config.clickhouse.clone().unwrap();
        assert_eq!(clickhouse.password.expose(), "s3cret-from-file");
        assert_eq!(clickhouse.master_key.expose(), KEY);
        assert_eq!(clickhouse.user, "admin2");
        // And a value variable wins over a file named in the configuration file.
        let file = "[clickhouse]\npassword_file = \"/nowhere\"\n";
        let clickhouse = load(Some(file), &env, &[])
            .unwrap()
            .config
            .clickhouse
            .unwrap();
        assert_eq!(clickhouse.password.expose(), "from-env");
    }

    #[test]
    fn unknown_keys_are_errors_that_name_the_nearest_key() {
        let file = r#"
[server]
lisen = "0.0.0.0:1"
[webhooks]
host = "mail"
[clickhouse]
master_key = "abc"
[data]
directory_of_everything = "x"
"#;
        let ConfigErrors(errors) = load(Some(file), &[], &[]).unwrap_err();
        assert_eq!(
            errors,
            [
                "crucible.toml: `master_key` is a top-level key, not a key of [clickhouse]: move it above the first [section] line",
                "crucible.toml: unknown key `data.directory_of_everything`; the keys of [data] are dir, wal_backend",
                "crucible.toml: unknown key `server.lisen`; did you mean `server.listen`?",
                "crucible.toml: unknown section [webhooks]. The sections are [server], [data], [recent], [cpu], [mcp], [sql], [clickhouse], [postgres], [security], [auth], [auth_github], [auth_google], [auth_oidc], [egress], [smtp], [object_store], [media].",
            ]
        );
    }

    #[test]
    fn every_problem_is_reported_in_one_run() {
        let file = "[server]\nlisten = \"nowhere\"\n[data]\nwal_backend = \"floppy\"\n[recent]\nmax_bytes = \"lots\"\n[postgres]\nurl = \"mysql://secret-password@db\"\npool_size = 0\n";
        let env = env(&[
            ("CRUCIBLE_CLICKHOUSE_URL", "clickhouse:9000"),
            ("CRUCIBLE_MASTER_KEY", "too-short-secret"),
        ]);
        let error = load(Some(file), &env, &[]).unwrap_err();
        let text = error.to_string();
        assert_eq!(error.0.len(), 8, "{text}");
        for needle in [
            "the configuration has 8 problems:",
            "`server.listen` = \"nowhere\" is not an address such as 0.0.0.0:8080 (file)",
            "`data.wal_backend` = \"floppy\" is not one of auto, portable, uring (file)",
            "`recent.max_bytes` = \"lots\" is not a whole number of bytes (file)",
            "`postgres.pool_size` must be at least 1",
            "`master_key` must be 64 hexadecimal characters",
            "`clickhouse.url` = \"clickhouse:9000\" must start with http:// or https://",
            "`clickhouse.url` needs the admin password",
            "`postgres.url` must look like postgres://user@host:5432/database",
        ] {
            assert!(text.contains(needle), "{needle} is missing from:\n{text}");
        }
        // No message quotes a secret.
        assert!(!text.contains("too-short-secret") && !text.contains("secret-password"));
    }

    #[test]
    fn a_file_that_does_not_parse_is_reported_without_its_text() {
        let file = "[server]\nlisten = \"0.0.0.0:1\"\nmaster_key = hunter2-secret\n";
        let ConfigErrors(errors) = load(Some(file), &[], &[]).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].starts_with("crucible.toml line 3: "),
            "{errors:?}"
        );
        assert!(!errors[0].contains("hunter2"), "{errors:?}");
        let ConfigErrors(errors) = load(Some("[server]\nlisten = [1, 2]\n"), &[], &[]).unwrap_err();
        assert_eq!(
            errors,
            ["crucible.toml: `server.listen` must be a string, a whole number or true or false"]
        );
    }

    #[test]
    fn a_secret_file_that_cannot_be_read_is_an_error_that_names_the_setting() {
        let env = env(&[
            ("CRUCIBLE_POSTGRES_URL", "postgres://u@h/d"),
            ("CRUCIBLE_POSTGRES_PASSWORD_FILE", "/missing"),
            ("CRUCIBLE_MASTER_KEY", KEY),
        ]);
        let ConfigErrors(errors) = load(None, &env, &[]).unwrap_err();
        assert_eq!(
            errors,
            [
                "cannot read `postgres.password_file` /missing (env CRUCIBLE_POSTGRES_PASSWORD_FILE): /missing is not there"
            ]
        );
    }

    #[test]
    fn both_forms_of_a_secret_in_one_layer_use_the_file_and_warn() {
        // The first `serve` let the file win over the value variable.
        let env = env(&[
            ("CRUCIBLE_CLICKHOUSE_URL", "http://clickhouse:8123"),
            ("CRUCIBLE_CLICKHOUSE_PASSWORD", "from-env"),
            (
                "CRUCIBLE_CLICKHOUSE_PASSWORD_FILE",
                "/run/secrets/clickhouse",
            ),
            ("CRUCIBLE_MASTER_KEY", KEY),
        ]);
        let loaded = load(None, &env, &[]).unwrap();
        assert_eq!(
            loaded.config.clickhouse.unwrap().password.expose(),
            "s3cret-from-file"
        );
        assert_eq!(
            loaded.warnings,
            [
                "both `clickhouse.password` and `clickhouse.password_file` are set (env CRUCIBLE_CLICKHOUSE_PASSWORD and env CRUCIBLE_CLICKHOUSE_PASSWORD_FILE); the file is used. Set only one"
            ]
        );
    }

    #[test]
    fn a_malformed_master_key_is_an_error_only_where_the_key_is_used() {
        let env = env(&[("CRUCIBLE_MASTER_KEY", "too-short-secret")]);
        let loaded = load(None, &env, &[]).unwrap();
        assert_eq!(loaded.config.master_key, None);
        assert_eq!(loaded.warnings.len(), 1);
        assert!(
            loaded.warnings[0].contains("64 hexadecimal")
                && !loaded.warnings[0].contains("too-short")
        );
    }

    #[test]
    fn an_empty_secret_variable_is_an_empty_secret() {
        // A ClickHouse admin without a password, as the first `serve` accepted.
        let env = env(&[
            ("CRUCIBLE_CLICKHOUSE_URL", "http://clickhouse:8123"),
            ("CRUCIBLE_CLICKHOUSE_PASSWORD", ""),
            ("CRUCIBLE_MASTER_KEY", KEY),
        ]);
        let clickhouse = load(None, &env, &[]).unwrap().config.clickhouse.unwrap();
        assert_eq!(clickhouse.password.expose(), "");
    }

    #[test]
    fn unknown_variables_are_warnings_and_empty_ones_are_unset() {
        let env = env(&[
            ("CRUCIBLE_SERVER_LISTN", "0.0.0.0:1"),
            ("CRUCIBLE_SIM_SEED", "7"),
            ("CRUCIBLE_CLICKHOUSE_URL", ""),
            ("CRUCIBLE_NOT_A_SETTING", ""),
            ("CRUCIBLE_CONFIG", "crucible.toml"),
        ]);
        let loaded = load(None, &env, &[]).unwrap();
        assert_eq!(
            loaded.warnings,
            [
                "the environment variable CRUCIBLE_SERVER_LISTN is not a Crucible setting and is ignored; did you mean CRUCIBLE_SERVER_LISTEN?",
                "the environment variable CRUCIBLE_SIM_SEED is not a Crucible setting and is ignored",
            ]
        );
        assert_eq!(loaded.config.clickhouse, None);
    }

    #[test]
    fn postgres_needs_the_master_key_and_the_auth_settings_parse() {
        let without_key = env(&[("CRUCIBLE_POSTGRES_URL", "postgres://u@h/d")]);
        let ConfigErrors(errors) = load(None, &without_key, &[]).unwrap_err();
        assert!(
            errors[0].contains("`postgres.url` needs the master key"),
            "{errors:?}"
        );
        let ok = env(&[
            ("CRUCIBLE_POSTGRES_URL", "postgres://u@h/d"),
            ("CRUCIBLE_MASTER_KEY", KEY),
            ("CRUCIBLE_SERVER_PUBLIC_URL", "https://crucible.example.com"),
            (
                "CRUCIBLE_SERVER_ALLOWED_ORIGINS",
                "http://localhost:3000, https://a.example",
            ),
            ("CRUCIBLE_AUTH_PG_OUTAGE_MODE", "fail_closed"),
            ("CRUCIBLE_AUTH_FAIL_OPEN_MAX", "120"),
            ("CRUCIBLE_AUTH_SESSION_OUTAGE_GRACE", "0"),
        ]);
        let config = load(None, &ok, &[]).unwrap().config;
        assert_eq!(
            config.server.public_url.as_deref(),
            Some("https://crucible.example.com")
        );
        assert_eq!(
            config.server.allowed_origins,
            ["http://localhost:3000", "https://a.example"]
        );
        assert_eq!(
            config.auth,
            Auth {
                pg_outage_mode: PgOutageMode::FailClosed,
                fail_open_max: Some(120),
                session_outage_grace: 0,
                require_https: false,
                providers: Vec::new(),
            }
        );
        let https = env(&[
            ("CRUCIBLE_SECURITY_REQUIRE_HTTPS", "true"),
            ("CRUCIBLE_SERVER_PUBLIC_URL", "http://crucible.internal"),
        ]);
        let ConfigErrors(errors) = load(None, &https, &[]).unwrap_err();
        assert!(
            errors[0].contains("needs `server.public_url` to be an https://"),
            "{errors:?}"
        );
        let bad = env(&[
            ("CRUCIBLE_AUTH_PG_OUTAGE_MODE", "sometimes"),
            ("CRUCIBLE_SERVER_PUBLIC_URL", "crucible.example.com"),
            ("CRUCIBLE_SERVER_ALLOWED_ORIGINS", "localhost"),
        ]);
        let ConfigErrors(errors) = load(None, &bad, &[]).unwrap_err();
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert!(!load(None, &[], &[]).unwrap().config.auth.require_https);
        let both = env(&[
            ("CRUCIBLE_POSTGRES_URL", "postgres://u@h/d"),
            ("CRUCIBLE_MASTER_KEY", KEY),
            ("CRUCIBLE_SERVER_KEYS_FILE", "/etc/crucible/keys.toml"),
        ]);
        let ConfigErrors(errors) = load(None, &both, &[]).unwrap_err();
        assert!(
            errors[0].contains("crucible admin create-key"),
            "{errors:?}"
        );
        // Without Postgres the keys file is the server's keys.
        let file_only = env(&[("CRUCIBLE_SERVER_KEYS_FILE", "/etc/crucible/keys.toml")]);
        assert_eq!(
            load(None, &file_only, &[]).unwrap().config.server.keys_file,
            Some(std::path::PathBuf::from("/etc/crucible/keys.toml"))
        );
        let defaults = load(None, &[], &[]).unwrap().config.auth;
        assert_eq!(defaults.pg_outage_mode, PgOutageMode::FailOpen);
        assert_eq!(defaults.session_outage_grace, 600);
    }

    #[test]
    fn the_object_store_needs_postgres_and_its_credentials_and_says_what_is_missing() {
        let defaults = load(None, &[], &[]).unwrap().config;
        assert_eq!(defaults.object_store, None);
        assert_eq!(defaults.media_max_bytes, 64 << 20);
        let full = env(&[
            ("CRUCIBLE_POSTGRES_URL", "postgres://u@h/d"),
            ("CRUCIBLE_MASTER_KEY", KEY),
            ("CRUCIBLE_OBJECT_STORE_ENDPOINT", "http://versitygw:7070"),
            (
                "CRUCIBLE_OBJECT_STORE_PUBLIC_ENDPOINT",
                "http://localhost:7070",
            ),
            ("CRUCIBLE_OBJECT_STORE_BUCKET", "media"),
            ("CRUCIBLE_OBJECT_STORE_ACCESS_KEY", "crucible"),
            ("CRUCIBLE_OBJECT_STORE_SECRET_KEY", "hunter2"),
            ("CRUCIBLE_OBJECT_STORE_PREFIX", "/media/"),
            ("CRUCIBLE_MEDIA_MAX_BYTES", "1048576"),
        ]);
        let config = load(None, &full, &[]).unwrap().config;
        let store = config.object_store.unwrap();
        assert_eq!(
            (
                store.endpoint.as_str(),
                store.public_endpoint.as_deref(),
                store.region.as_str(),
                store.bucket.as_str(),
                store.prefix.as_str(),
                store.virtual_host
            ),
            (
                "http://versitygw:7070",
                Some("http://localhost:7070"),
                "us-east-1",
                "media",
                "media",
                false
            )
        );
        assert_eq!(store.secret_key.expose(), "hunter2");
        assert!(!format!("{store:?}").contains("hunter2"));
        assert_eq!(config.media_max_bytes, 1 << 20);
        // Without Postgres, credentials or a valid URL style, the refusals say what to set.
        let bare = env(&[
            ("CRUCIBLE_OBJECT_STORE_ENDPOINT", "versitygw:7070"),
            ("CRUCIBLE_OBJECT_STORE_URL_STYLE", "dns"),
            ("CRUCIBLE_MEDIA_MAX_BYTES", "0"),
        ]);
        let ConfigErrors(errors) = load(None, &bare, &[]).unwrap_err();
        let expected = [
            "`media.max_bytes` must be between 1 and",
            "`object_store.endpoint` = \"versitygw:7070\" must start with http:// or https://",
            "`object_store.url_style` = \"dns\" is not one of path, virtual_host",
            "`object_store.endpoint` needs the control plane, which records media: set `postgres.url`",
            "`object_store.endpoint` needs `object_store.bucket`",
            "`object_store.endpoint` needs `object_store.access_key`",
            "`object_store.endpoint` needs `object_store.secret_key`",
        ];
        assert_eq!(errors.len(), expected.len(), "{errors:?}");
        for (error, expected) in errors.iter().zip(expected) {
            assert!(error.contains(expected), "{error}");
        }
        // A bucket without an endpoint is a mistake, not a silent no.
        let orphan = env(&[("CRUCIBLE_OBJECT_STORE_BUCKET", "media")]);
        let ConfigErrors(errors) = load(None, &orphan, &[]).unwrap_err();
        assert!(
            errors[0].contains("`object_store.bucket` is set but `object_store.endpoint` is not"),
            "{errors:?}"
        );
    }

    #[test]
    fn print_shows_sources_and_never_a_secret() {
        let file = format!(
            "master_key = \"{KEY}\"\n[recent]\nmax_bytes = 4096\n[postgres]\nurl = \"postgres://crucible:pg-secret@postgres:5432/crucible?sslmode=require\"\n"
        );
        let env = env(&[
            ("CRUCIBLE_CLICKHOUSE_URL", "http://clickhouse:8123"),
            (
                "CRUCIBLE_CLICKHOUSE_PASSWORD_FILE",
                "/run/secrets/clickhouse",
            ),
        ]);
        let flags = [("data.dir", "C:\\data \"x\"".to_owned())];
        let loaded = load(Some(&file), &env, &flags).unwrap();
        let printed = loaded.print();
        assert_eq!(
            printed,
            r#"master_key = "<redacted>"  # file

[server]
listen = "0.0.0.0:8080"  # default
ingest_listen = "0.0.0.0:4318"  # default
grpc_listen = "0.0.0.0:4317"  # default
metrics_listen = "127.0.0.1:9464"  # default

[data]
dir = "C:\\data \"x\""  # flag
wal_backend = "auto"  # default

[recent]
max_bytes = 4096  # file

[mcp]
calls_per_second = 50  # default
burst = 200  # default

[sql]
statements_per_hour = 1800  # default

[clickhouse]
url = "http://clickhouse:8123"  # env CRUCIBLE_CLICKHOUSE_URL
user = "crucible_admin"  # default
password_file = "/run/secrets/clickhouse"  # env CRUCIBLE_CLICKHOUSE_PASSWORD_FILE

[postgres]
url = "postgres://crucible:<redacted>@postgres:5432/crucible?sslmode=require"  # file
pool_size = 8  # default

[security]
require_https = false  # default

[auth]
pg_outage_mode = "fail_open"  # default
session_outage_grace = 600  # default

[egress]
mode = "open"  # default

[smtp]
tls = "starttls"  # default

[object_store]
region = "us-east-1"  # default
url_style = "path"  # default
prefix = "media"  # default

[media]
max_bytes = 67108864  # default
"#
        );
        // What is printed loads again, to the same settings apart from the redacted secrets.
        let again = load(Some(&printed), &[], &[]).unwrap_err();
        assert_eq!(again.0.len(), 1, "{again}");
        assert!(again.0[0].contains("`master_key` must be 64 hexadecimal characters"));
        let debug = format!("{:?}", loaded.config);
        for secret in [KEY, "pg-secret", "s3cret-from-file"] {
            assert!(
                !printed.contains(secret) && !debug.contains(secret),
                "{secret} leaked"
            );
        }
        assert_eq!(Secret::new("x").to_string(), "<redacted>");
    }

    #[test]
    fn the_egress_policy_and_the_mail_server_parse_and_say_what_is_missing() {
        let defaults = load(None, &[], &[]).unwrap().config;
        assert_eq!(defaults.egress, Egress::default());
        assert_eq!(defaults.smtp, None);
        let file = "[egress]\nmode = \"strict\"\nallow_cidrs = \"10.20.0.0/16, fd12::/48\"\n\
                    [smtp]\nhost = \"mail.internal\"\nfrom = \"crucible@example.com\"\ntls = \"none\"\n";
        let config = load(Some(file), &[], &[]).unwrap().config;
        assert!(config.egress.strict);
        assert_eq!(config.egress.allow_cidrs, ["10.20.0.0/16", "fd12::/48"]);
        let smtp = config.smtp.unwrap();
        assert_eq!(
            (smtp.host.as_str(), smtp.port, smtp.tls),
            ("mail.internal", 25, SmtpTls::None)
        );
        let config = load(
            None,
            &env(&[
                ("CRUCIBLE_SMTP_HOST", "smtp.example.com"),
                ("CRUCIBLE_SMTP_FROM", "crucible@example.com"),
                ("CRUCIBLE_SMTP_PASSWORD", "hunter2"),
            ]),
            &[],
        )
        .unwrap()
        .config;
        let smtp = config.smtp.unwrap();
        assert_eq!((smtp.port, smtp.tls), (587, SmtpTls::StartTls));
        assert!(!format!("{smtp:?}").contains("hunter2"));
        let errors = load(
            Some("[egress]\nmode = \"closed\"\nallow_cidrs = \"10.0.0.0/40\"\n[smtp]\nhost = \"mail\"\n"),
            &[],
            &[],
        )
        .unwrap_err();
        assert_eq!(errors.0.len(), 3, "{errors}");
        assert!(errors.0[0].contains("`egress.mode` = \"closed\" is not one of open, strict"));
        assert!(errors.0[1].contains("`egress.allow_cidrs` entry \"10.0.0.0/40\""));
        assert!(errors.0[2].contains("set `smtp.from` (CRUCIBLE_SMTP_FROM)"));
        let loaded = load(Some("[smtp]\nfrom = \"crucible@example.com\"\n"), &[], &[]).unwrap();
        assert!(
            loaded
                .warnings
                .iter()
                .any(|w| w.contains("set `smtp.host`"))
        );
    }

    #[test]
    fn the_sql_allowance_comes_from_the_file_or_the_environment_and_is_checked() {
        let file = "[sql]\nstatements_per_hour = 6000\n";
        let config = load(Some(file), &[], &[]).unwrap().config;
        assert_eq!(config.sql.statements_per_hour, 6_000);
        let lower = env(&[("CRUCIBLE_SQL_STATEMENTS_PER_HOUR", "90")]);
        let config = load(Some(file), &lower, &[]).unwrap().config;
        assert_eq!(config.sql.statements_per_hour, 90);
        let errors = load(Some("[sql]\nstatements_per_hour = 0\n"), &[], &[]).unwrap_err();
        assert!(
            errors.0[0].contains("`sql.statements_per_hour` must be at least 1"),
            "{errors}"
        );
    }

    #[test]
    fn the_mcp_rate_limit_comes_from_the_file_or_the_environment_and_is_checked() {
        let file = "[mcp]\ncalls_per_second = 10\nburst = 20\n";
        let config = load(Some(file), &[], &[]).unwrap().config;
        let limit = Mcp {
            calls_per_second: 10,
            burst: 20,
        };
        assert_eq!(config.mcp, limit);
        let off = env(&[("CRUCIBLE_MCP_CALLS_PER_SECOND", "0")]);
        let config = load(Some(file), &off, &[]).unwrap().config;
        assert_eq!(config.mcp.calls_per_second, 0);
        for (file, says) in [
            ("[mcp]\nburst = 0\n", "`mcp.burst` must be at least 1"),
            (
                "[mcp]\ncalls_per_second = -1\n",
                "`mcp.calls_per_second` = \"-1\" is not a whole number of calls",
            ),
        ] {
            let errors = load(Some(file), &[], &[]).unwrap_err();
            assert!(errors.to_string().contains(says), "{errors}");
        }
    }

    #[test]
    fn the_cpu_budget_is_optional_and_at_least_one() {
        assert_eq!(load(None, &[], &[]).unwrap().config.cpu_permits, None);
        let config = load(Some("[cpu]\npermits = 3\n"), &[], &[]).unwrap().config;
        assert_eq!(config.cpu_permits, Some(3));
        let env = env(&[("CRUCIBLE_CPU_PERMITS", "12")]);
        let loaded = load(Some("[cpu]\npermits = 3\n"), &env, &[]).unwrap();
        assert_eq!(loaded.config.cpu_permits, Some(12));
        assert!(
            loaded
                .print()
                .contains("[cpu]\npermits = 12  # env CRUCIBLE_CPU_PERMITS\n")
        );
        for (file, says) in [
            (
                "[cpu]\npermits = 0\n",
                "`cpu.permits` must be at least 1; leave it unset for the default of 8 per CPU",
            ),
            (
                "[cpu]\npermits = \"all\"\n",
                "`cpu.permits` = \"all\" is not a whole number of blocking jobs (file)",
            ),
        ] {
            let ConfigErrors(errors) = load(Some(file), &[], &[]).unwrap_err();
            assert_eq!(errors, [says]);
        }
    }

    #[test]
    fn the_clickhouse_idle_timeout_is_optional_whole_seconds() {
        let env = env(&[
            ("CRUCIBLE_CLICKHOUSE_URL", "http://clickhouse:8123"),
            ("CRUCIBLE_CLICKHOUSE_PASSWORD", "pw"),
            ("CRUCIBLE_MASTER_KEY", KEY),
        ]);
        let clickhouse = load(None, &env, &[]).unwrap().config.clickhouse.unwrap();
        assert_eq!(clickhouse.idle_timeout, None);
        let mut with = env.clone();
        with.push((
            "CRUCIBLE_CLICKHOUSE_IDLE_TIMEOUT".to_owned(),
            "0".to_owned(),
        ));
        let clickhouse = load(None, &with, &[]).unwrap().config.clickhouse.unwrap();
        assert_eq!(clickhouse.idle_timeout, Some(Duration::ZERO));
        let ConfigErrors(errors) =
            load(Some("[clickhouse]\nidle_timeout = \"soon\"\n"), &env, &[]).unwrap_err();
        assert_eq!(
            errors,
            ["`clickhouse.idle_timeout` = \"soon\" is not a whole number of seconds (file)"]
        );
    }

    #[test]
    fn sign_in_providers_come_from_the_configuration_and_say_what_is_missing() {
        let file = format!(
            "master_key = \"{KEY}\"\n\
             [server]\npublic_url = \"https://crucible.example.com\"\n\
             [postgres]\nurl = \"postgres://crucible@postgres:5432/crucible\"\n\
             [auth_github]\nclient_id = \"Iv1.abc\"\nallowed_domains = \"Example.com, @partner.example\"\n\
             [auth_oidc]\nissuer = \"https://login.example.com\"\nclient_id = \"crucible\"\n\
             client_secret_file = \"/run/secrets/clickhouse\"\ndisplay_name = \"Continue with Acme SSO\"\n\
             scopes = \"groups, offline_access\"\n"
        );
        let secret = env(&[("CRUCIBLE_AUTH_GITHUB_CLIENT_SECRET", "gh-secret")]);
        let loaded = load(Some(&file), &secret, &[]).unwrap();
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        let [github, oidc] = loaded.config.auth.providers.as_slice() else {
            panic!("{:?}", loaded.config.auth.providers);
        };
        assert_eq!(
            (
                github.kind,
                github.client_id.as_str(),
                github.client_secret.expose(),
                github.display_name.as_deref(),
                github.issuer.as_deref()
            ),
            (SignInKind::Github, "Iv1.abc", "gh-secret", None, None)
        );
        assert_eq!(github.allowed_domains, ["example.com", "partner.example"]);
        assert_eq!(
            (
                oidc.kind,
                oidc.issuer.as_deref(),
                oidc.client_secret.expose(),
                oidc.display_name.as_deref()
            ),
            (
                SignInKind::Oidc,
                Some("https://login.example.com"),
                "s3cret-from-file",
                Some("Continue with Acme SSO")
            )
        );
        assert_eq!(oidc.scopes, ["groups", "offline_access"]);
        let printed = loaded.print();
        assert!(
            printed.contains("[auth_github]\nclient_id = \"Iv1.abc\"  # file\n"),
            "{printed}"
        );
        assert!(!printed.contains("gh-secret") && !printed.contains("s3cret-from-file"));
        // Whatever is missing or malformed is named with the setting that fixes it.
        let bare = env(&[
            ("CRUCIBLE_AUTH_GITHUB_ALLOWED_DOMAINS", "example.com"),
            ("CRUCIBLE_AUTH_GOOGLE_CLIENT_ID", "google-client"),
            (
                "CRUCIBLE_AUTH_GOOGLE_ALLOWED_DOMAINS",
                "example.com, someone@example.com",
            ),
            ("CRUCIBLE_AUTH_OIDC_CLIENT_ID", "crucible"),
            ("CRUCIBLE_AUTH_OIDC_CLIENT_SECRET", "oidc-secret"),
            ("CRUCIBLE_AUTH_OIDC_ISSUER", "login.example.com"),
        ]);
        let ConfigErrors(errors) = load(None, &bare, &[]).unwrap_err();
        let expected = [
            "`auth_github.allowed_domains` is set but `auth_github.client_id` is not, so GitHub sign-in stays off; set \
             `auth_github.client_id` (CRUCIBLE_AUTH_GITHUB_CLIENT_ID) or remove `auth_github.allowed_domains`",
            "`auth_google.client_id` needs the client secret: set `auth_google.client_secret_file` \
             (CRUCIBLE_AUTH_GOOGLE_CLIENT_SECRET_FILE) or CRUCIBLE_AUTH_GOOGLE_CLIENT_SECRET",
            "`auth_google.allowed_domains` entry \"someone@example.com\" must be a bare domain such as example.com",
            "`auth_google.client_id` needs the control plane, which keeps sign-in: set `postgres.url` \
             (CRUCIBLE_POSTGRES_URL), or remove the [auth_google] settings",
            "`auth_oidc.issuer` = \"login.example.com\" must start with https:// or http://",
            "`auth_oidc.client_id` needs the control plane, which keeps sign-in: set `postgres.url` \
             (CRUCIBLE_POSTGRES_URL), or remove the [auth_oidc] settings",
        ];
        assert_eq!(errors, expected);
        assert!(!errors.concat().contains("oidc-secret"));
        // OpenID Connect needs its issuer; without server.public_url a warning says the sign-in page does not offer
        // the providers yet.
        let file = format!(
            "master_key = \"{KEY}\"\n[postgres]\nurl = \"postgres://crucible@postgres:5432/crucible\"\n\
             [auth_oidc]\nclient_id = \"crucible\"\nclient_secret = \"s\"\n"
        );
        let ConfigErrors(errors) = load(Some(&file), &[], &[]).unwrap_err();
        assert_eq!(
            errors,
            [
                "`auth_oidc.client_id` needs the provider's issuer: set `auth_oidc.issuer` (CRUCIBLE_AUTH_OIDC_ISSUER), \
              for example https://login.example.com"
            ]
        );
        let file = format!("{file}issuer = \"https://login.example.com\"\n");
        let loaded = load(Some(&file), &[], &[]).unwrap();
        assert_eq!(
            loaded.warnings,
            [
                "the sign-in providers of the configuration are offered only once `server.public_url` is set \
              (CRUCIBLE_SERVER_PUBLIC_URL): their callback URLs are built from it"
            ]
        );
    }

    #[test]
    fn passwords_in_urls_are_redacted() {
        assert_eq!(
            without_password("postgres://u:p@h:5432/d"),
            "postgres://u:<redacted>@h:5432/d"
        );
        assert_eq!(
            without_password("postgres://u:p@ss@h/d?x=a@b"),
            "postgres://u:<redacted>@h/d?x=a@b"
        );
        assert_eq!(without_password("postgres://u@h/d"), "postgres://u@h/d");
        assert_eq!(without_password("postgres://h/d"), "postgres://h/d");
        assert_eq!(without_password("not a url"), "<redacted>");
    }

    proptest! {
        /// For any subset of layers that set a key, the highest layer wins: flag, then environment, then
        /// file, then the default.
        #[test]
        fn the_highest_layer_wins(in_file: bool, in_env: bool, in_flag: bool, ports in proptest::array::uniform3(1u16..u16::MAX)) {
            let [file_port, env_port, flag_port] = ports;
            let file = in_file.then(|| format!("[server]\nlisten = \"10.0.0.1:{file_port}\"\n"));
            let env = if in_env { env(&[("CRUCIBLE_SERVER_LISTEN", &format!("10.0.0.2:{env_port}"))]) } else { Vec::new() };
            let flags: Vec<(&str, String)> = if in_flag { vec![("server.listen", format!("10.0.0.3:{flag_port}"))] } else { Vec::new() };
            let loaded = load(file.as_deref(), &env, &flags).unwrap();
            let expected = if in_flag {
                format!("10.0.0.3:{flag_port}")
            } else if in_env {
                format!("10.0.0.2:{env_port}")
            } else if in_file {
                format!("10.0.0.1:{file_port}")
            } else {
                "0.0.0.0:8080".to_owned()
            };
            prop_assert_eq!(loaded.config.server.listen.to_string(), expected);
        }

        /// No input makes the loader panic, and an error never quotes the master key it was given.
        #[test]
        fn arbitrary_files_never_panic_or_leak(text in "\\PC{0,200}", key in "[a-z0-9]{12,40}") {
            let env = env(&[("CRUCIBLE_MASTER_KEY", &key)]);
            if let Err(error) = load(Some(&text), &env, &[]) {
                prop_assert!(!error.to_string().contains(&key));
            }
        }
    }
}
