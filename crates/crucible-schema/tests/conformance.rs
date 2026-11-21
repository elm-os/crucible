//! Conformance against real ClickHouse (contract sections 6, 7.3 and 5.2).
//!
//! `scripts/conformance.sh` applies the committed golden statements to ClickHouse 26.3, 26.8 and latest in Docker
//! and commits what the servers answer under `tests/conformance/expected/<tag>/`. These tests compare those
//! answers with the `TableDef`s, the routers and the access scheme, so CI needs no ClickHouse.

// Test code throughout: lets the helpers below use `unwrap`, `expect` and `panic!` under the workspace lints.
#![cfg(test)]

use crucible_records::{PartitionId, TableId, Timestamp};
use crucible_schema::{Granularity, Ident, ServiceIdentity, TABLES, Variant, create_table, route};

struct Expected {
    tag: &'static str,
    version: &'static str,
    single_day: &'static str,
    single_week: &'static str,
    partition_ids: &'static str,
    service_identities: &'static str,
    checks: &'static str,
    cluster_day: &'static str,
    cluster_identities: &'static str,
}

macro_rules! expected {
    ($tag:literal) => {
        Expected {
            tag: $tag,
            version: include_str!(concat!("conformance/expected/", $tag, "/version.txt")),
            single_day: include_str!(concat!("conformance/expected/", $tag, "/single_day.sql")),
            single_week: include_str!(concat!("conformance/expected/", $tag, "/single_week.sql")),
            partition_ids: include_str!(concat!(
                "conformance/expected/",
                $tag,
                "/partition_ids.tsv"
            )),
            service_identities: include_str!(concat!(
                "conformance/expected/",
                $tag,
                "/service_identities.tsv"
            )),
            checks: include_str!(concat!("conformance/expected/", $tag, "/checks.tsv")),
            cluster_day: include_str!(concat!("conformance/expected/", $tag, "/cluster_day.sql")),
            cluster_identities: include_str!(concat!(
                "conformance/expected/",
                $tag,
                "/cluster_identities.tsv"
            )),
        }
    };
}

const VERSIONS: [Expected; 3] = [expected!("26.3"), expected!("26.8"), expected!("latest")];

const SWITCHES: [&str; 2] = ["permissive", "strict"];
const CONTROL: &str = "conformance_control";

/// Drops what `SHOW CREATE TABLE` never prints and the layout differences: line breaks, the indentation inside
/// multi-line `Tuple(...)` types, `IF NOT EXISTS` and `ON CLUSTER`.
fn normalize(sql: &str) -> String {
    sql.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace("( ", "(")
        .replace(" IF NOT EXISTS", "")
        .replace(" ON CLUSTER crucible", "")
}

/// Splits a `-- table: <name>` sectioned file into (name, statement) pairs.
fn sections(file: &'static str) -> Vec<(&'static str, &'static str)> {
    file.split("-- table: ")
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.split_once('\n').expect("a table name line"))
        .map(|(name, sql)| (name.trim(), sql))
        .collect()
}

fn assert_show_create_matches(
    file: &'static str,
    variant: &Variant,
    granularity: Granularity,
    context: &str,
) {
    let shown = sections(file);
    assert_eq!(shown.len(), TABLES.len(), "{context}");
    for (def, (name, sql)) in TABLES.iter().zip(shown) {
        assert_eq!(name, def.name(), "{context}");
        // ClickHouse expands the {database} and {table} macros of a replication path when it creates the table
        // and keeps {shard} and {replica}.
        let rendered = normalize(&create_table(def, variant, granularity))
            .replace("{database}", "crucible")
            .replace("{table}", name);
        assert!(normalize(sql) == rendered, "{context}: {name}");
    }
}

fn rows(file: &'static str) -> Vec<Vec<&'static str>> {
    file.lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('\t').collect())
        .collect()
}

#[test]
fn the_three_supported_versions_were_recorded() {
    for expected in &VERSIONS {
        let version = expected.version.trim();
        let mut parts = version
            .split('.')
            .map(|p| p.parse::<u32>().expect("a number"));
        let (major, minor) = (parts.next().expect("major"), parts.next().expect("minor"));
        match expected.tag {
            "latest" => assert!((major, minor) >= (26, 8), "{version}"),
            tag => assert_eq!(format!("{major}.{minor}"), tag),
        }
    }
}

#[test]
fn show_create_table_matches_the_descriptor_on_every_version() {
    for expected in &VERSIONS {
        for (file, granularity) in [
            (expected.single_day, Granularity::Day),
            (expected.single_week, Granularity::Week),
        ] {
            let context = format!("{} {granularity:?}", expected.tag);
            assert_show_create_matches(file, &Variant::SingleNode, granularity, &context);
        }
    }
}

/// The clustered run: one shard, two replicas and Keeper, under the strict row-policy switch. The statements
/// are applied `ON CLUSTER` on the first replica; the table definitions and the known rows are read back on
/// the second.
#[test]
fn the_clustered_rendering_matches_on_the_second_replica() {
    let cluster = Ident::new("crucible").expect("a plain identifier");
    let variant = Variant::Clustered { cluster };
    assert!(
        variant
            .ddl_settings()
            .contains(&("distributed_ddl_entry_format_version", "8")),
        "the harness sends this setting; see scripts/conformance.sh"
    );
    for expected in &VERSIONS {
        let context = format!("{} cluster", expected.tag);
        assert_show_create_matches(expected.cluster_day, &variant, Granularity::Day, &context);
        let recorded = rows(expected.cluster_identities);
        assert_eq!(recorded.len(), ServiceIdentity::ALL.len() * TABLES.len());
        let mut recorded = recorded.into_iter();
        for identity in ServiceIdentity::ALL {
            for def in &TABLES {
                let denied = identity == ServiceIdentity::Ingest && def.id == TableId::SqlProbe;
                let want = if denied { "error 497" } else { "1" };
                assert_eq!(
                    recorded.next().expect("a row"),
                    [identity.name(), def.name(), want],
                    "{context}"
                );
            }
        }
    }
}

#[test]
fn partition_ids_match_the_router_on_every_version() {
    for expected in &VERSIONS {
        let recorded = rows(expected.partition_ids);
        // Ten sample instants in span_core and fact_tags per granularity; fewer distinct rows survive in the
        // aggregating tables.
        assert!(recorded.len() >= 50, "{}", expected.tag);
        for row in recorded {
            let [table, granularity, micros, id] = row[..] else {
                panic!("four fields: {row:?}");
            };
            let table = TableId::from_name(table).expect("a table");
            let granularity = match granularity {
                "day" => Granularity::Day,
                "week" => Granularity::Week,
                other => panic!("granularity {other}"),
            };
            let time = Timestamp::from_micros(micros.parse().expect("microseconds"));
            assert_eq!(
                route(table, granularity, time),
                Ok(PartitionId::parse(id).expect("a partition id")),
                "{} {table:?} {granularity:?} {time}",
                expected.tag
            );
        }
    }
}

/// The conformance row "service identities see all rows after access migrations": after access step A1 every
/// service identity counts the known row in every base table under both values of
/// `users_without_row_policies_can_read_rows`. The control user, which has a grant but no row policy, shows that
/// the strict server really was strict.
#[test]
fn service_identities_see_all_rows_after_access_migrations() {
    for expected in &VERSIONS {
        let recorded = rows(expected.service_identities);
        assert_eq!(
            recorded.len(),
            SWITCHES.len() * (ServiceIdentity::ALL.len() + 1) * TABLES.len(),
            "{}",
            expected.tag
        );
        let mut recorded = recorded.into_iter();
        for switch in SWITCHES {
            let identities = ServiceIdentity::ALL.map(|i| (i.name(), Some(i)));
            for (name, identity) in identities.into_iter().chain([(CONTROL, None)]) {
                for def in &TABLES {
                    let want = match identity {
                        // crucible_ingest has no grant on sql_probe (contract section 7.1).
                        Some(ServiceIdentity::Ingest) if def.id == TableId::SqlProbe => "error 497",
                        Some(_) => "1",
                        None if switch == "strict" => "0",
                        None => "1",
                    };
                    assert_eq!(
                        recorded.next().expect("a row"),
                        [switch, name, def.name(), want],
                        "{}",
                        expected.tag
                    );
                }
            }
        }
    }
}

#[test]
fn access_checks_hold_under_both_switch_values() {
    const DENIED: &str = "error 497";
    const CONSTRAINT: &str = "error 452";
    let want = [
        ("service_policies_present", "19"),
        ("ingest_probe_plain", "1"),
        ("ingest_probe_marker", "1"),
        ("ingest_reads_no_other_column", DENIED),
        ("ingest_execution_time_ceiling", CONSTRAINT),
        ("read_ui_within_ceiling", "1"),
        ("read_ui_execution_time_ceiling", CONSTRAINT),
        ("read_ui_unlimited_time_refused", CONSTRAINT),
        ("read_ui_cannot_insert", DENIED),
        ("read_agent_within_ceiling", "1"),
        ("read_agent_rows_ceiling", CONSTRAINT),
        ("read_agent_concurrency_is_const", CONSTRAINT),
        ("maint_reads_mutations", "1"),
        ("maint_reads_engines", "MergeTree"),
        ("maint_reads_server_settings", "1"),
        ("maint_lightweight_delete", "ok"),
        ("maint_rows_after_delete", "0"),
        ("maint_apply_deleted_mask", "ok"),
        ("maint_drop_partition", "ok"),
        ("maint_rows_after_drop", "0"),
        ("maint_cannot_create", DENIED),
    ];
    assert_eq!(TABLES.len(), 19);
    for expected in &VERSIONS {
        let mut recorded = rows(expected.checks).into_iter();
        for switch in SWITCHES {
            for (check, result) in want {
                assert_eq!(
                    recorded.next().expect("a row"),
                    [switch, check, result],
                    "{}",
                    expected.tag
                );
            }
        }
        assert_eq!(recorded.next(), None);
    }
}
