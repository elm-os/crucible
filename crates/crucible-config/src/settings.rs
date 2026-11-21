//! The list of settings.

/// One setting.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Setting {
    /// The dotted key: the section and the name in the TOML file, or just the name for a top-level key.
    pub key: &'static str,
    /// The value when no layer sets it.
    pub default: Option<&'static str>,
    /// Whether the value is a secret. A secret `x` has a twin `x_file` that names a file holding it.
    pub secret: bool,
    /// What the setting is, for `crucible config print`.
    pub about: &'static str,
}

const fn plain(key: &'static str, default: Option<&'static str>, about: &'static str) -> Setting {
    Setting {
        key,
        default,
        secret: false,
        about,
    }
}

const fn secret(key: &'static str, about: &'static str) -> Setting {
    Setting {
        key,
        default: None,
        secret: true,
        about,
    }
}

/// Every setting, in the order `crucible config print` shows them. Top-level keys come first, as TOML needs.
pub const SETTINGS: &[Setting] = &[
    secret(
        "master_key",
        "the master key, 64 hexadecimal characters; the service passwords derive from it",
    ),
    plain("master_key_file", None, "a file that holds the master key"),
    plain(
        "server.listen",
        Some("0.0.0.0:8080"),
        "address of the main listener (API, UI, ingest and health paths)",
    ),
    plain(
        "server.ingest_listen",
        Some("0.0.0.0:4318"),
        "address of the OTLP/HTTP ingest listener",
    ),
    plain(
        "server.grpc_listen",
        Some("0.0.0.0:4317"),
        "address of the OTLP/gRPC ingest listener",
    ),
    plain(
        "server.metrics_listen",
        Some("127.0.0.1:9464"),
        "address of the Prometheus metrics listener",
    ),
    plain(
        "server.ui_dir",
        None,
        "serve the UI from this directory instead of the one compiled in",
    ),
    plain(
        "server.keys_file",
        None,
        "TOML file of the project keys, for a server without Postgres (with Postgres the keys live there)",
    ),
    plain(
        "server.public_url",
        None,
        "the URL people reach the server at, for example https://crucible.example.com; provider sign-in needs \
         it for its callback URL, and an https URL makes the session cookie Secure",
    ),
    plain(
        "server.allowed_origins",
        None,
        "comma-separated origins besides the server's own that may send cookie-authenticated writes",
    ),
    plain(
        "data.dir",
        Some("data"),
        "directory that holds the wal, quarantine and exports roots",
    ),
    plain(
        "data.wal_backend",
        Some("auto"),
        "disk backend of the WAL: auto, portable or uring",
    ),
    plain(
        "recent.max_bytes",
        None,
        "bound on the memory of the recent-span index, in bytes (default: 10% of the container's memory limit, \
         else of RAM, at most 1 GiB)",
    ),
    plain(
        "cpu.permits",
        None,
        "blocking jobs (inflate, scan, compression) that run at once, shared out between the stages that run \
         them; the runtime keeps 8 threads more for disk reads, name lookups and password hashing (default: 8 \
         per CPU the process may use)",
    ),
    plain(
        "mcp.calls_per_second",
        Some("50"),
        "MCP tool calls one key may make per second, sustained; 0 turns the limit off",
    ),
    plain(
        "mcp.burst",
        Some("200"),
        "MCP tool calls one key may make at once after a quiet spell",
    ),
    plain(
        "sql.statements_per_hour",
        Some("1800"),
        "statements one project's SQL workspace may send to ClickHouse per hour; a run is three",
    ),
    plain(
        "metrics.queries_per_project",
        Some("4"),
        "metrics queries one project runs at once on this server (a dashboard sends one per widget); the others \
         wait their turn within their deadline",
    ),
    plain(
        "clickhouse.url",
        None,
        "HTTP endpoint of the ClickHouse telemetry store; without it the server only ingests",
    ),
    plain(
        "clickhouse.user",
        Some("crucible_admin"),
        "the ClickHouse admin user, which runs the migrations",
    ),
    secret(
        "clickhouse.password",
        "the ClickHouse admin user's password",
    ),
    plain(
        "clickhouse.password_file",
        None,
        "a file that holds the ClickHouse admin user's password",
    ),
    plain(
        "clickhouse.idle_timeout",
        None,
        "seconds an idle connection to ClickHouse is kept for the next statement; set it below the idle timeout \
         of a balancer in front of the store (default: a margin below the server's keep_alive_timeout)",
    ),
    plain(
        "postgres.url",
        None,
        "postgres://user@host:port/database of the control plane; add ?sslmode=require for TLS",
    ),
    secret(
        "postgres.password",
        "the Postgres password, when the URL carries none",
    ),
    plain(
        "postgres.password_file",
        None,
        "a file that holds the Postgres password",
    ),
    plain(
        "postgres.pool_size",
        Some("8"),
        "connections of the main Postgres pool (a reserved pool of 2 is added)",
    ),
    plain(
        "security.require_https",
        Some("false"),
        "refuse sign-in unless server.public_url is an https URL (plain HTTP works by default)",
    ),
    plain(
        "auth.pg_outage_mode",
        Some("fail_open"),
        "API keys during a Postgres outage: fail_open keeps known keys working for ingest, fail_closed refuses \
         every secret key after 5 s without a successful key-feed poll",
    ),
    plain(
        "auth.fail_open_max",
        None,
        "seconds after which a fail_open outage behaves as fail_closed (default: unlimited)",
    ),
    plain(
        "auth.session_outage_grace",
        Some("600"),
        "seconds that cached sessions stay valid during a Postgres outage; 0 refuses them at once",
    ),
    plain(
        "egress.mode",
        Some("open"),
        "which notification targets may be called: open allows any network except cloud metadata addresses, \
         strict allows public https targets only",
    ),
    plain(
        "egress.allow_cidrs",
        None,
        "comma-separated address ranges that egress.mode = strict lets through, for example 10.20.0.0/16",
    ),
    plain(
        "smtp.host",
        None,
        "host of the mail server for email channels; without it email channels cannot be created",
    ),
    plain(
        "smtp.port",
        None,
        "port of the mail server (default: 587, or 465 with smtp.tls = tls, or 25 with smtp.tls = none)",
    ),
    plain(
        "smtp.tls",
        Some("starttls"),
        "how the connection to the mail server is secured: starttls, tls, or none",
    ),
    plain("smtp.username", None, "the user name at the mail server"),
    secret("smtp.password", "the password at the mail server"),
    plain(
        "smtp.password_file",
        None,
        "a file that holds the password at the mail server",
    ),
    plain(
        "smtp.from",
        None,
        "the sender address of notification email, for example crucible@example.com",
    ),
    plain(
        "object_store.endpoint",
        None,
        "the S3-compatible endpoint this server reaches, for example http://versitygw:7070; with postgres.url \
         it turns media on",
    ),
    plain(
        "object_store.public_endpoint",
        None,
        "the endpoint clients and browsers reach, which presigned URLs name (default: object_store.endpoint)",
    ),
    plain(
        "object_store.region",
        Some("us-east-1"),
        "the region the request signatures name",
    ),
    plain(
        "object_store.bucket",
        None,
        "the bucket that holds media and export files",
    ),
    plain(
        "object_store.access_key",
        None,
        "the access key id of the bucket",
    ),
    secret(
        "object_store.secret_key",
        "the secret access key of the bucket",
    ),
    plain(
        "object_store.secret_key_file",
        None,
        "a file that holds the secret access key",
    ),
    plain(
        "object_store.url_style",
        Some("path"),
        "how URLs name the bucket: path (host/bucket) or virtual_host (bucket.host)",
    ),
    plain(
        "object_store.prefix",
        Some("media"),
        "the key prefix under which media objects and export files are stored",
    ),
    plain(
        "media.max_bytes",
        Some("67108864"),
        "the largest media object, in bytes (default 64 MiB)",
    ),
];

/// The setting with this dotted key.
pub(crate) fn find(key: &str) -> Option<&'static Setting> {
    SETTINGS.iter().find(|setting| setting.key == key)
}

impl Setting {
    /// The environment variable of the setting: `CRUCIBLE_` and the key in upper case with `_` for `.`.
    pub fn variable(&self) -> String {
        format!("CRUCIBLE_{}", self.key.replace('.', "_").to_uppercase())
    }

    /// The section of the key, empty for a top-level key.
    pub(crate) fn section(&self) -> &'static str {
        self.key.split_once('.').map_or("", |(section, _)| section)
    }

    /// The name of the key inside its section.
    pub(crate) fn name(&self) -> &'static str {
        self.key.split_once('.').map_or(self.key, |(_, name)| name)
    }
}

/// The known text nearest to `given`, when it is near enough to be a typing mistake.
pub(crate) fn nearest<'a>(given: &str, known: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    known
        .map(|known| (distance(given, known), known))
        .filter(|(distance, known)| *distance <= known.len().min(given.len()) / 3)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, known)| known)
}

/// The edit distance between two texts.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut previous = i;
        let mut left = i + 1;
        for (cell, cb) in row.iter_mut().skip(1).zip(&b) {
            let next = (previous + usize::from(ca != *cb))
                .min(*cell + 1)
                .min(left + 1);
            previous = *cell;
            *cell = next;
            left = next;
        }
        if let Some(first) = row.first_mut() {
            *first = i + 1;
        }
    }
    row.last().copied().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_variables_are_unique_and_secrets_have_file_twins() {
        for (at, setting) in SETTINGS.iter().enumerate() {
            for other in &SETTINGS[at + 1..] {
                assert_ne!(setting.key, other.key);
                assert_ne!(setting.variable(), other.variable());
            }
            if setting.secret {
                let twin = find(&format!("{}_file", setting.key)).expect("a _file twin");
                assert!(!twin.secret && twin.default.is_none());
            }
        }
    }

    #[test]
    fn the_variables_of_the_first_serve_flags_keep_their_names() {
        let variable = |key| find(key).unwrap().variable();
        assert_eq!(variable("clickhouse.url"), "CRUCIBLE_CLICKHOUSE_URL");
        assert_eq!(variable("clickhouse.user"), "CRUCIBLE_CLICKHOUSE_USER");
        assert_eq!(
            variable("clickhouse.password"),
            "CRUCIBLE_CLICKHOUSE_PASSWORD"
        );
        assert_eq!(
            variable("clickhouse.password_file"),
            "CRUCIBLE_CLICKHOUSE_PASSWORD_FILE"
        );
        assert_eq!(variable("master_key"), "CRUCIBLE_MASTER_KEY");
        assert_eq!(variable("master_key_file"), "CRUCIBLE_MASTER_KEY_FILE");
    }

    #[test]
    fn top_level_keys_come_before_the_sections() {
        let first = SETTINGS
            .iter()
            .position(|setting| !setting.section().is_empty())
            .unwrap();
        assert!(
            SETTINGS[first..]
                .iter()
                .all(|setting| !setting.section().is_empty())
        );
    }

    #[test]
    fn nearest_finds_typing_mistakes_only() {
        let keys = || SETTINGS.iter().map(|setting| setting.key);
        assert_eq!(nearest("server.lisen", keys()), Some("server.listen"));
        assert_eq!(nearest("clickhouse.uri", keys()), Some("clickhouse.url"));
        assert_eq!(nearest("mail.server", keys()), None);
        assert_eq!(distance("kitten", "sitting"), 3);
        assert_eq!(distance("", "abc"), 3);
        assert_eq!(distance("abc", ""), 3);
        assert_eq!(distance("same", "same"), 0);
    }
}
