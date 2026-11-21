//! What an action row carries besides its table and partition (the `detail` document of
//! `lifecycle_actions`), and the predicate, the store call and the verification read it stands for.
//!
//! An action is built again from its row alone, by whichever node leads when it is issued or verified, so the
//! document holds everything the predicate needs: the retention cut-off, or the deletion request's point in
//! time with the trace ids, the user or the session it selects.

use crucible_control::{ActionKind, LifecycleAction, TxError};
use crucible_plan::{
    CmpOp, DeletePlan, Literal, PlanError, Pred, QueryPlan, expired_before, lifecycle_column,
    lifecycle_oldest,
};
use crucible_records::{PartitionId, ProjectScope, TableId, Timestamp, TraceId, Uuid};
use crucible_schema::Granularity;
use serde::{Deserialize, Serialize};

use crate::partitions::partition_window;

/// The `detail` document of an action.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(tag = "what", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Detail {
    /// A retention delete of the rows before `cutoff` (microseconds; the start of a UTC day).
    Retention {
        /// The cut-off.
        cutoff: i64,
    },
    /// A partition drop: every project with rows in the partition had expired.
    Drop,
    /// A delete of what a deletion request selects, among the rows received before `requested_at`.
    Request {
        /// The request's point in time, in microseconds.
        requested_at: i64,
        /// Trace ids, as 32 hex characters.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        traces: Vec<String>,
        /// A `user_id`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user_id: Option<String>,
        /// A `session_id`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        /// The `user_subject_hash` values of the `user_id`, as 32 hex characters: on the MCP tables a deletion
        /// by user also selects the rows that carry one of them.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        subjects: Vec<String>,
    },
    /// A mask application over the deletes that had finished when it was planned.
    Mask {
        /// The ids of those delete actions.
        covers: Vec<String>,
    },
}

/// Why an action cannot be built from its row.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub(crate) enum DetailError {
    /// The stored document does not parse.
    #[error("the action's detail does not parse: {0}")]
    Parse(String),
    /// The document does not fit the action's kind or table.
    #[error("the action's detail does not fit its kind or table: {0}")]
    Shape(&'static str),
    /// The typed plan refused the predicate.
    #[error("the action's plan was refused: {0}")]
    Plan(String),
}

impl From<PlanError> for DetailError {
    fn from(error: PlanError) -> Self {
        Self::Plan(error.to_string())
    }
}

impl From<DetailError> for TxError {
    fn from(error: DetailError) -> Self {
        Self::Invalid(error.to_string())
    }
}

/// A subject hash as a detail document holds it: 32 lowercase hex characters.
pub(crate) fn subject_hex(hash: &[u8; 16]) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The subject hash that [`subject_hex`] wrote.
pub(crate) fn subject_hash(hex: &str) -> Result<[u8; 16], DetailError> {
    let bad = || DetailError::Shape("a subject hash is not 32 hex characters");
    if hex.len() != 32 {
        return Err(bad());
    }
    let mut hash = [0u8; 16];
    for (at, byte) in hash.iter_mut().enumerate() {
        let pair = hex.get(2 * at..2 * at + 2).ok_or_else(bad)?;
        *byte = u8::from_str_radix(pair, 16).map_err(|_| bad())?;
    }
    Ok(hash)
}

impl Detail {
    pub(crate) fn to_text(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub(crate) fn parse(text: &str) -> Result<Self, DetailError> {
        serde_json::from_str(text).map_err(|error| DetailError::Parse(error.to_string()))
    }

    /// The delete actions a mask covers.
    pub(crate) fn covers(&self) -> Vec<Uuid> {
        match self {
            Self::Mask { covers } => covers.iter().filter_map(|id| id.parse().ok()).collect(),
            _ => Vec::new(),
        }
    }

    /// The rows of `table_id` a delete with this document removes, besides the project predicate.
    pub(crate) fn pred(&self, table_id: TableId) -> Result<Pred, DetailError> {
        match self {
            Self::Retention { cutoff } => {
                Ok(expired_before(table_id, Timestamp::from_micros(*cutoff))?)
            }
            Self::Request {
                requested_at,
                traces,
                user_id,
                session_id,
                subjects,
            } => {
                let mut selected = Vec::new();
                if !traces.is_empty() {
                    let column = lifecycle_column(table_id, "trace_id")
                        .ok_or(DetailError::Shape("the table has no trace_id"))?;
                    let ids = traces
                        .iter()
                        .map(|id| TraceId::parse(id).map(Literal::from))
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|_| DetailError::Shape("a trace id is not 32 hex characters"))?;
                    selected.push(Pred::In(column, ids));
                }
                for (name, value) in [("user_id", user_id), ("session_id", session_id)] {
                    if let Some(value) = value {
                        let column = lifecycle_column(table_id, name)
                            .ok_or(DetailError::Shape("the table lacks the selector's column"))?;
                        selected.push(Pred::Cmp(column, CmpOp::Eq, Literal::from(value.as_str())));
                    }
                }
                if !subjects.is_empty() {
                    let column = lifecycle_column(table_id, "user_subject_hash")
                        .ok_or(DetailError::Shape("the table has no user_subject_hash"))?;
                    let hashes = subjects
                        .iter()
                        .map(|hash| subject_hash(hash).map(|hash| Literal::Bytes(hash.to_vec())))
                        .collect::<Result<Vec<_>, _>>()?;
                    selected.push(Pred::In(column, hashes));
                }
                let selected = match selected.len() {
                    0 => return Err(DetailError::Shape("the request selects nothing")),
                    1 => selected.remove(0),
                    _ => Pred::Or(selected),
                };
                // The tables merged by key carry no arrival time: their rows go by key (contract 13.4).
                Ok(match lifecycle_column(table_id, "received_at") {
                    Some(received_at) => Pred::And(vec![
                        selected,
                        Pred::Cmp(
                            received_at,
                            CmpOp::Lt,
                            Literal::Time(Timestamp::from_micros(*requested_at)),
                        ),
                    ]),
                    None => selected,
                })
            }
            Self::Drop | Self::Mask { .. } => Err(DetailError::Shape("not a delete")),
        }
    }
}

/// What an action sends to the store.
#[derive(Clone, PartialEq, Debug)]
pub(crate) enum Call {
    /// A lightweight delete.
    Delete(DeletePlan),
    /// A partition drop.
    Drop(TableId, PartitionId),
    /// A mask application.
    Mask(TableId, PartitionId),
}

/// The store call of an action. `scope` is the scope of the action's project, which a delete needs.
pub(crate) fn call(
    action: &LifecycleAction,
    scope: Option<ProjectScope>,
) -> Result<Call, DetailError> {
    match action.kind {
        ActionKind::Drop => Ok(Call::Drop(action.table, action.partition)),
        ActionKind::Mask => Ok(Call::Mask(action.table, action.partition)),
        ActionKind::Delete => {
            let scope = scope.ok_or(DetailError::Shape("a delete without a project"))?;
            let pred = Detail::parse(&action.detail)?.pred(action.table)?;
            Ok(Call::Delete(DeletePlan::project(
                scope,
                action.table,
                action.partition,
                pred,
            )?))
        }
    }
}

/// The read that verifies a delete: the oldest row of the partition that the delete's predicate still
/// matches. No row means the delete left nothing behind.
pub(crate) fn verification(
    action: &LifecycleAction,
    scope: ProjectScope,
    granularity: Granularity,
) -> Result<QueryPlan, DetailError> {
    let window = partition_window(action.table, granularity, action.partition)
        .ok_or(DetailError::Shape("the partition is not one of the table"))?;
    let pred = Detail::parse(&action.detail)?.pred(action.table)?;
    // On a cluster no replica may hold a matching row.
    Ok(lifecycle_oldest(scope, action.table, window, pred)?.on_every_replica())
}

#[cfg(test)]
mod tests {
    use crucible_auth::test_support::scope;
    use crucible_control::ActionState;
    use crucible_records::{CivilDate, ProjectId};

    use super::*;

    fn action(kind: ActionKind, table: TableId, detail: &Detail) -> LifecycleAction {
        let partition = if matches!(table, TableId::FactScores | TableId::UserRollup) {
            PartitionId::month(2026, 11).unwrap()
        } else {
            PartitionId::date(CivilDate::new(2026, 11, 17).unwrap())
        };
        LifecycleAction {
            id: Uuid::from_bytes([1; 16]),
            key: "k".to_owned(),
            kind,
            table,
            partition,
            project_id: Some(ProjectId::from_bytes([2; 16])),
            request_id: None,
            detail: detail.to_text(),
            state: ActionState::Planned,
            fencing_token: 0,
            mutation: None,
            attempts: 0,
            created_at: Timestamp::EPOCH,
            updated_at: Timestamp::EPOCH,
        }
    }

    fn request(traces: &[u8], user_id: Option<&str>, session_id: Option<&str>) -> Detail {
        Detail::Request {
            requested_at: 1_000,
            traces: traces
                .iter()
                .map(|n| TraceId::from_bytes([*n; 16]).to_string())
                .collect(),
            user_id: user_id.map(str::to_owned),
            session_id: session_id.map(str::to_owned),
            subjects: Vec::new(),
        }
    }

    fn by_subject(hashes: &[[u8; 16]]) -> Detail {
        Detail::Request {
            requested_at: 1_000,
            traces: Vec::new(),
            user_id: Some("ada".to_owned()),
            session_id: None,
            subjects: hashes.iter().map(subject_hex).collect(),
        }
    }

    #[test]
    fn subject_hashes_round_trip_through_hex() {
        let hash = [
            0x00, 0x01, 0xab, 0xff, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 0xfe,
        ];
        assert_eq!(subject_hex(&hash), "0001abff0708090a0b0c0d0e0f1011fe");
        assert_eq!(subject_hash(&subject_hex(&hash)), Ok(hash));
        for bad in [
            "",
            "0001",
            "zz01abff0708090a0b0c0d0e0f1011fe",
            "é001abff0708090a0b0c0d0e0f1011f",
        ] {
            assert!(subject_hash(bad).is_err(), "{bad}");
        }
    }

    /// Contract 13.3: on the MCP tables a deletion by user selects `user_id` or one of the subject hashes;
    /// a table without the hash column refuses the document rather than drop the term.
    #[test]
    fn a_user_predicate_carries_the_subject_hashes_where_the_table_has_them() {
        let scope = scope(ProjectId::from_bytes([2; 16])).unwrap();
        let detail = by_subject(&[[1; 16], [2; 16]]);
        assert_eq!(Detail::parse(&detail.to_text()), Ok(detail.clone()));
        for table in [TableId::McpOps, TableId::McpContent] {
            let Pred::And(parts) = detail.pred(table).unwrap() else {
                panic!("no arrival bound on {table:?}");
            };
            let Pred::Or(terms) = &parts[0] else {
                panic!("{:?}", parts[0]);
            };
            assert!(matches!(&terms[0], Pred::Cmp(c, CmpOp::Eq, _) if c.name() == "user_id"));
            assert!(
                matches!(&terms[1], Pred::In(c, values) if c.name() == "user_subject_hash" && values.len() == 2)
            );
            let action = action(ActionKind::Delete, table, &detail);
            assert!(matches!(call(&action, Some(scope)), Ok(Call::Delete(_))));
            assert!(verification(&action, scope, Granularity::Day).is_ok());
        }
        assert!(detail.pred(TableId::SpanCore).is_err());
        assert!(
            Detail::parse(r#"{"what":"request","requested_at":1,"user_id":"a","subjects":["01"]}"#)
                .unwrap()
                .pred(TableId::McpOps)
                .is_err()
        );
    }

    /// Contract 13.3, step 7: on a cluster the verification of a delete must find no row on any replica, so its
    /// read goes to every replica.
    #[test]
    fn the_verification_of_a_delete_reads_every_replica() {
        let scope = scope(ProjectId::from_bytes([2; 16])).unwrap();
        let action = action(
            ActionKind::Delete,
            TableId::SpanCore,
            &request(&[1], None, None),
        );
        let plan = verification(&action, scope, Granularity::Day).unwrap();
        assert!(plan.every_replica());
    }

    #[test]
    fn details_round_trip() {
        for detail in [
            Detail::Retention {
                cutoff: 86_400_000_000,
            },
            Detail::Drop,
            request(&[1, 2], Some("ada"), None),
            request(&[], None, Some("s-1")),
            Detail::Mask {
                covers: vec![Uuid::from_bytes([9; 16]).to_string()],
            },
        ] {
            assert_eq!(Detail::parse(&detail.to_text()), Ok(detail.clone()));
        }
        assert_eq!(
            request(&[], None, Some("s-1")).to_text(),
            r#"{"what":"request","requested_at":1000,"session_id":"s-1"}"#
        );
        assert!(Detail::parse(r#"{"what":"retention","cutoff":1,"more":2}"#).is_err());
    }

    #[test]
    fn a_request_predicate_names_only_columns_its_table_has() {
        let scope = scope(ProjectId::from_bytes([2; 16])).unwrap();
        // Span tables take every selector and the arrival bound.
        for table in [TableId::SpanCore, TableId::SpanPayload, TableId::TraceHeads] {
            for detail in [
                request(&[1], None, None),
                request(&[], Some("ada"), None),
                request(&[], None, Some("s")),
            ] {
                let pred = detail.pred(table).unwrap();
                assert!(
                    matches!(&pred, Pred::And(parts) if parts.len() == 2),
                    "{pred:?}"
                );
                let action = action(ActionKind::Delete, table, &detail);
                assert!(matches!(call(&action, Some(scope)), Ok(Call::Delete(_))));
                assert!(verification(&action, scope, Granularity::Day).is_ok());
            }
        }
        // MCP rows go by trace and by user, never by session.
        let both = request(&[1], Some("ada"), None)
            .pred(TableId::McpOps)
            .unwrap();
        assert!(
            matches!(&both, Pred::And(parts) if matches!(&parts[0], Pred::Or(two) if two.len() == 2))
        );
        assert!(request(&[], None, Some("s")).pred(TableId::McpOps).is_err());
        // Rollups go by key, without an arrival bound.
        let by_key = request(&[], Some("ada"), None)
            .pred(TableId::UserRollup)
            .unwrap();
        assert!(matches!(by_key, Pred::Cmp(..)));
        assert!(request(&[1], None, None).pred(TableId::TraceStats).is_ok());
        assert!(request(&[1], None, None).pred(TableId::UserRollup).is_err());
        assert!(request(&[], None, None).pred(TableId::SpanCore).is_err());
        // A delete needs its project.
        let orphan = action(
            ActionKind::Delete,
            TableId::SpanCore,
            &request(&[1], None, None),
        );
        assert!(call(&orphan, None).is_err());
    }

    #[test]
    fn a_retention_predicate_compares_the_table_time_column() {
        let cutoff = CivilDate::new(2026, 11, 1).unwrap().start();
        let detail = Detail::Retention {
            cutoff: cutoff.micros(),
        };
        for (table, date) in [
            (TableId::SpanCore, false),
            (TableId::FactScores, false),
            (TableId::SpanMetrics, false),
            (TableId::TraceStats, true),
            (TableId::UserRollup, true),
        ] {
            match detail.pred(table).unwrap() {
                Pred::Cmp(column, CmpOp::Lt, value) => {
                    assert_eq!(
                        column.name(),
                        crucible_schema::table(table).retention.time_column
                    );
                    assert_eq!(matches!(value, Literal::Date(_)), date, "{table:?}");
                }
                other => panic!("{other:?}"),
            }
        }
        // The unpartitioned tables keep their time in an aggregated column, which no predicate can name.
        assert!(detail.pred(TableId::McpDeploys).is_err());
        let drop = action(ActionKind::Drop, TableId::SpanCore, &Detail::Drop);
        assert!(matches!(call(&drop, None), Ok(Call::Drop(..))));
        assert!(Detail::Drop.pred(TableId::SpanCore).is_err());
    }
}
