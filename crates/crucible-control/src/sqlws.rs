//! The SQL workspace's rows (contracts 7.4 and 11.1): the ClickHouse identity of each project, saved queries
//! and the audit row of every run, with their repositories.

use std::fmt;

use async_trait::async_trait;
use crucible_records::{ApiKeyId, ProjectId, SavedQueryId, SqlRunId, Timestamp, UserId, Uuid};
use serde::{Deserialize, Serialize};

use crate::error::TxError;

/// A closed set of values stored as lowercase text.
macro_rules! text_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident => $text:literal,)+ }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
        pub enum $name {
            $($(#[$vmeta])* #[serde(rename = $text)] $variant,)+
        }

        impl $name {
            /// Every variant, in declaration order.
            pub const ALL: &'static [Self] = &[$(Self::$variant,)+];

            /// The stored text.
            pub const fn as_str(&self) -> &'static str {
                match self {
                    $(Self::$variant => $text,)+
                }
            }

            /// The variant with this stored text.
            pub fn parse(text: &str) -> Option<Self> {
                match text {
                    $($text => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

text_enum! {
    /// Where a project's ClickHouse identity stands (contract 7.4).
    SqlIdentityState {
        /// Requested; the identity may not exist in ClickHouse yet.
        Pending => "pending",
        /// Provisioned at `generation` with `retention`.
        Active => "active",
        /// Being removed; the workspace does not use it.
        Dropping => "dropping",
    }
}

/// The retention a project's row policies were written with. It mirrors `crucible_schema::RetentionSettings`;
/// this crate does not depend on the schema crate.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct AppliedRetention {
    /// `retention_days`.
    pub retention_days: Option<u32>,
    /// `payload_retention_days`.
    pub payload_retention_days: Option<u32>,
    /// `mcp_content_retention_days`.
    pub mcp_content_retention_days: Option<u32>,
}

/// One row of `ch_sql_identities`: the ClickHouse identity of a project.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SqlIdentity {
    /// The project.
    pub project_id: ProjectId,
    /// The password generation the identity was last provisioned with. The workspace signs in with it.
    pub generation: u32,
    /// The generation the identity should have. Rotation raises it; the reconciler follows.
    pub target_generation: u32,
    /// Where the identity stands.
    pub state: SqlIdentityState,
    /// The retention its policies were written with.
    pub retention: AppliedRetention,
    /// When the reconciler last provisioned or confirmed it.
    pub last_probe: Option<Timestamp>,
    /// When the workspace last used it. Identities idle for 30 days are dropped.
    pub last_used_at: Timestamp,
}

/// The ClickHouse identities of projects. Changes are compare-and-set on the state and the generation, so the
/// reconciler and a first use on another node cannot overwrite each other.
#[async_trait]
pub trait SqlIdentityRepo: Send + Sync {
    /// The identity of a project.
    async fn get(&self, project_id: ProjectId) -> Result<Option<SqlIdentity>, TxError>;
    /// Every identity, ordered by project id.
    async fn list(&self) -> Result<Vec<SqlIdentity>, TxError>;
    /// Creates the row. A second row for the project is `UniqueViolation("ch_sql_identities_pkey")`.
    async fn insert(&self, identity: &SqlIdentity) -> Result<(), TxError>;
    /// Writes `identity` if the stored row still has this state and generation. `false` when it does not.
    async fn update_if(
        &self,
        identity: &SqlIdentity,
        expected_state: SqlIdentityState,
        expected_generation: u32,
    ) -> Result<bool, TxError>;
    /// Records a use: `last_used_at` moves forward to `at`, never back. `false` when there is no row.
    async fn touch(&self, project_id: ProjectId, at: Timestamp) -> Result<bool, TxError>;
    /// Raises `target_generation` of every identity by one; returns how many rows changed.
    async fn rotate_all(&self) -> Result<u64, TxError>;
    /// Deletes the row if it is in `dropping`. `false` when it is not.
    async fn delete_dropping(&self, project_id: ProjectId) -> Result<bool, TxError>;
}

text_enum! {
    /// Who may see a saved query (contract 12.6).
    SqlVisibility {
        /// Its owner only.
        Private => "private",
        /// Everyone who can read the project.
        Project => "project",
    }
}

text_enum! {
    /// The type of a declared parameter of a saved query. Each maps to one ClickHouse type, so a value is
    /// checked before it is sent.
    SqlParamType {
        /// `String`.
        String => "string",
        /// `Int64`.
        Int => "int",
        /// `Float64`.
        Float => "float",
        /// `Bool`.
        Bool => "bool",
        /// `DateTime64(6, 'UTC')`, written as RFC 3339.
        Timestamp => "timestamp",
        /// `UUID`.
        Uuid => "uuid",
    }
}

/// One declared parameter of a saved query.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedParam {
    /// The name, as the text uses it in `{name:Type}`.
    pub name: String,
    /// The type.
    #[serde(rename = "type")]
    pub ty: SqlParamType,
    /// The value used when a run gives none; `None` makes the parameter required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// Who made a row: a signed-in user or an API key.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum SqlActor {
    /// A signed-in user.
    User(UserId),
    /// An API key.
    ApiKey(ApiKeyId),
}

impl SqlActor {
    /// The user, if the actor is one.
    pub const fn user_id(&self) -> Option<UserId> {
        match self {
            Self::User(id) => Some(*id),
            Self::ApiKey(_) => None,
        }
    }

    /// The API key, if the actor is one.
    pub const fn api_key_id(&self) -> Option<ApiKeyId> {
        match self {
            Self::User(_) => None,
            Self::ApiKey(id) => Some(*id),
        }
    }

    /// The actor of a row's two nullable columns.
    pub fn from_columns(user: Option<Uuid>, key: Option<Uuid>) -> Result<Self, TxError> {
        match (user, key) {
            (Some(user), None) => Ok(Self::User(UserId::from_uuid(user))),
            (None, Some(key)) => Ok(Self::ApiKey(ApiKeyId::from_uuid(key))),
            _ => Err(TxError::Invalid(
                "a row names a user or an API key, never both or neither".to_owned(),
            )),
        }
    }
}

/// A saved query.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SavedQuery {
    /// The id.
    pub id: SavedQueryId,
    /// The project.
    pub project_id: ProjectId,
    /// Who owns it.
    pub owner: SqlActor,
    /// The display name.
    pub name: String,
    /// What it is for.
    pub description: String,
    /// Who may see it.
    pub visibility: SqlVisibility,
    /// The query text.
    pub sql: String,
    /// The declared parameters, besides the window parameters `from` and `to`.
    pub parameters: Vec<SavedParam>,
    /// The optimistic-concurrency version; starts at 1.
    pub version: u64,
    /// When it was created.
    pub created_at: Timestamp,
    /// When it last changed.
    pub updated_at: Timestamp,
}

/// Saved queries.
#[async_trait]
pub trait SavedQueryRepo: Send + Sync {
    /// The query with this id in the project.
    async fn get(&self, project_id: ProjectId, id: SavedQueryId) -> Result<Option<SavedQuery>, TxError>;
    /// The project's queries, ordered by id.
    async fn list(&self, project_id: ProjectId) -> Result<Vec<SavedQuery>, TxError>;
    /// Creates a query.
    async fn insert(&self, query: &SavedQuery) -> Result<(), TxError>;
    /// Writes `query` with version `expected + 1` if the stored version is `expected`. `false` when it is not.
    async fn update_if_version(&self, query: &SavedQuery, expected: u64) -> Result<bool, TxError>;
    /// Deletes a query. `false` when there is no such query.
    async fn delete(&self, project_id: ProjectId, id: SavedQueryId) -> Result<bool, TxError>;
}

text_enum! {
    /// What a run was.
    SqlRunKind {
        /// Ad hoc text.
        AdHoc => "ad_hoc",
        /// A saved query.
        Saved => "saved",
        /// An export.
        Export => "export",
    }
}

text_enum! {
    /// How a run ended.
    SqlRunOutcome {
        /// Still running, or the node died before it recorded the end.
        Running => "running",
        /// It returned a result.
        Succeeded => "succeeded",
        /// It failed; `error_code` says how.
        Failed => "failed",
        /// It was cancelled.
        Cancelled => "cancelled",
    }
}

/// How a finished run went.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SqlRunEnd {
    /// How it ended.
    pub outcome: SqlRunOutcome,
    /// The problem code of a failed run.
    pub error_code: Option<String>,
    /// Rows returned.
    pub result_rows: u64,
    /// Whether the result was cut at a limit.
    pub truncated: bool,
    /// Rows the server read.
    pub rows_read: u64,
    /// Bytes the server read.
    pub bytes_read: u64,
    /// Server-side time, in milliseconds.
    pub elapsed_ms: u64,
    /// When it ended.
    pub finished_at: Timestamp,
}

/// One row of `sql_runs`: the audit record of a run (contract 7.4). Kept 30 days.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SqlRun {
    /// The id.
    pub id: SqlRunId,
    /// The project.
    pub project_id: ProjectId,
    /// Who ran it.
    pub actor: SqlActor,
    /// The id the statement carried in ClickHouse.
    pub query_id: Uuid,
    /// What it was.
    pub kind: SqlRunKind,
    /// The saved query it ran, if any.
    pub saved_query_id: Option<SavedQueryId>,
    /// The query text.
    pub sql: String,
    /// When it started.
    pub started_at: Timestamp,
    /// How it ended; `None` while it runs.
    pub end: Option<SqlRunEnd>,
}

/// The audit rows of runs.
#[async_trait]
pub trait SqlRunRepo: Send + Sync {
    /// The run with this id in the project.
    async fn get(&self, project_id: ProjectId, id: SqlRunId) -> Result<Option<SqlRun>, TxError>;
    /// The project's most recent runs, newest first, at most `limit`.
    async fn list(&self, project_id: ProjectId, limit: u32) -> Result<Vec<SqlRun>, TxError>;
    /// Records the start of a run.
    async fn insert(&self, run: &SqlRun) -> Result<(), TxError>;
    /// Records the end of a run that has none yet. `false` when it already ended or does not exist.
    async fn finish(&self, id: SqlRunId, end: &SqlRunEnd) -> Result<bool, TxError>;
    /// Deletes the runs started before `before`; returns how many.
    async fn delete_before(&self, before: Timestamp) -> Result<u64, TxError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_texts_round_trip() {
        for state in SqlIdentityState::ALL {
            assert_eq!(SqlIdentityState::parse(state.as_str()), Some(*state));
        }
        for ty in SqlParamType::ALL {
            assert_eq!(SqlParamType::parse(ty.as_str()), Some(*ty));
        }
        for outcome in SqlRunOutcome::ALL {
            assert_eq!(SqlRunOutcome::parse(outcome.as_str()), Some(*outcome));
        }
        assert_eq!(SqlVisibility::parse("public"), None);
    }

    #[test]
    fn a_parameter_list_is_a_json_array() {
        let params = vec![
            SavedParam {
                name: "user".to_owned(),
                ty: SqlParamType::String,
                default: None,
            },
            SavedParam {
                name: "limit".to_owned(),
                ty: SqlParamType::Int,
                default: Some("10".to_owned()),
            },
        ];
        let text = serde_json::to_string(&params).unwrap();
        assert_eq!(
            text,
            r#"[{"name":"user","type":"string"},{"name":"limit","type":"int","default":"10"}]"#
        );
        assert_eq!(serde_json::from_str::<Vec<SavedParam>>(&text).unwrap(), params);
    }

    #[test]
    fn an_actor_is_a_user_or_a_key() {
        let id = Uuid::from_bytes([7; 16]);
        assert_eq!(
            SqlActor::from_columns(Some(id), None).unwrap().user_id(),
            Some(UserId::from_uuid(id))
        );
        assert_eq!(
            SqlActor::from_columns(None, Some(id)).unwrap().api_key_id(),
            Some(ApiKeyId::from_uuid(id))
        );
        assert!(SqlActor::from_columns(None, None).is_err());
        assert!(SqlActor::from_columns(Some(id), Some(id)).is_err());
    }
}
