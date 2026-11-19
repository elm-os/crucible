//! The retention and deletion repository over Postgres (the lifecycle migration): the node registry, deletion requests
//! and their acknowledgements, leader actions and retention floors.

use std::time::Duration;

use async_trait::async_trait;
use crucible_control::{
    ActionKind, ActionState, DeletionRequest, DeletionSelector, LifecycleAction, LifecycleRepo,
    Node, NodeAck, RetentionFloor, TxError, duration_micros,
};
use crucible_records::{
    DeletionKind, DeletionRequestId, DeletionState, Lsn, PartitionId, ProjectId, TableId,
    Timestamp, WalEpoch,
};

use crate::sql::{Cols, P, int};
use crate::store::{Handle, every, first};

const NODE: &str = "wal_epoch, heartbeat_at, version, checkpoint_lsn, address";

fn node(c: &mut Cols<'_>) -> Result<Node, TxError> {
    Ok(Node {
        wal_epoch: WalEpoch::from_uuid(c.uuid()?),
        heartbeat_at: c.ts()?,
        version: c.text()?,
        checkpoint_lsn: Lsn::new(c.u64()?),
        address: c.text()?,
    })
}

const REQUEST: &str = "id, project_id, kind, selector::text, state, requested_at, deletion_seq, \
     requested_by, idempotency_key, request_hash, places::text, attempts, error, updated_at";

fn request(c: &mut Cols<'_>) -> Result<DeletionRequest, TxError> {
    let id = DeletionRequestId::from_uuid(c.uuid()?);
    let project_id = ProjectId::from_uuid(c.uuid()?);
    let kind = c.parsed(DeletionKind::parse)?;
    let selector = DeletionSelector::parse(kind, &c.text()?)?;
    Ok(DeletionRequest {
        id,
        project_id,
        selector,
        state: c.parsed(DeletionState::parse)?,
        requested_at: c.ts()?,
        deletion_seq: c.u64()?,
        requested_by: c.text()?,
        idempotency_key: c.opt_text()?,
        request_hash: c.text()?,
        places: c.text()?,
        attempts: c.u32()?,
        error: c.opt_text()?,
        updated_at: c.ts()?,
    })
}

const ACK: &str = "request_id, wal_epoch, tombstone_lsn, passed_at";

fn ack(c: &mut Cols<'_>) -> Result<NodeAck, TxError> {
    Ok(NodeAck {
        request_id: DeletionRequestId::from_uuid(c.uuid()?),
        wal_epoch: WalEpoch::from_uuid(c.uuid()?),
        tombstone_lsn: Lsn::new(c.u64()?),
        passed_at: c.opt_ts()?,
    })
}

const ACTION: &str = "id, key, kind, table_id, partition_id, project_id, request_id, detail::text, state, \
     fencing_token, mutation, attempts, created_at, updated_at";

fn action(c: &mut Cols<'_>) -> Result<LifecycleAction, TxError> {
    let invalid = |what: &str| TxError::Invalid(format!("lifecycle action: {what}"));
    Ok(LifecycleAction {
        id: c.uuid()?,
        key: c.text()?,
        kind: c.parsed(ActionKind::parse)?,
        table: u8::try_from(c.u32()?)
            .ok()
            .and_then(TableId::from_id)
            .ok_or_else(|| invalid("unknown table id"))?,
        partition: PartitionId::parse(&c.text()?).map_err(|_| invalid("unknown partition id"))?,
        project_id: c.opt_uuid()?.map(ProjectId::from_uuid),
        request_id: c.opt_uuid()?.map(DeletionRequestId::from_uuid),
        detail: c.text()?,
        state: c.parsed(ActionState::parse)?,
        fencing_token: c.u64()?,
        mutation: c.opt_text()?,
        attempts: c.u32()?,
        created_at: c.ts()?,
        updated_at: c.ts()?,
    })
}

#[async_trait]
impl LifecycleRepo for Handle {
    async fn heartbeat(&self, node: &Node) -> Result<(), TxError> {
        self.write(
            "INSERT INTO nodes (wal_epoch, heartbeat_at, version, checkpoint_lsn, address) \
             VALUES ($1, clock_timestamp(), $2, $3, $4) \
             ON CONFLICT (wal_epoch) DO UPDATE SET heartbeat_at = clock_timestamp(), \
             version = EXCLUDED.version, checkpoint_lsn = EXCLUDED.checkpoint_lsn, \
             address = EXCLUDED.address RETURNING 1",
            &[
                P::Uuid(node.wal_epoch.uuid()),
                P::Text(&node.version),
                int(node.checkpoint_lsn.get())?,
                P::Text(&node.address),
            ],
        )
        .await?;
        Ok(())
    }

    async fn live_nodes(&self, within: Duration) -> Result<Vec<Node>, TxError> {
        let sql = format!(
            "SELECT {NODE} FROM nodes \
             WHERE heartbeat_at >= clock_timestamp() - $1 * interval '1 microsecond' ORDER BY wal_epoch"
        );
        every(
            &self.read(&sql, &[P::Int(duration_micros(within))]).await?,
            node,
        )
    }

    async fn insert_request(&self, request: &DeletionRequest) -> Result<u64, TxError> {
        let rows = self
            .query_main(
                true,
                "INSERT INTO deletion_requests (id, project_id, kind, selector, state, requested_at, \
                 deletion_seq, requested_by, idempotency_key, request_hash, places, attempts, error, \
                 updated_at) VALUES ($1, $2, $3, $4::jsonb, $5, $6, nextval('deletion_seq'), $7, $8, $9, \
                 $10::jsonb, $11, $12, $13) RETURNING deletion_seq",
                &[
                    P::Uuid(request.id.uuid()),
                    P::Uuid(request.project_id.uuid()),
                    P::Text(request.selector.kind().as_str()),
                    P::Text(&request.selector.to_text()),
                    P::Text(request.state.as_str()),
                    P::Ts(request.requested_at),
                    P::Text(&request.requested_by),
                    P::OptText(request.idempotency_key.as_deref()),
                    P::Text(&request.request_hash),
                    P::Text(&request.places),
                    P::Int4(request.attempts),
                    P::OptText(request.error.as_deref()),
                    P::Ts(request.updated_at),
                ],
            )
            .await?;
        first(&rows, |c| c.u64())?
            .ok_or_else(|| TxError::Invalid("the insert returned no sequence number".into()))
    }

    async fn request(
        &self,
        project_id: ProjectId,
        id: DeletionRequestId,
    ) -> Result<Option<DeletionRequest>, TxError> {
        let sql =
            format!("SELECT {REQUEST} FROM deletion_requests WHERE project_id = $1 AND id = $2");
        let rows = self
            .read(&sql, &[P::Uuid(project_id.uuid()), P::Uuid(id.uuid())])
            .await?;
        first(&rows, request)
    }

    async fn request_by_key(
        &self,
        project_id: ProjectId,
        requested_by: &str,
        key: &str,
    ) -> Result<Option<DeletionRequest>, TxError> {
        let sql = format!(
            "SELECT {REQUEST} FROM deletion_requests \
             WHERE project_id = $1 AND requested_by = $2 AND idempotency_key = $3"
        );
        let rows = self
            .read(
                &sql,
                &[
                    P::Uuid(project_id.uuid()),
                    P::Text(requested_by),
                    P::Text(key),
                ],
            )
            .await?;
        first(&rows, request)
    }

    async fn list_requests(
        &self,
        project_id: ProjectId,
        before: Option<u64>,
        limit: u32,
    ) -> Result<Vec<DeletionRequest>, TxError> {
        let sql = format!(
            "SELECT {REQUEST} FROM deletion_requests WHERE project_id = $1 AND deletion_seq < $2 \
             ORDER BY deletion_seq DESC LIMIT $3"
        );
        let before = before.map_or(Ok(P::Int(i64::MAX)), int)?;
        let rows = self
            .read(
                &sql,
                &[P::Uuid(project_id.uuid()), before, P::Int(i64::from(limit))],
            )
            .await?;
        every(&rows, request)
    }

    async fn open_requests(&self) -> Result<Vec<DeletionRequest>, TxError> {
        let sql = format!(
            "SELECT {REQUEST} FROM deletion_requests WHERE state NOT IN ('completed', 'failed') \
             ORDER BY deletion_seq"
        );
        every(&self.read(&sql, &[]).await?, request)
    }

    async fn update_request(&self, request: &DeletionRequest) -> Result<bool, TxError> {
        let changed = self
            .write(
                "UPDATE deletion_requests SET state = $2, places = $3::jsonb, attempts = $4, error = $5, \
                 updated_at = $6 WHERE id = $1 RETURNING 1",
                &[
                    P::Uuid(request.id.uuid()),
                    P::Text(request.state.as_str()),
                    P::Text(&request.places),
                    P::Int4(request.attempts),
                    P::OptText(request.error.as_deref()),
                    P::Ts(request.updated_at),
                ],
            )
            .await?;
        Ok(changed == 1)
    }

    async fn record_ack(&self, ack: &NodeAck) -> Result<bool, TxError> {
        let inserted = self
            .write(
                "INSERT INTO deletion_node_acks (request_id, wal_epoch, tombstone_lsn, passed_at) \
                 VALUES ($1, $2, $3, $4) ON CONFLICT (request_id, wal_epoch) DO NOTHING RETURNING 1",
                &[
                    P::Uuid(ack.request_id.uuid()),
                    P::Uuid(ack.wal_epoch.uuid()),
                    int(ack.tombstone_lsn.get())?,
                    P::OptTs(ack.passed_at),
                ],
            )
            .await?;
        Ok(inserted == 1)
    }

    async fn mark_passed(
        &self,
        request_id: DeletionRequestId,
        wal_epoch: WalEpoch,
        at: Timestamp,
    ) -> Result<bool, TxError> {
        let changed = self
            .write(
                "UPDATE deletion_node_acks SET passed_at = $3 \
                 WHERE request_id = $1 AND wal_epoch = $2 AND passed_at IS NULL RETURNING 1",
                &[
                    P::Uuid(request_id.uuid()),
                    P::Uuid(wal_epoch.uuid()),
                    P::Ts(at),
                ],
            )
            .await?;
        Ok(changed == 1)
    }

    async fn acks(&self, request_id: DeletionRequestId) -> Result<Vec<NodeAck>, TxError> {
        let sql = format!(
            "SELECT {ACK} FROM deletion_node_acks WHERE request_id = $1 ORDER BY wal_epoch"
        );
        every(&self.read(&sql, &[P::Uuid(request_id.uuid())]).await?, ack)
    }

    async fn node_acks(&self, wal_epoch: WalEpoch) -> Result<Vec<NodeAck>, TxError> {
        let sql = format!(
            "SELECT {ACK} FROM deletion_node_acks WHERE wal_epoch = $1 ORDER BY request_id"
        );
        every(&self.read(&sql, &[P::Uuid(wal_epoch.uuid())]).await?, ack)
    }

    async fn plan_action(&self, action: &LifecycleAction) -> Result<bool, TxError> {
        let inserted = self
            .write(
                "INSERT INTO lifecycle_actions (id, key, kind, table_id, partition_id, project_id, \
                 request_id, detail, state, fencing_token, mutation, attempts, created_at, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8::jsonb, $9, $10, $11, $12, $13, $14) \
                 ON CONFLICT (key) DO NOTHING RETURNING 1",
                &[
                    P::Uuid(action.id),
                    P::Text(&action.key),
                    P::Text(action.kind.as_str()),
                    P::Int4(u32::from(action.table.id())),
                    P::Text(&action.partition.to_string()),
                    P::OptUuid(action.project_id.map(ProjectId::uuid)),
                    P::OptUuid(action.request_id.map(DeletionRequestId::uuid)),
                    P::Text(&action.detail),
                    P::Text(action.state.as_str()),
                    int(action.fencing_token)?,
                    P::OptText(action.mutation.as_deref()),
                    P::Int4(action.attempts),
                    P::Ts(action.created_at),
                    P::Ts(action.updated_at),
                ],
            )
            .await?;
        Ok(inserted == 1)
    }

    async fn open_actions(&self) -> Result<Vec<LifecycleAction>, TxError> {
        let sql = format!(
            "SELECT {ACTION} FROM lifecycle_actions WHERE state NOT IN ('verified', 'failed') \
             ORDER BY created_at, id"
        );
        every(&self.read(&sql, &[]).await?, action)
    }

    async fn request_actions(
        &self,
        request_id: DeletionRequestId,
    ) -> Result<Vec<LifecycleAction>, TxError> {
        let sql = format!(
            "SELECT {ACTION} FROM lifecycle_actions WHERE request_id = $1 ORDER BY created_at, id"
        );
        every(
            &self.read(&sql, &[P::Uuid(request_id.uuid())]).await?,
            action,
        )
    }

    async fn update_action(&self, action: &LifecycleAction) -> Result<bool, TxError> {
        let changed = self
            .write(
                "UPDATE lifecycle_actions SET state = $2, fencing_token = $3, mutation = $4, \
                 attempts = $5, updated_at = $6 WHERE id = $1 RETURNING 1",
                &[
                    P::Uuid(action.id),
                    P::Text(action.state.as_str()),
                    int(action.fencing_token)?,
                    P::OptText(action.mutation.as_deref()),
                    P::Int4(action.attempts),
                    P::Ts(action.updated_at),
                ],
            )
            .await?;
        Ok(changed == 1)
    }

    async fn prune_actions(&self, before: Timestamp) -> Result<u64, TxError> {
        self.write(
            "DELETE FROM lifecycle_actions \
             WHERE state = 'verified' AND request_id IS NULL AND updated_at < $1 RETURNING 1",
            &[P::Ts(before)],
        )
        .await
    }

    async fn floors(&self) -> Result<Vec<(ProjectId, RetentionFloor)>, TxError> {
        let rows = self
            .read(
                "SELECT project_id, retention, payload, mcp_content FROM retention_floors \
                 ORDER BY project_id",
                &[],
            )
            .await?;
        every(&rows, |c| {
            Ok((
                ProjectId::from_uuid(c.uuid()?),
                RetentionFloor {
                    retention: c.opt_ts()?,
                    payload: c.opt_ts()?,
                    mcp_content: c.opt_ts()?,
                },
            ))
        })
    }

    async fn raise_floor(
        &self,
        project_id: ProjectId,
        floor: RetentionFloor,
    ) -> Result<(), TxError> {
        // GREATEST ignores nulls, so a setting without a floor keeps the other side's value.
        self.write(
            "INSERT INTO retention_floors (project_id, retention, payload, mcp_content) \
             VALUES ($1, $2, $3, $4) ON CONFLICT (project_id) DO UPDATE SET \
             retention = GREATEST(retention_floors.retention, EXCLUDED.retention), \
             payload = GREATEST(retention_floors.payload, EXCLUDED.payload), \
             mcp_content = GREATEST(retention_floors.mcp_content, EXCLUDED.mcp_content) RETURNING 1",
            &[
                P::Uuid(project_id.uuid()),
                P::OptTs(floor.retention),
                P::OptTs(floor.payload),
                P::OptTs(floor.mcp_content),
            ],
        )
        .await?;
        Ok(())
    }
}
