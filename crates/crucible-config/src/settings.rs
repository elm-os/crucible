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
        "TOML file of the project keys that ingest accepts",
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
        "bound on the memory of the recent-span index, in bytes (default: 10% of RAM, at most 1 GiB)",
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
        assert_eq!(nearest("smtp.host", keys()), None);
        assert_eq!(distance("kitten", "sitting"), 3);
        assert_eq!(distance("", "abc"), 3);
        assert_eq!(distance("abc", ""), 3);
        assert_eq!(distance("same", "same"), 0);
    }
}
