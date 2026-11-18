//! The startup path of contract 5.8, end to end: detection before and after migration, the deduplication
//! self-test and the service-identity visibility check.

use anyhow::{Context as _, Result, bail};
use crucible_env::{EngineFamily, StoreError, StoreFlavour, StoreHealth, StoreVersion, TelemetryStore};
use crucible_schema::TABLES;

use crate::{Ctx, expect};

pub(crate) async fn startup(ctx: &Ctx) -> Result<String> {
    ctx.migrate().await?;
    match ctx.store.health().await {
        StoreHealth::Healthy => {}
        StoreHealth::Unavailable { reason } => bail!("the server is not healthy: {reason}"),
    }

    let facts = ctx.store.detect().await.context("detect")?;
    expect(facts.version >= StoreVersion::MINIMUM, || {
        format!("version {:?} is below the minimum", facts.version)
    })?;
    expect(facts.flavour == ctx.flavour, || {
        format!("flavour {:?}, expected {:?}", facts.flavour, ctx.flavour)
    })?;
    let family = match ctx.flavour {
        StoreFlavour::SingleNode => EngineFamily::MergeTree,
        StoreFlavour::Clustered => EngineFamily::Replicated,
        StoreFlavour::Cloud => EngineFamily::Shared,
    };
    let expected: Vec<_> = TABLES.iter().map(|def| (def.id, family)).collect();
    expect(facts.engines == expected, || {
        format!("engines {:?}", facts.engines)
    })?;
    expect(facts.missing_settings.is_empty(), || {
        format!("the server lacks settings {:?}", facts.missing_settings)
    })?;
    expect(facts.text_search_functions && facts.text_index, || {
        "text search is unavailable".to_owned()
    })?;
    if ctx.flavour == StoreFlavour::Clustered {
        expect(facts.ddl_initial_user_and_roles == Some(true), || {
            format!(
                "distributed_ddl_use_initial_user_and_roles is {:?}",
                facts.ddl_initial_user_and_roles
            )
        })?;
    }

    ctx.store.dedup_selftest().await.context("dedup_selftest")?;
    Ok(format!(
        "version {}.{}, {:?}, {} tables, every setting known, self-test left one row",
        facts.version.major,
        facts.version.minor,
        facts.flavour,
        facts.engines.len()
    ))
}

/// Every service identity sees a known row after access step A1 (`detect` checked that in `startup`). Then the
/// negative: with the service policy of `dedup_selftest` narrowed to the admin user, a server that hides rows
/// from users without a policy makes `detect` refuse.
pub(crate) async fn visibility(ctx: &Ctx) -> Result<String> {
    let admin = &ctx.config.admin.user;
    let policy = "crucible_service_dedup_selftest ON crucible.dedup_selftest";
    ctx.admin(format!("DROP ROW POLICY {policy}")).await?;
    ctx.admin(format!(
        "CREATE ROW POLICY {policy} AS PERMISSIVE FOR SELECT USING 1 TO {admin}"
    ))
    .await?;
    let narrowed = ctx.store.detect().await;
    // Restore the policy of access step A1 before judging.
    ctx.admin(format!("DROP ROW POLICY {policy}")).await?;
    ctx.migrate().await?;
    ctx.store.detect().await.context("detect after the policy was restored")?;
    match narrowed {
        Err(StoreError::SelfTest(reason)) => Ok(format!(
            "all five identities see the known row; with the policy narrowed detect refuses ({reason})"
        )),
        Ok(_) => Ok(
            "all five identities see the known row; this server lets users without a row policy read rows, so \
             narrowing the policy hides nothing"
                .to_owned(),
        ),
        Err(other) => Err(other).context("detect with the policy narrowed"),
    }
}
