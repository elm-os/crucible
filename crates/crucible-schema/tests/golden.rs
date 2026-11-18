//! Golden snapshots of every rendered statement: each table for each variant and granularity, and the migration
//! steps. The live conformance harness (`scripts/conformance.sh`) applies the migration snapshots to real
//! ClickHouse servers, so what is pinned here is what is checked there.

// Test code throughout: lets the helpers below use `unwrap`, `expect` and `panic!` under the workspace lints.
#![cfg(test)]

use crucible_schema::{
    AccessParams, Granularity, Ident, MigrationStep, PasswordHash, ProfileCaps, TABLES, Variant,
    access_service, create_table, schema_base, schema_span_usage_aggregate, service_policy_check,
};

fn variants() -> [(&'static str, Variant); 3] {
    let cluster = Ident::new("crucible").expect("a plain identifier");
    [
        ("single", Variant::SingleNode),
        ("cluster", Variant::Clustered { cluster }),
        ("cloud", Variant::Cloud),
    ]
}

const GRANULARITIES: [(&str, Granularity); 2] =
    [("day", Granularity::Day), ("week", Granularity::Week)];

/// The digests are the SHA-256 of `conformance-<identity>`, the passwords the live harness logs in with. The
/// ceilings are the class maxima of contract section 8.4.
fn params() -> AccessParams {
    let hash = |hex| PasswordHash::new(hex).expect("a digest");
    AccessParams {
        admin_user: Ident::new("crucible_admin").expect("a plain identifier"),
        ingest: hash("83295f7e2b82d963d8371e5958a3e05668348542af0fbcfe1aca55571add4956"),
        read_ui: hash("10048572f301b66b8003db9cd3ddd62a04a66a1e886fbf812c88cb568cdabf3a"),
        read_agent: hash("cf70a3095e02a18eba9677a6d7a97b4ee1595ed64aa58a9c5b2b893e6fe837aa"),
        maint: hash("256f3bb0b9ea13afea3f7a133dc363b24c59ff7aaec95dcd0021357e02fddf73"),
        read_ui_caps: ProfileCaps {
            max_execution_time_secs: 300,
            max_memory_usage: 8 << 30,
            max_rows_to_read: None,
            max_threads: 8,
        },
        read_agent_caps: ProfileCaps {
            max_execution_time_secs: 5,
            max_memory_usage: 1 << 30,
            max_rows_to_read: Some(100_000_000),
            max_threads: 4,
        },
    }
}

fn script(step: &MigrationStep) -> String {
    let mut sql = step.statements.join(";\n\n");
    sql.push_str(";\n");
    sql
}

#[test]
fn ddl_for_every_table_variant_and_granularity() {
    let mut count = 0;
    for (variant_name, variant) in variants() {
        for (granularity_name, granularity) in GRANULARITIES {
            for def in &TABLES {
                let sql = create_table(def, &variant, granularity);
                insta::assert_snapshot!(
                    format!("ddl__{variant_name}__{granularity_name}__{}", def.name()),
                    sql
                );
                count += 1;
            }
        }
    }
    assert_eq!(count, 108);
}

#[test]
fn migration_steps() {
    for (variant_name, variant) in variants() {
        for (granularity_name, granularity) in GRANULARITIES {
            let step = schema_base(&variant, granularity);
            insta::assert_snapshot!(
                format!("step__{variant_name}__{granularity_name}__{}", step.id),
                script(&step)
            );
        }
        let step = access_service(&variant, &params());
        insta::assert_snapshot!(format!("step__{variant_name}__{}", step.id), script(&step));
        let step = schema_span_usage_aggregate(&variant);
        insta::assert_snapshot!(format!("step__{variant_name}__{}", step.id), script(&step));
    }
    insta::assert_snapshot!(
        "service_policy_check",
        service_policy_check(&params().admin_user)
    );
}
