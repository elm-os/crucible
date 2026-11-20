//! The retention and deletion repository of the model (the lifecycle migration): the node registry, deletion requests
//! and their acknowledgements, leader actions and retention floors.
//!
//! Deletion sequence numbers are drawn when the row is written and survive a rollback, like the key feed's.
//! The heartbeat of a node is the database clock.

use std::time::Duration;

use async_trait::async_trait;
use crucible_control::{
    ActionState, DeletionRequest, LifecycleAction, LifecycleRepo, Node, NodeAck, RetentionFloor,
    TxError, duration_micros,
};
use crucible_records::{DeletionRequestId, DeletionState, ProjectId, Timestamp, WalEpoch};

use super::{Key, Pool, Row, Runner, Tbl, uuid_key};

pub(super) fn ack_key(request_id: DeletionRequestId, wal_epoch: WalEpoch) -> Key {
    [request_id.to_bytes(), wal_epoch.to_bytes()].concat()
}

/// The key of the unique constraint `deletion_requests_idempotency_key`.
pub(super) fn idempotency_key(request: &DeletionRequest) -> Option<Key> {
    let key = request.idempotency_key.as_ref()?;
    let mut unique = request.project_id.to_bytes().to_vec();
    unique.extend_from_slice(request.requested_by.as_bytes());
    unique.push(0);
    unique.extend_from_slice(key.as_bytes());
    Some(unique)
}

fn open(state: DeletionState) -> bool {
    !matches!(state, DeletionState::Completed | DeletionState::Failed)
}

#[async_trait]
impl LifecycleRepo for Runner {
    async fn heartbeat(&self, node: &Node) -> Result<(), TxError> {
        self.run(Pool::Main, true, |cx| {
            let row = Node {
                heartbeat_at: cx.now,
                ..node.clone()
            };
            cx.put(
                Tbl::Nodes,
                uuid_key(node.wal_epoch.to_bytes()),
                Some(Row::Node(row)),
            )
        })
        .await
    }

    async fn live_nodes(&self, within: Duration) -> Result<Vec<Node>, TxError> {
        self.run(Pool::Main, false, |cx| {
            let since = cx.now.saturating_add_micros(-duration_micros(within));
            let mut nodes = all!(cx, Tbl::Nodes, Row::Node);
            nodes.retain(|node| node.heartbeat_at >= since);
            Ok(nodes)
        })
        .await
    }

    async fn insert_request(&self, request: &DeletionRequest) -> Result<u64, TxError> {
        self.run(Pool::Main, true, |cx| {
            cx.db.deletion_seq += 1;
            let seq = cx.db.deletion_seq;
            let row = DeletionRequest {
                deletion_seq: seq,
                ..request.clone()
            };
            cx.insert(
                Tbl::DeletionRequests,
                uuid_key(request.id.to_bytes()),
                Row::DeletionRequest(row),
                "deletion_requests_pkey",
            )?;
            Ok(seq)
        })
        .await
    }

    async fn request(
        &self,
        project_id: ProjectId,
        id: DeletionRequestId,
    ) -> Result<Option<DeletionRequest>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(one!(
                cx,
                Tbl::DeletionRequests,
                &uuid_key(id.to_bytes()),
                Row::DeletionRequest
            )
            .filter(|request| request.project_id == project_id))
        })
        .await
    }

    async fn request_by_key(
        &self,
        project_id: ProjectId,
        requested_by: &str,
        key: &str,
    ) -> Result<Option<DeletionRequest>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(all!(cx, Tbl::DeletionRequests, Row::DeletionRequest)
                .into_iter()
                .find(|request| {
                    request.project_id == project_id
                        && request.requested_by == requested_by
                        && request.idempotency_key.as_deref() == Some(key)
                }))
        })
        .await
    }

    async fn list_requests(
        &self,
        project_id: ProjectId,
        before: Option<u64>,
        limit: u32,
    ) -> Result<Vec<DeletionRequest>, TxError> {
        self.run(Pool::Main, false, |cx| {
            let mut requests = all!(cx, Tbl::DeletionRequests, Row::DeletionRequest);
            requests.retain(|request| {
                request.project_id == project_id
                    && before.is_none_or(|before| request.deletion_seq < before)
            });
            requests.sort_by_key(|request| std::cmp::Reverse(request.deletion_seq));
            requests.truncate(limit as usize);
            Ok(requests)
        })
        .await
    }

    async fn open_requests(&self) -> Result<Vec<DeletionRequest>, TxError> {
        self.run(Pool::Main, false, |cx| {
            let mut requests = all!(cx, Tbl::DeletionRequests, Row::DeletionRequest);
            requests.retain(|request| open(request.state));
            requests.sort_by_key(|request| request.deletion_seq);
            Ok(requests)
        })
        .await
    }

    async fn requests_after(
        &self,
        requested_after: Timestamp,
        after: u64,
        limit: u32,
    ) -> Result<Vec<DeletionRequest>, TxError> {
        self.run(Pool::Main, false, |cx| {
            let mut requests = all!(cx, Tbl::DeletionRequests, Row::DeletionRequest);
            requests.retain(|request| {
                request.requested_at > requested_after && request.deletion_seq > after
            });
            requests.sort_by_key(|request| request.deletion_seq);
            requests.truncate(limit as usize);
            Ok(requests)
        })
        .await
    }

    async fn update_request(&self, request: &DeletionRequest) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(request.id.to_bytes());
            let Some(stored) = one!(cx, Tbl::DeletionRequests, &key, Row::DeletionRequest) else {
                return Ok(false);
            };
            let next = DeletionRequest {
                state: request.state,
                places: request.places.clone(),
                attempts: request.attempts,
                error: request.error.clone(),
                updated_at: request.updated_at,
                ..stored
            };
            cx.put(Tbl::DeletionRequests, key, Some(Row::DeletionRequest(next)))?;
            Ok(true)
        })
        .await
    }

    async fn record_ack(&self, ack: &NodeAck) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = ack_key(ack.request_id, ack.wal_epoch);
            if cx.locked_by_other(Tbl::DeletionAcks, &key) {
                return Err(super::Halt::Blocked);
            }
            if cx.latest(Tbl::DeletionAcks, &key).is_some() {
                return Ok(false);
            }
            cx.put(Tbl::DeletionAcks, key, Some(Row::DeletionAck(*ack)))?;
            Ok(true)
        })
        .await
    }

    async fn mark_passed(
        &self,
        request_id: DeletionRequestId,
        wal_epoch: WalEpoch,
        at: Timestamp,
    ) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = ack_key(request_id, wal_epoch);
            match one!(cx, Tbl::DeletionAcks, &key, Row::DeletionAck) {
                Some(stored) if stored.passed_at.is_none() => {
                    let next = NodeAck {
                        passed_at: Some(at),
                        ..stored
                    };
                    cx.put(Tbl::DeletionAcks, key, Some(Row::DeletionAck(next)))?;
                    Ok(true)
                }
                _ => Ok(false),
            }
        })
        .await
    }

    async fn acks(&self, request_id: DeletionRequestId) -> Result<Vec<NodeAck>, TxError> {
        self.run(Pool::Main, false, |cx| {
            let mut acks = all!(cx, Tbl::DeletionAcks, Row::DeletionAck);
            acks.retain(|ack| ack.request_id == request_id);
            acks.sort_by_key(|ack| ack.wal_epoch);
            Ok(acks)
        })
        .await
    }

    async fn node_acks(&self, wal_epoch: WalEpoch) -> Result<Vec<NodeAck>, TxError> {
        self.run(Pool::Main, false, |cx| {
            let mut acks = all!(cx, Tbl::DeletionAcks, Row::DeletionAck);
            acks.retain(|ack| ack.wal_epoch == wal_epoch);
            acks.sort_by_key(|ack| ack.request_id);
            Ok(acks)
        })
        .await
    }

    async fn plan_action(&self, action: &LifecycleAction) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let taken = cx
                .latest_rows(Tbl::Actions)
                .into_values()
                .chain(cx.scan(Tbl::Actions).into_iter().map(|(_, row)| row))
                .any(|row| matches!(row, Row::Action(stored) if stored.key == action.key));
            if taken {
                return Ok(false);
            }
            cx.insert(
                Tbl::Actions,
                uuid_key(action.id.to_bytes()),
                Row::Action(action.clone()),
                "lifecycle_actions_pkey",
            )?;
            Ok(true)
        })
        .await
    }

    async fn open_actions(&self) -> Result<Vec<LifecycleAction>, TxError> {
        self.run(Pool::Main, false, |cx| {
            let mut actions = all!(cx, Tbl::Actions, Row::Action);
            actions.retain(|action| {
                !matches!(action.state, ActionState::Verified | ActionState::Failed)
            });
            actions.sort_by_key(|action| (action.created_at, action.id));
            Ok(actions)
        })
        .await
    }

    async fn request_actions(
        &self,
        request_id: DeletionRequestId,
    ) -> Result<Vec<LifecycleAction>, TxError> {
        self.run(Pool::Main, false, |cx| {
            let mut actions = all!(cx, Tbl::Actions, Row::Action);
            actions.retain(|action| action.request_id == Some(request_id));
            actions.sort_by_key(|action| (action.created_at, action.id));
            Ok(actions)
        })
        .await
    }

    async fn update_action(&self, action: &LifecycleAction) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(action.id.to_bytes());
            let Some(stored) = one!(cx, Tbl::Actions, &key, Row::Action) else {
                return Ok(false);
            };
            let next = LifecycleAction {
                state: action.state,
                fencing_token: action.fencing_token,
                mutation: action.mutation.clone(),
                attempts: action.attempts,
                updated_at: action.updated_at,
                ..stored
            };
            cx.put(Tbl::Actions, key, Some(Row::Action(next)))?;
            Ok(true)
        })
        .await
    }

    async fn prune_actions(&self, before: Timestamp) -> Result<u64, TxError> {
        self.run(Pool::Main, true, |cx| {
            let mut pruned = 0;
            for action in all!(cx, Tbl::Actions, Row::Action) {
                if action.state == ActionState::Verified
                    && action.request_id.is_none()
                    && action.updated_at < before
                {
                    cx.put(Tbl::Actions, uuid_key(action.id.to_bytes()), None)?;
                    pruned += 1;
                }
            }
            Ok(pruned)
        })
        .await
    }

    async fn floors(&self) -> Result<Vec<(ProjectId, RetentionFloor)>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(all!(cx, Tbl::Floors, Row::Floor))
        })
        .await
    }

    async fn raise_floor(
        &self,
        project_id: ProjectId,
        floor: RetentionFloor,
    ) -> Result<(), TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(project_id.to_bytes());
            let stored = one!(cx, Tbl::Floors, &key, Row::Floor)
                .map_or_else(RetentionFloor::default, |(_, floor)| floor);
            cx.put(
                Tbl::Floors,
                key,
                Some(Row::Floor((project_id, stored.max(floor)))),
            )
        })
        .await
    }
}

impl super::PgModel {
    /// Every committed leader action, in key order. For oracles: no fault applies and no time passes.
    pub fn lifecycle_actions(&self) -> Vec<LifecycleAction> {
        let db = super::lock(&self.shared().db);
        db.rows
            .values()
            .filter_map(|slot| match &slot.val {
                Some(Row::Action(action)) => Some(action.clone()),
                _ => None,
            })
            .collect()
    }

    /// Every committed deletion request, in key order. For oracles: no fault applies and no time passes.
    pub fn deletion_requests(&self) -> Vec<DeletionRequest> {
        let db = super::lock(&self.shared().db);
        db.rows
            .values()
            .filter_map(|slot| match &slot.val {
                Some(Row::DeletionRequest(request)) => Some(request.clone()),
                _ => None,
            })
            .collect()
    }
}
