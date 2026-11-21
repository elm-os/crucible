//! Reads leave column statistics unused (contract 8.4, `docs/testing/perf.md`). ClickHouse 26.8 keeps one
//! snapshot of every part's column statistics per table, refreshed every `refresh_statistics_interval`, and
//! reuses it only while the table's parts are the ones the snapshot saw: after an insert, each read that plans
//! with statistics loads them for every part again. A typed read right after inserts must load none.

use anyhow::{Context as _, Result, anyhow};
use crucible_auth::test_support::scope;
use crucible_env::TelemetryStore;
use crucible_plan::{Caller, RetentionClamp, Value, trace_spans};
use crucible_records::{TableId, TraceId};
use crucible_schema::RetentionSettings;

use crate::inserts::plain_rows;
use crate::{Ctx, T0, expect};

/// Inserts into `span_core`, each of which leaves a part.
const INSERTS: u64 = 3;
/// Spans of each insert.
const SPANS: usize = 4;

pub(crate) async fn statistics(ctx: &Ctx) -> Result<String> {
    let project = ctx.project();
    let epoch = ctx.epoch();
    let trace = TraceId::from_bytes([0x5a; 16]);
    for seq in 1..=INSERTS {
        let salt = 90 + u32::try_from(seq)?;
        let rows = plain_rows(TableId::SpanCore, project, epoch, seq, T0, SPANS, salt)
            .all("trace_id", Value::Bytes(trace.to_bytes().to_vec()))
            .all("trace_hash", Value::UInt(u64::from(trace.trace_hash())));
        ctx.insert(&rows, ctx.token(epoch, seq, TableId::SpanCore, T0)?)
            .await?;
    }
    let scope = scope(project).map_err(|e| anyhow!("{e}"))?;
    let clamp = RetentionClamp::new(RetentionSettings::default(), T0);
    let plan = trace_spans(scope, T0, T0, &clamp, Caller::Session, trace)?
        .context("the builder returned no plan")?;
    let stream = ctx.store.execute(plan).await?;
    let query_id = stream.query_id();
    let spans = stream.rows().await?.len();
    let stored = SPANS * usize::try_from(INSERTS)?;
    expect(spans == stored, || {
        format!("the read returned {spans} spans of {stored}")
    })?;
    // The same conditions without the adapter's settings, as an operator's own query runs them.
    let control = format!("crucible-statistics-{query_id}");
    ctx.admin_text(format!(
        "SELECT count() FROM crucible.span_core WHERE project_id = toUUID('{}') AND trace_hash = {} AND \
         trace_id = toFixedString(unhex('{trace}'), 16) SETTINGS log_comment = '{control}'",
        project.uuid(),
        trace.trace_hash()
    ))
    .await?;
    ctx.admin("SYSTEM FLUSH LOGS").await?;
    let loaded = async |filter: String| -> Result<(u64, u64)> {
        let answer = ctx
            .admin_text(format!(
                "SELECT count(), sum(ProfileEvents['LoadedStatisticsMicroseconds']) FROM system.query_log \
                 WHERE type = 'QueryFinish' AND event_date >= yesterday() AND {filter}"
            ))
            .await?;
        let mut numbers = answer.split_whitespace().map(str::parse::<u64>);
        match (numbers.next(), numbers.next()) {
            (Some(Ok(reads)), Some(Ok(us))) => Ok((reads, us)),
            _ => Err(anyhow!("system.query_log answered {answer:?}")),
        }
    };
    let (reads, typed) = loaded(format!("query_id = '{query_id}'")).await?;
    let (_, raw) = loaded(format!("log_comment = '{control}'")).await?;
    expect(reads == 1, || {
        format!("system.query_log holds {reads} entries of the typed read")
    })?;
    expect(typed == 0, || {
        format!(
            "a typed read of span_core loaded {typed} us of column statistics right after {INSERTS} inserts"
        )
    })?;
    Ok(format!(
        "a typed read of span_core right after {INSERTS} inserts loaded no column statistics; the same conditions \
         without the adapter's settings loaded {raw} us"
    ))
}
