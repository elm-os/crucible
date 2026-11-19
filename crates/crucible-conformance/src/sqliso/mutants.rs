//! Layer D: broken deployments (contract 7.5). Each mutant breaks the access scheme the way an operator or a
//! bad migration could; the suite's detectors must fail on every one of them, or the suite proves nothing.
//!
//! The detectors are the rows of layer A that judge isolation ([`layer_a::detectors`]). For each mutant the
//! row records which detectors caught it and whether the runtime self-probe, which is all a running server
//! has, was among them.
//!
//! The sixth mutant of the contract, the server switch flipped, cannot be applied from inside: the switch is
//! server configuration. `scripts/sqliso.sh` and the `sqliso` workflow restart the server with the other value
//! and run the suite with the original declaration, which must fail on row W0. Under either value the suite
//! itself passes, which is rule 7.2: behaviour is identical under both.

use anyhow::{Result, bail};
use crucible_schema::{
    SQL_PROFILE, SQL_READER_ROLE, project_policy_name, static_policy, workspace_user,
};

use super::{FORMS, Iso, layer_a};

struct Mutant {
    name: &'static str,
    /// Statements that break the deployment, as the admin.
    apply: Vec<String>,
    /// Statements that repair it.
    revert: Vec<String>,
}

fn mutants(iso: &Iso) -> Vec<Mutant> {
    let variant = &iso.ctx.variant;
    let table_id = crucible_records::TableId::SpanCore;
    let a = workspace_user(iso.a);
    let profile = iso.profile;
    let const_clause = profile
        .settings()
        .iter()
        .map(|(name, value)| format!("{name} = {value} CONST"))
        .collect::<Vec<_>>()
        .join(", ");
    let restore_profile = format!("ALTER SETTINGS PROFILE {SQL_PROFILE} SETTINGS {const_clause}");
    // Dropping every policy of both layers on one table: the table is then unguarded for the role.
    let mut drop_policies = Vec::new();
    let mut restore_policies = Vec::new();
    for form in FORMS {
        drop_policies.push(format!(
            "DROP ROW POLICY IF EXISTS {} ON crucible.span_core",
            crucible_schema::static_policy_name(form)
        ));
        restore_policies.push(static_policy(table_id, form, variant));
        for project in [iso.a, iso.b] {
            drop_policies.push(format!(
                "DROP ROW POLICY IF EXISTS {} ON crucible.span_core",
                project_policy_name(project, form)
            ));
        }
    }
    vec![
        Mutant {
            name: "D1 mutant: policies dropped on one table",
            apply: drop_policies,
            // The projects' own policies come back through provisioning, below.
            revert: restore_policies,
        },
        Mutant {
            name: "D2 mutant: one restrictive policy dropped",
            apply: vec![format!(
                "DROP ROW POLICY IF EXISTS {} ON crucible.span_core",
                crucible_schema::static_policy_name(FORMS[1])
            )],
            revert: vec![static_policy(table_id, FORMS[1], variant)],
        },
        Mutant {
            name: "D3 mutant: a source granted",
            // A table function over a source needs the source grant and the right to create a temporary table.
            apply: vec![
                format!("GRANT READ ON URL TO {SQL_READER_ROLE}"),
                format!("GRANT CREATE TEMPORARY TABLE ON *.* TO {SQL_READER_ROLE}"),
            ],
            revert: vec![
                format!("REVOKE READ ON URL FROM {SQL_READER_ROLE}"),
                format!("REVOKE CREATE TEMPORARY TABLE ON *.* FROM {SQL_READER_ROLE}"),
            ],
        },
        Mutant {
            name: "D4 mutant: readonly = 2",
            apply: vec![format!(
                "ALTER SETTINGS PROFILE {SQL_PROFILE} SETTINGS readonly = 2 CONST"
            )],
            revert: vec![restore_profile.clone()],
        },
        Mutant {
            name: "D5 mutant: a system log table granted",
            apply: vec![
                "SYSTEM FLUSH LOGS".to_owned(),
                format!("GRANT SELECT ON system.query_log TO {SQL_READER_ROLE}"),
            ],
            revert: vec![format!(
                "REVOKE SELECT ON system.query_log FROM {SQL_READER_ROLE}"
            )],
        },
        Mutant {
            name: "D6 mutant: profile missing",
            apply: vec![
                format!("ALTER USER {a} SETTINGS PROFILE 'default'"),
                format!(
                    "ALTER USER {} SETTINGS PROFILE 'default'",
                    workspace_user(crucible_schema::PROBE_PROJECTS[0])
                ),
            ],
            revert: vec![
                format!("ALTER USER {a} SETTINGS PROFILE '{SQL_PROFILE}'"),
                format!(
                    "ALTER USER {} SETTINGS PROFILE '{SQL_PROFILE}'",
                    workspace_user(crucible_schema::PROBE_PROJECTS[0])
                ),
            ],
        },
        Mutant {
            name: "D7 mutant: base table granted in full",
            apply: vec![format!("GRANT SELECT ON crucible.* TO {SQL_READER_ROLE}")],
            // The column grants come back with access step A2, below.
            revert: vec![format!(
                "REVOKE SELECT ON crucible.* FROM {SQL_READER_ROLE}"
            )],
        },
    ]
}

/// Applies each mutant, requires a detector to fail, and repairs the deployment. The last row runs the
/// detectors on the repaired deployment: they must all pass again.
pub(crate) async fn run(iso: &Iso) -> Vec<(&'static str, Result<String>)> {
    let mut rows = Vec::new();
    for mutant in mutants(iso) {
        rows.push((mutant.name, one(iso, &mutant).await));
    }
    rows.push((
        "Layer D: detectors pass after repair",
        async {
            let failed: Vec<String> = layer_a::detectors(iso)
                .await
                .into_iter()
                .filter_map(|(name, result)| result.err().map(|e| format!("{name}: {e:#}")))
                .collect();
            if !failed.is_empty() {
                bail!("the repaired deployment still fails: {}", failed.join("; "));
            }
            Ok("every detector passes on the repaired deployment".to_owned())
        }
        .await,
    ));
    rows
}

async fn one(iso: &Iso, mutant: &Mutant) -> Result<String> {
    for sql in &mutant.apply {
        iso.ctx.admin(sql.clone()).await?;
    }
    let results = layer_a::detectors(iso).await;
    // Repair before judging, so a failed row leaves a clean deployment for the next mutant.
    for sql in &mutant.revert {
        iso.ctx.admin(sql.clone()).await?;
    }
    // Access step A2 and provisioning are idempotent: they restore whatever grant or policy the revert left out.
    let params = iso
        .sql
        .workspace_params(iso.profile)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let workspace = crucible_schema::access_workspace(&iso.ctx.variant, &params);
    crucible_env::TelemetryStore::apply_migration(&iso.ctx.store, &workspace).await?;
    for project in [iso.a, iso.b] {
        iso.provision(project).await?;
    }
    // A2 restores the quota's limits; the run keeps them lifted.
    layer_a::lift_quota(iso).await?;
    let caught: Vec<&str> = results
        .iter()
        .filter(|(_, result)| result.is_err())
        .map(|(name, _)| *name)
        .collect();
    if caught.is_empty() {
        bail!("no detector failed on this broken deployment: the suite would pass it");
    }
    let leak = results
        .iter()
        .filter_map(|(_, result)| result.as_ref().err())
        .any(|error| format!("{error:#}").contains("LEAK"));
    // With every policy of a table gone, a permissive server shows project A every row of that table. The
    // canary oracle is what must say so; under the strict switch project A reads nothing instead.
    let must_leak = mutant.name.starts_with("D7")
        || (mutant.name.starts_with("D1") && iso.switch == super::Switch::Permissive);
    if must_leak && !leak {
        bail!(
            "this deployment shows project A another project's rows, and the canary oracle did not report it \
             (caught only by {})",
            caught.join(", ")
        );
    }
    let probe = caught.contains(&"Runtime self-probe (7.5)");
    Ok(format!(
        "caught by {} ({}the runtime self-probe{})",
        caught.join(", "),
        if probe { "including " } else { "not by " },
        if leak {
            "; a canary of another project was read"
        } else {
            ""
        }
    ))
}
