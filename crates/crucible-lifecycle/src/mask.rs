//! The mask scheduler (contract 13.2, step 6): a leader job under the `lifecycle` lease. Once per cycle
//! (`lifecycle.mask_cycle`, 6 h) it coalesces the finished lightweight deletes of every partition into one
//! `APPLY DELETED MASK IN PARTITION`, which removes their rows physically.
//!
//! A cycle is the database clock divided by the cycle length, and a mask action's key holds its table,
//! partition and cycle, so a partition gets at most one mask application per cycle whoever leads.

use std::collections::BTreeMap;
use std::sync::Arc;

use crucible_control::{ActionKind, ActionState};
use crucible_jobs::{JobResult, LeaderCtx};
use crucible_records::{PartitionId, TableId};

use crate::actions::{Place, Shared, plan, planned};
use crate::detail::Detail;
use crate::retention::Schedule;

/// One pass of the mask scheduler.
pub(crate) async fn run(ctx: LeaderCtx, shared: Arc<Shared>) -> JobResult {
    if shared.config.drops_mask_cycle() {
        return Ok(());
    }
    let db_now = ctx.store().db_now().await?;
    let cycle = Schedule::period(db_now, shared.config.mask_cycle);
    if Schedule::load(&ctx).await?.mask >= cycle {
        return Ok(());
    }
    let open = ctx.store().lifecycle().open_actions().await?;
    // A partition whose earlier mask application is still on its way is left to that one.
    let mut pairs: BTreeMap<(TableId, PartitionId), Vec<String>> = BTreeMap::new();
    for action in &open {
        if action.kind == ActionKind::Delete && action.state == ActionState::Done {
            pairs
                .entry((action.table, action.partition))
                .or_default()
                .push(action.id.to_string());
        }
    }
    for action in &open {
        if action.kind == ActionKind::Mask {
            pairs.remove(&(action.table, action.partition));
        }
    }
    let actions = pairs
        .into_iter()
        .map(|((table, partition), covers)| {
            planned(
                ctx.env.new_id(),
                db_now,
                format!("mask:{}:{partition}:{cycle}", table.name()),
                ActionKind::Mask,
                Place {
                    table,
                    partition,
                    project_id: None,
                    request_id: None,
                },
                &Detail::Mask { covers },
            )
        })
        .collect();
    let planned = plan(&ctx, actions).await?;
    if planned > 0 {
        tracing::info!(
            planned,
            cycle,
            "the mask scheduler planned its mask applications"
        );
    }
    let mut schedule = Schedule::load(&ctx).await?;
    schedule.mask = cycle;
    schedule.store(&ctx).await
}
