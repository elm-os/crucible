//! `ControlStore` over Postgres: every repository, transactions, leases, claims and the key feed.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use crucible_control::{
    ApiKey, ApiKeyRepo, AuthFlowRepo, AuthMethod, AuthProviderRepo, Claim, Claims, ControlStore,
    ControlTx, Credential, CredentialRepo, Email, InvitationRepo, JsonDoc, KeyChange,
    KeyChangeKind, KeyFeed, KeyScope, KeySnapshot, Lease, LeaseName, Leases, LedgerEntry,
    LifecycleRepo, MembershipRepo, MigrationKind, Migrations, NewJob, OauthAccountRepo,
    OneTimeTokenRepo, Org, OrgMembership, OrgRepo, OrgRole, OrgSettings, Project,
    ProjectMembership, ProjectRepo, ProjectRole, Queue, Repos, Secret, SecretRepo, Session,
    SessionRepo, SigningKey, SigningKeyRepo, SigningKeyState, SigningPurpose, TRANSACTION_CAP,
    TokenHash, TxError, User, UserRepo, UserStatus, duration_micros,
};
use crucible_env::{Env, Instant};
use crucible_records::{ApiKeyId, OrgId, ProjectId, Timestamp, UserId, Uuid};
use deadpool::managed::Object;
use tokio_postgres::Row;
use tokio_postgres::types::{ToSql, Type};

use crate::conn::{Connector, PgConfig, Pool, Pooled, RESERVED_POOL, STATEMENT_TIMEOUT, pool};
use crate::sql::{Cols, P, failure, int};

/// The SQL workspace's repositories.
mod sqlws;

/// How much longer than the server's statement timeout the client waits for an answer before it gives the
/// connection up.
const ANSWER_GRACE: Duration = Duration::from_secs(2);

const BEGIN: &str = "BEGIN ISOLATION LEVEL REPEATABLE READ; \
     SET LOCAL statement_timeout = 10000; \
     SET LOCAL idle_in_transaction_session_timeout = 10000";

#[derive(Clone, Copy)]
enum Which {
    Main,
    Reserved,
}

struct Shared {
    env: Env,
    main: Pool,
    reserved: Pool,
    acquire_timeout: Duration,
}

impl Shared {
    async fn get(&self, which: Which) -> Result<Pooled, TxError> {
        let (pool, name) = match which {
            Which::Main => (&self.main, "main"),
            Which::Reserved => (&self.reserved, "reserved"),
        };
        match self
            .env
            .timeout(self.acquire_timeout, Box::pin(pool.get()))
            .await
        {
            Ok(Ok(connection)) => Ok(connection),
            Ok(Err(deadpool::managed::PoolError::Backend(error))) => Err(error),
            Ok(Err(error)) => Err(TxError::Unavailable(format!(
                "the {name} Postgres pool: {error}"
            ))),
            Err(_) => Err(TxError::Unavailable(format!(
                "no connection of the {name} Postgres pool became free within {:?}",
                self.acquire_timeout
            ))),
        }
    }
}

/// Closes a connection instead of returning it to its pool.
fn discard(connection: Pooled) {
    drop(Object::take(connection));
}

/// One open transaction: its connection, when it began and whether a statement has failed.
struct Tx {
    connection: Option<Pooled>,
    begun: Instant,
    failed: Mutex<Option<TxError>>,
    broken: AtomicBool,
}

impl Tx {
    fn fail(&self, error: &TxError) {
        let mut failed = self.failed.lock().unwrap_or_else(PoisonError::into_inner);
        failed.get_or_insert_with(|| error.clone());
    }

    fn failed(&self) -> Option<TxError> {
        self.failed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The time left under the transaction cap.
    fn remaining(&self, env: &Env) -> Result<Duration, TxError> {
        let spent = env.clock.now().saturating_duration_since(self.begun);
        match TRANSACTION_CAP.checked_sub(spent) {
            Some(left) if !left.is_zero() => Ok(left),
            _ => Err(TxError::Unavailable(
                "the transaction exceeded its 10 s cap".to_owned(),
            )),
        }
    }
}

impl Drop for Tx {
    fn drop(&mut self) {
        // Not committed and not rolled back: closing the connection makes the server roll back.
        if let Some(connection) = self.connection.take() {
            discard(connection);
        }
    }
}

/// Runs statements: each on a pooled connection as its own commit (`tx` is `None`), or inside one
/// transaction.
pub(crate) struct Handle {
    shared: Arc<Shared>,
    tx: Option<Tx>,
}

type Params<'a> = Vec<(&'a (dyn ToSql + Sync), Type)>;

impl Handle {
    /// Runs one statement and returns its rows. `writes` says whether it can change anything.
    async fn query(
        &self,
        which: Which,
        writes: bool,
        sql: &str,
        params: &[P<'_>],
    ) -> Result<Vec<Row>, TxError> {
        let typed: Params<'_> = params
            .iter()
            .map(|param| (param as &(dyn ToSql + Sync), param.ty()))
            .collect();
        let env = &self.shared.env;
        if let Some(tx) = &self.tx {
            if let Some(error) = tx.failed() {
                return Err(error);
            }
            let result = match (&tx.connection, tx.remaining(env)) {
                (Some(connection), Ok(left)) => {
                    match env
                        .timeout(left, Box::pin(connection.query_typed(sql, &typed)))
                        .await
                    {
                        Ok(Ok(rows)) => return Ok(rows),
                        // The transaction is lost with its connection, so nothing was applied.
                        Ok(Err(error)) => failure(&error, false),
                        Err(_) => crate::sql::Failure {
                            error: TxError::Unavailable(
                                "the transaction exceeded its 10 s cap".to_owned(),
                            ),
                            broken: true,
                        },
                    }
                }
                (_, Err(error)) => crate::sql::Failure {
                    error,
                    broken: false,
                },
                (None, _) => crate::sql::Failure {
                    error: TxError::Unavailable("the transaction has ended".to_owned()),
                    broken: false,
                },
            };
            if result.broken {
                tx.broken.store(true, Ordering::SeqCst);
            }
            tx.fail(&result.error);
            return Err(result.error);
        }
        let connection = self.shared.get(which).await?;
        let limit = STATEMENT_TIMEOUT + ANSWER_GRACE;
        match env
            .timeout(limit, Box::pin(connection.query_typed(sql, &typed)))
            .await
        {
            Ok(Ok(rows)) => Ok(rows),
            Ok(Err(error)) => {
                let failure = failure(&error, writes);
                if failure.broken {
                    discard(connection);
                }
                Err(failure.error)
            }
            Err(_) => {
                discard(connection);
                Err(if writes {
                    TxError::CommitUnknown
                } else {
                    TxError::Unavailable(format!("Postgres did not answer within {limit:?}"))
                })
            }
        }
    }

    pub(crate) async fn read(&self, sql: &str, params: &[P<'_>]) -> Result<Vec<Row>, TxError> {
        self.query(Which::Main, false, sql, params).await
    }

    /// Runs one statement on the main pool and returns its rows.
    pub(crate) async fn query_main(
        &self,
        writes: bool,
        sql: &str,
        params: &[P<'_>],
    ) -> Result<Vec<Row>, TxError> {
        self.query(Which::Main, writes, sql, params).await
    }

    /// Runs a writing statement that returns one row per row it changed; answers how many.
    pub(crate) async fn write(&self, sql: &str, params: &[P<'_>]) -> Result<u64, TxError> {
        let rows = self.query(Which::Main, true, sql, params).await?;
        Ok(rows.len() as u64)
    }
}

/// The control store over Postgres (contract 2.7). Clones share the two pools.
#[derive(Clone)]
pub struct PgControlStore {
    handle: Arc<Handle>,
}

impl fmt::Debug for PgControlStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgControlStore")
            .field("main", &self.handle.shared.main.status())
            .field("reserved", &self.handle.shared.reserved.status())
            .finish()
    }
}

impl PgControlStore {
    /// Builds the store with its main pool and its reserved pool of two. No connection is opened yet; every
    /// connection goes through `env.net` and its protocol task runs on `env.spawner`.
    pub fn new(env: &Env, config: &PgConfig) -> Result<Self, TxError> {
        let connector = Connector::new(env, config)?;
        let shared = Shared {
            env: env.clone(),
            main: pool(connector.clone(), config.main_pool)?,
            reserved: pool(connector, RESERVED_POOL)?,
            acquire_timeout: config.acquire_timeout,
        };
        Ok(Self {
            handle: Arc::new(Handle {
                shared: Arc::new(shared),
                tx: None,
            }),
        })
    }

    /// Closes both pools. Calls in flight finish; later calls fail as unavailable.
    pub fn close(&self) {
        self.handle.shared.main.close();
        self.handle.shared.reserved.close();
    }

    /// Empties every table and restarts the feed sequence.
    #[cfg(feature = "test-support")]
    pub async fn reset(&self) -> Result<(), TxError> {
        let sql = "WITH s AS (SELECT setval('key_feed_seq', 1, false)), \
             d1 AS (DELETE FROM key_feed), d2 AS (DELETE FROM api_keys), \
             d3 AS (DELETE FROM project_memberships), d4 AS (DELETE FROM org_memberships), \
             d5 AS (DELETE FROM sessions), d6 AS (DELETE FROM credentials), d7 AS (DELETE FROM users), \
             d8 AS (DELETE FROM projects), d9 AS (DELETE FROM org), d10 AS (DELETE FROM secrets), \
             d11 AS (DELETE FROM signing_keys), d12 AS (DELETE FROM job_leases), \
             d13 AS (DELETE FROM job_cursors), d14 AS (DELETE FROM notification_outbox), \
             d15 AS (DELETE FROM schema_migrations WHERE kind <> 'postgres'), \
             d16 AS (DELETE FROM oauth_accounts), d17 AS (DELETE FROM auth_providers), \
             d18 AS (DELETE FROM auth_flows), d19 AS (DELETE FROM one_time_tokens), \
             d20 AS (DELETE FROM invitations), d21 AS (DELETE FROM deletion_node_acks), \
             d22 AS (DELETE FROM deletion_requests), d23 AS (DELETE FROM lifecycle_actions), \
             d24 AS (DELETE FROM retention_floors), d25 AS (DELETE FROM nodes), \
             s2 AS (SELECT setval('deletion_seq', 1, false)) \
             SELECT * FROM s, s2";
        self.handle.query(Which::Main, true, sql, &[]).await?;
        Ok(())
    }

    /// A server setting as the pooled connections see it, or as a transaction sees it.
    #[cfg(feature = "test-support")]
    pub async fn current_setting(
        &self,
        name: &str,
        in_transaction: bool,
    ) -> Result<String, TxError> {
        let sql = "SELECT current_setting($1)";
        let rows = if in_transaction {
            let tx = self.begin_tx().await?;
            let rows = tx
                .handle
                .query(Which::Main, false, sql, &[P::Text(name)])
                .await?;
            Box::new(tx).rollback().await;
            rows
        } else {
            self.handle
                .query(Which::Main, false, sql, &[P::Text(name)])
                .await?
        };
        first(&rows, |c| c.text())?.ok_or_else(|| TxError::Invalid("no such setting".into()))
    }

    /// Terminates every other connection of this database, as a restart or a failover does. Returns how
    /// many.
    #[cfg(feature = "test-support")]
    pub async fn terminate_other_backends(&self) -> Result<u64, TxError> {
        let rows = self
            .handle
            .query(
                Which::Main,
                false,
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                 WHERE datname = current_database() AND pid <> pg_backend_pid()",
                &[],
            )
            .await?;
        Ok(rows.len() as u64)
    }

    async fn begin_tx(&self) -> Result<PgTx, TxError> {
        let shared = &self.handle.shared;
        let connection = shared.get(Which::Main).await?;
        let begun = shared.env.clock.now();
        match shared
            .env
            .timeout(STATEMENT_TIMEOUT, connection.batch_execute(BEGIN))
            .await
        {
            Ok(Ok(())) => Ok(PgTx {
                handle: Handle {
                    shared: Arc::clone(shared),
                    tx: Some(Tx {
                        connection: Some(connection),
                        begun,
                        failed: Mutex::new(None),
                        broken: AtomicBool::new(false),
                    }),
                },
            }),
            Ok(Err(error)) => {
                discard(connection);
                Err(failure(&error, false).error)
            }
            Err(_) => {
                discard(connection);
                Err(TxError::Unavailable(
                    "Postgres did not open a transaction within 10 s".to_owned(),
                ))
            }
        }
    }
}

/// One open transaction on a main-pool connection.
pub struct PgTx {
    handle: Handle,
}

impl fmt::Debug for PgTx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PgTx")
    }
}

#[async_trait]
impl ControlStore for PgControlStore {
    async fn begin(&self) -> Result<Box<dyn ControlTx>, TxError> {
        Ok(Box::new(self.begin_tx().await?))
    }

    async fn db_now(&self) -> Result<Timestamp, TxError> {
        let rows = self
            .handle
            .query(Which::Reserved, false, "SELECT clock_timestamp()", &[])
            .await?;
        match rows.first() {
            Some(row) => Cols::new(row).ts(),
            None => Err(TxError::Invalid("the clock query returned no row".into())),
        }
    }
}

impl PgTx {
    /// Rolls back on the server, or closes the connection when that is not possible.
    async fn end(shared: &Shared, mut tx: Tx) {
        let Some(connection) = tx.connection.take() else {
            return;
        };
        if tx.broken.load(Ordering::SeqCst) {
            discard(connection);
            return;
        }
        match shared
            .env
            .timeout(ANSWER_GRACE, connection.batch_execute("ROLLBACK"))
            .await
        {
            Ok(Ok(())) => {}
            _ => discard(connection),
        }
    }
}

#[async_trait]
impl ControlTx for PgTx {
    async fn fence(&self, lease: &Lease) -> Result<(), TxError> {
        let rows = self
            .handle
            .query(
                Which::Main,
                false,
                "SELECT 1 FROM job_leases WHERE name = $1 AND holder = $2 AND fencing_token = $3 \
                 FOR SHARE",
                &[
                    P::Text(lease.name.as_str()),
                    P::Text(&lease.holder),
                    int(lease.fencing_token)?,
                ],
            )
            .await?;
        if rows.is_empty() {
            let error = TxError::Fenced;
            if let Some(tx) = &self.handle.tx {
                tx.fail(&error);
            }
            return Err(error);
        }
        Ok(())
    }

    async fn commit(mut self: Box<Self>) -> Result<(), TxError> {
        let shared = Arc::clone(&self.handle.shared);
        let Some(mut tx) = self.handle.tx.take() else {
            return Err(TxError::Unavailable("the transaction has ended".into()));
        };
        let refused = tx.failed().or_else(|| tx.remaining(&shared.env).err());
        if let Some(error) = refused {
            Self::end(&shared, tx).await;
            return Err(error);
        }
        let Some(connection) = tx.connection.take() else {
            return Err(TxError::Unavailable("the transaction has ended".into()));
        };
        match shared
            .env
            .timeout(STATEMENT_TIMEOUT, connection.batch_execute("COMMIT"))
            .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) if error.as_db_error().is_some() => {
                // The server answered: the transaction was not committed and has ended.
                discard(connection);
                Err(failure(&error, false).error)
            }
            _ => {
                discard(connection);
                Err(TxError::CommitUnknown)
            }
        }
    }

    async fn rollback(mut self: Box<Self>) {
        let shared = Arc::clone(&self.handle.shared);
        if let Some(tx) = self.handle.tx.take() {
            Self::end(&shared, tx).await;
        }
    }
}

macro_rules! repos {
    ($ty:ty, $($deref:tt)*) => {
        impl Repos for $ty {
            fn org(&self) -> &dyn OrgRepo {
                $($deref)* self.handle
            }
            fn users(&self) -> &dyn UserRepo {
                $($deref)* self.handle
            }
            fn credentials(&self) -> &dyn CredentialRepo {
                $($deref)* self.handle
            }
            fn sessions(&self) -> &dyn SessionRepo {
                $($deref)* self.handle
            }
            fn projects(&self) -> &dyn ProjectRepo {
                $($deref)* self.handle
            }
            fn memberships(&self) -> &dyn MembershipRepo {
                $($deref)* self.handle
            }
            fn api_keys(&self) -> &dyn ApiKeyRepo {
                $($deref)* self.handle
            }
            fn key_feed(&self) -> &dyn KeyFeed {
                $($deref)* self.handle
            }
            fn signing_keys(&self) -> &dyn SigningKeyRepo {
                $($deref)* self.handle
            }
            fn secrets(&self) -> &dyn SecretRepo {
                $($deref)* self.handle
            }
            fn auth_providers(&self) -> &dyn AuthProviderRepo {
                $($deref)* self.handle
            }
            fn oauth_accounts(&self) -> &dyn OauthAccountRepo {
                $($deref)* self.handle
            }
            fn auth_flows(&self) -> &dyn AuthFlowRepo {
                $($deref)* self.handle
            }
            fn one_time_tokens(&self) -> &dyn OneTimeTokenRepo {
                $($deref)* self.handle
            }
            fn invitations(&self) -> &dyn InvitationRepo {
                $($deref)* self.handle
            }
            fn leases(&self) -> &dyn Leases {
                $($deref)* self.handle
            }
            fn claims(&self) -> &dyn Claims {
                $($deref)* self.handle
            }
            fn migrations(&self) -> &dyn Migrations {
                $($deref)* self.handle
            }
            fn lifecycle(&self) -> &dyn LifecycleRepo {
                $($deref)* self.handle
            }
            fn sql_identities(&self) -> &dyn crucible_control::SqlIdentityRepo {
                $($deref)* self.handle
            }
            fn saved_queries(&self) -> &dyn crucible_control::SavedQueryRepo {
                $($deref)* self.handle
            }
            fn sql_runs(&self) -> &dyn crucible_control::SqlRunRepo {
                $($deref)* self.handle
            }
        }
    };
}

repos!(PgControlStore, &*);
repos!(PgTx, &);

pub(crate) fn first<T>(
    rows: &[Row],
    decode: impl Fn(&mut Cols<'_>) -> Result<T, TxError>,
) -> Result<Option<T>, TxError> {
    rows.first()
        .map(|row| decode(&mut Cols::new(row)))
        .transpose()
}

pub(crate) fn every<T>(
    rows: &[Row],
    decode: impl Fn(&mut Cols<'_>) -> Result<T, TxError>,
) -> Result<Vec<T>, TxError> {
    rows.iter().map(|row| decode(&mut Cols::new(row))).collect()
}

fn micros(d: Duration) -> P<'static> {
    P::Int(duration_micros(d))
}

fn org(c: &mut Cols<'_>) -> Result<Org, TxError> {
    Ok(Org {
        id: OrgId::from_uuid(c.uuid()?),
        name: c.text()?,
        settings: OrgSettings::parse(&c.text()?)?,
        version: c.u64()?,
    })
}

#[async_trait]
impl OrgRepo for Handle {
    async fn get(&self) -> Result<Option<Org>, TxError> {
        let rows = self
            .read("SELECT id, name, settings::text, version FROM org", &[])
            .await?;
        first(&rows, org)
    }

    async fn insert(&self, org: &Org) -> Result<(), TxError> {
        self.write(
            "INSERT INTO org (id, singleton, name, settings, version) \
             VALUES ($1, true, $2, $3::jsonb, $4) RETURNING 1",
            &[
                P::Uuid(org.id.uuid()),
                P::Text(&org.name),
                P::Text(&org.settings.to_text()),
                int(org.version)?,
            ],
        )
        .await?;
        Ok(())
    }

    async fn update_if_version(&self, org: &Org, expected: u64) -> Result<bool, TxError> {
        let changed = self
            .write(
                "UPDATE org SET name = $2, settings = $3::jsonb, version = version + 1 \
                 WHERE id = $1 AND version = $4 RETURNING 1",
                &[
                    P::Uuid(org.id.uuid()),
                    P::Text(&org.name),
                    P::Text(&org.settings.to_text()),
                    int(expected)?,
                ],
            )
            .await?;
        Ok(changed == 1)
    }
}

const USER: &str = "id, email, name, status, email_verified_at";

fn user(c: &mut Cols<'_>) -> Result<User, TxError> {
    Ok(User {
        id: UserId::from_uuid(c.uuid()?),
        email: c.parsed(Email::new)?,
        name: c.text()?,
        status: c.parsed(UserStatus::parse)?,
        email_verified_at: c.opt_ts()?,
    })
}

#[async_trait]
impl UserRepo for Handle {
    async fn get(&self, id: UserId) -> Result<Option<User>, TxError> {
        let sql = format!("SELECT {USER} FROM users WHERE id = $1");
        first(&self.read(&sql, &[P::Uuid(id.uuid())]).await?, user)
    }

    async fn get_by_email(&self, email: &str) -> Result<Option<User>, TxError> {
        let sql = format!("SELECT {USER} FROM users WHERE email = $1");
        first(&self.read(&sql, &[P::Text(email)]).await?, user)
    }

    async fn list(&self) -> Result<Vec<User>, TxError> {
        let sql = format!("SELECT {USER} FROM users ORDER BY id");
        every(&self.read(&sql, &[]).await?, user)
    }

    async fn insert(&self, user: &User) -> Result<(), TxError> {
        let sql = format!("INSERT INTO users ({USER}) VALUES ($1, $2, $3, $4, $5) RETURNING 1");
        self.write(
            &sql,
            &[
                P::Uuid(user.id.uuid()),
                P::Text(user.email.as_str()),
                P::Text(&user.name),
                P::Text(user.status.as_str()),
                P::OptTs(user.email_verified_at),
            ],
        )
        .await?;
        Ok(())
    }

    async fn update(&self, user: &User) -> Result<bool, TxError> {
        let changed = self
            .write(
                "UPDATE users SET email = $2, name = $3, status = $4, email_verified_at = $5 \
                 WHERE id = $1 RETURNING 1",
                &[
                    P::Uuid(user.id.uuid()),
                    P::Text(user.email.as_str()),
                    P::Text(&user.name),
                    P::Text(user.status.as_str()),
                    P::OptTs(user.email_verified_at),
                ],
            )
            .await?;
        Ok(changed == 1)
    }

    async fn delete(&self, id: UserId) -> Result<bool, TxError> {
        let deleted = self
            .write(
                "DELETE FROM users WHERE id = $1 RETURNING 1",
                &[P::Uuid(id.uuid())],
            )
            .await?;
        Ok(deleted == 1)
    }
}

fn credential(c: &mut Cols<'_>) -> Result<Credential, TxError> {
    Ok(Credential {
        user_id: UserId::from_uuid(c.uuid()?),
        password_hash: c.text()?,
        params_version: c.u32()?,
        failed_count: c.u32()?,
        locked_until: c.opt_ts()?,
    })
}

#[async_trait]
impl CredentialRepo for Handle {
    async fn get(&self, user_id: UserId) -> Result<Option<Credential>, TxError> {
        let rows = self
            .read(
                "SELECT user_id, password_hash, params_version, failed_count, locked_until \
                 FROM credentials WHERE user_id = $1",
                &[P::Uuid(user_id.uuid())],
            )
            .await?;
        first(&rows, credential)
    }

    async fn put(&self, credential: &Credential) -> Result<(), TxError> {
        self.write(
            "INSERT INTO credentials (user_id, password_hash, params_version, failed_count, locked_until) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (user_id) DO UPDATE SET password_hash = EXCLUDED.password_hash, \
             params_version = EXCLUDED.params_version, failed_count = EXCLUDED.failed_count, \
             locked_until = EXCLUDED.locked_until RETURNING 1",
            &[
                P::Uuid(credential.user_id.uuid()),
                P::Text(&credential.password_hash),
                P::Int4(credential.params_version),
                P::Int4(credential.failed_count),
                P::OptTs(credential.locked_until),
            ],
        )
        .await?;
        Ok(())
    }

    async fn delete(&self, user_id: UserId) -> Result<bool, TxError> {
        let deleted = self
            .write(
                "DELETE FROM credentials WHERE user_id = $1 RETURNING 1",
                &[P::Uuid(user_id.uuid())],
            )
            .await?;
        Ok(deleted == 1)
    }
}

const SESSION: &str = "token_hash, user_id, csrf_token, idle_expires_at, absolute_expires_at, auth_method, revoked_at";

fn session(c: &mut Cols<'_>) -> Result<Session, TxError> {
    Ok(Session {
        token_hash: c.array()?,
        user_id: UserId::from_uuid(c.uuid()?),
        csrf_token: c.text()?,
        idle_expires_at: c.ts()?,
        absolute_expires_at: c.ts()?,
        auth_method: c.parsed(AuthMethod::parse)?,
        revoked_at: c.opt_ts()?,
    })
}

#[async_trait]
impl SessionRepo for Handle {
    async fn get(&self, token_hash: &TokenHash) -> Result<Option<Session>, TxError> {
        let sql = format!("SELECT {SESSION} FROM sessions WHERE token_hash = $1");
        let rows = self
            .query(Which::Reserved, false, &sql, &[P::Bytes(token_hash)])
            .await?;
        first(&rows, session)
    }

    async fn insert(&self, session: &Session) -> Result<(), TxError> {
        let sql = format!(
            "INSERT INTO sessions ({SESSION}) VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING 1"
        );
        self.write(
            &sql,
            &[
                P::Bytes(&session.token_hash),
                P::Uuid(session.user_id.uuid()),
                P::Text(&session.csrf_token),
                P::Ts(session.idle_expires_at),
                P::Ts(session.absolute_expires_at),
                P::Text(session.auth_method.as_str()),
                P::OptTs(session.revoked_at),
            ],
        )
        .await?;
        Ok(())
    }

    async fn touch(
        &self,
        token_hash: &TokenHash,
        idle_expires_at: Timestamp,
    ) -> Result<bool, TxError> {
        let changed = self
            .write(
                "UPDATE sessions SET idle_expires_at = $2 WHERE token_hash = $1 RETURNING 1",
                &[P::Bytes(token_hash), P::Ts(idle_expires_at)],
            )
            .await?;
        Ok(changed == 1)
    }

    async fn revoke(&self, token_hash: &TokenHash, at: Timestamp) -> Result<bool, TxError> {
        let changed = self
            .write(
                "UPDATE sessions SET revoked_at = $2 WHERE token_hash = $1 AND revoked_at IS NULL \
                 RETURNING 1",
                &[P::Bytes(token_hash), P::Ts(at)],
            )
            .await?;
        Ok(changed == 1)
    }

    async fn revoke_user(
        &self,
        user_id: UserId,
        keep: Option<&TokenHash>,
        at: Timestamp,
    ) -> Result<u64, TxError> {
        self.write(
            "UPDATE sessions SET revoked_at = $2 WHERE user_id = $1 AND revoked_at IS NULL \
             AND ($3::bytea IS NULL OR token_hash <> $3) RETURNING 1",
            &[
                P::Uuid(user_id.uuid()),
                P::Ts(at),
                P::OptBytes(keep.map(<[u8; 32]>::as_slice)),
            ],
        )
        .await
    }

    async fn delete_expired(&self, now: Timestamp) -> Result<u64, TxError> {
        self.write(
            "DELETE FROM sessions WHERE absolute_expires_at < $1 RETURNING 1",
            &[P::Ts(now)],
        )
        .await
    }
}

const PROJECT: &str = "id, name, slug, settings::text, retention_days, payload_retention_days, \
     mcp_content_retention_days, config_version, version, deleted_at";

fn project(c: &mut Cols<'_>) -> Result<Project, TxError> {
    Ok(Project {
        id: ProjectId::from_uuid(c.uuid()?),
        name: c.text()?,
        slug: c.text()?,
        settings: JsonDoc::parse(&c.text()?)?,
        retention_days: c.opt_u32()?,
        payload_retention_days: c.opt_u32()?,
        mcp_content_retention_days: c.opt_u32()?,
        config_version: c.u64()?,
        version: c.u64()?,
        deleted_at: c.opt_ts()?,
    })
}

#[async_trait]
impl ProjectRepo for Handle {
    async fn get(&self, id: ProjectId) -> Result<Option<Project>, TxError> {
        let sql = format!("SELECT {PROJECT} FROM projects WHERE id = $1");
        first(&self.read(&sql, &[P::Uuid(id.uuid())]).await?, project)
    }

    async fn get_by_slug(&self, slug: &str) -> Result<Option<Project>, TxError> {
        let sql = format!("SELECT {PROJECT} FROM projects WHERE slug = $1");
        first(&self.read(&sql, &[P::Text(slug)]).await?, project)
    }

    async fn list(&self) -> Result<Vec<Project>, TxError> {
        let sql = format!("SELECT {PROJECT} FROM projects WHERE deleted_at IS NULL ORDER BY id");
        every(&self.read(&sql, &[]).await?, project)
    }

    async fn insert(&self, project: &Project) -> Result<(), TxError> {
        self.write(
            "INSERT INTO projects (id, name, slug, settings, retention_days, payload_retention_days, \
             mcp_content_retention_days, config_version, version, deleted_at) \
             VALUES ($1, $2, $3, $4::jsonb, $5, $6, $7, $8, $9, $10) RETURNING 1",
            &[
                P::Uuid(project.id.uuid()),
                P::Text(&project.name),
                P::Text(&project.slug),
                P::Text(&project.settings.to_text()),
                P::OptInt4(project.retention_days),
                P::OptInt4(project.payload_retention_days),
                P::OptInt4(project.mcp_content_retention_days),
                int(project.config_version)?,
                int(project.version)?,
                P::OptTs(project.deleted_at),
            ],
        )
        .await?;
        Ok(())
    }

    async fn update_if_version(&self, project: &Project, expected: u64) -> Result<bool, TxError> {
        let changed = self
            .write(
                "UPDATE projects SET name = $2, slug = $3, settings = $4::jsonb, retention_days = $5, \
                 payload_retention_days = $6, mcp_content_retention_days = $7, config_version = $8, \
                 version = version + 1, deleted_at = $9 WHERE id = $1 AND version = $10 RETURNING 1",
                &[
                    P::Uuid(project.id.uuid()),
                    P::Text(&project.name),
                    P::Text(&project.slug),
                    P::Text(&project.settings.to_text()),
                    P::OptInt4(project.retention_days),
                    P::OptInt4(project.payload_retention_days),
                    P::OptInt4(project.mcp_content_retention_days),
                    int(project.config_version)?,
                    P::OptTs(project.deleted_at),
                    int(expected)?,
                ],
            )
            .await?;
        Ok(changed == 1)
    }
}

fn project_member(c: &mut Cols<'_>) -> Result<ProjectMembership, TxError> {
    Ok(ProjectMembership {
        project_id: ProjectId::from_uuid(c.uuid()?),
        user_id: UserId::from_uuid(c.uuid()?),
        role: c.parsed(ProjectRole::parse)?,
    })
}

#[async_trait]
impl MembershipRepo for Handle {
    async fn org_role(&self, user_id: UserId) -> Result<Option<OrgRole>, TxError> {
        let rows = self
            .read(
                "SELECT role FROM org_memberships WHERE user_id = $1",
                &[P::Uuid(user_id.uuid())],
            )
            .await?;
        first(&rows, |c| c.parsed(OrgRole::parse))
    }

    async fn list_org(&self) -> Result<Vec<OrgMembership>, TxError> {
        let rows = self
            .read(
                "SELECT user_id, role FROM org_memberships ORDER BY user_id",
                &[],
            )
            .await?;
        every(&rows, |c| {
            Ok(OrgMembership {
                user_id: UserId::from_uuid(c.uuid()?),
                role: c.parsed(OrgRole::parse)?,
            })
        })
    }

    async fn put_org(&self, membership: OrgMembership) -> Result<(), TxError> {
        self.write(
            "INSERT INTO org_memberships (user_id, role) VALUES ($1, $2) \
             ON CONFLICT (user_id) DO UPDATE SET role = EXCLUDED.role RETURNING 1",
            &[
                P::Uuid(membership.user_id.uuid()),
                P::Text(membership.role.as_str()),
            ],
        )
        .await?;
        Ok(())
    }

    async fn delete_org(&self, user_id: UserId) -> Result<bool, TxError> {
        let deleted = self
            .write(
                "DELETE FROM org_memberships WHERE user_id = $1 RETURNING 1",
                &[P::Uuid(user_id.uuid())],
            )
            .await?;
        Ok(deleted == 1)
    }

    async fn list_for_user(&self, user_id: UserId) -> Result<Vec<ProjectMembership>, TxError> {
        let rows = self
            .read(
                "SELECT project_id, user_id, role FROM project_memberships WHERE user_id = $1 \
                 ORDER BY project_id",
                &[P::Uuid(user_id.uuid())],
            )
            .await?;
        every(&rows, project_member)
    }

    async fn list_project(&self, project_id: ProjectId) -> Result<Vec<ProjectMembership>, TxError> {
        let rows = self
            .read(
                "SELECT project_id, user_id, role FROM project_memberships WHERE project_id = $1 \
                 ORDER BY user_id",
                &[P::Uuid(project_id.uuid())],
            )
            .await?;
        every(&rows, project_member)
    }

    async fn put_project(&self, membership: ProjectMembership) -> Result<(), TxError> {
        self.write(
            "INSERT INTO project_memberships (project_id, user_id, role) VALUES ($1, $2, $3) \
             ON CONFLICT (project_id, user_id) DO UPDATE SET role = EXCLUDED.role RETURNING 1",
            &[
                P::Uuid(membership.project_id.uuid()),
                P::Uuid(membership.user_id.uuid()),
                P::Text(membership.role.as_str()),
            ],
        )
        .await?;
        Ok(())
    }

    async fn delete_project(
        &self,
        project_id: ProjectId,
        user_id: UserId,
    ) -> Result<bool, TxError> {
        let deleted = self
            .write(
                "DELETE FROM project_memberships WHERE project_id = $1 AND user_id = $2 RETURNING 1",
                &[P::Uuid(project_id.uuid()), P::Uuid(user_id.uuid())],
            )
            .await?;
        Ok(deleted == 1)
    }
}

const API_KEY: &str = "k.id, k.project_id, k.public_key, k.secret_salt, k.secret_hash, k.lookup_tag, \
     k.scopes, k.import, k.note, k.expires_at, k.last_used_at, k.revoked_at, k.version";

fn api_key(c: &mut Cols<'_>) -> Result<ApiKey, TxError> {
    Ok(ApiKey {
        id: ApiKeyId::from_uuid(c.uuid()?),
        project_id: ProjectId::from_uuid(c.uuid()?),
        public_key: c.text()?,
        secret_salt: c.array()?,
        secret_hash: c.array()?,
        lookup_tag: c.array()?,
        scopes: c
            .texts()?
            .iter()
            .map(|scope| {
                KeyScope::parse(scope)
                    .ok_or_else(|| TxError::Invalid(format!("unknown key scope {scope:?}")))
            })
            .collect::<Result<_, _>>()?,
        import: c.bool()?,
        note: c.text()?,
        expires_at: c.opt_ts()?,
        last_used_at: c.opt_ts()?,
        revoked_at: c.opt_ts()?,
        version: c.u64()?,
    })
}

/// The feed row of a key change, written by the statement that changes the key (contract 11.8).
const FEED: &str = "INSERT INTO key_feed (seq, key_id, change, key_version, at) \
     SELECT nextval('key_feed_seq'), id, $1, version, clock_timestamp() FROM k RETURNING 1";

#[async_trait]
impl ApiKeyRepo for Handle {
    async fn get(&self, id: ApiKeyId) -> Result<Option<ApiKey>, TxError> {
        let sql = format!("SELECT {API_KEY} FROM api_keys k WHERE k.id = $1");
        first(&self.read(&sql, &[P::Uuid(id.uuid())]).await?, api_key)
    }

    async fn list(&self, project_id: ProjectId) -> Result<Vec<ApiKey>, TxError> {
        let sql = format!("SELECT {API_KEY} FROM api_keys k WHERE k.project_id = $1 ORDER BY k.id");
        every(
            &self.read(&sql, &[P::Uuid(project_id.uuid())]).await?,
            api_key,
        )
    }

    async fn insert(&self, key: &ApiKey) -> Result<(), TxError> {
        let sql = format!(
            "WITH k AS (INSERT INTO api_keys (id, project_id, public_key, secret_salt, secret_hash, \
             lookup_tag, scopes, import, note, expires_at, last_used_at, revoked_at, version) \
             VALUES ($2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) RETURNING id, version) \
             {FEED}"
        );
        self.write(
            &sql,
            &[
                P::Text(KeyChangeKind::Created.as_str()),
                P::Uuid(key.id.uuid()),
                P::Uuid(key.project_id.uuid()),
                P::Text(&key.public_key),
                P::Bytes(&key.secret_salt),
                P::Bytes(&key.secret_hash),
                P::Bytes(&key.lookup_tag),
                P::TextArray(key.scopes.iter().map(KeyScope::as_str).collect()),
                P::Bool(key.import),
                P::Text(&key.note),
                P::OptTs(key.expires_at),
                P::OptTs(key.last_used_at),
                P::OptTs(key.revoked_at),
                int(key.version)?,
            ],
        )
        .await?;
        Ok(())
    }

    async fn update_if_version(
        &self,
        key: &ApiKey,
        expected: u64,
        change: KeyChangeKind,
    ) -> Result<bool, TxError> {
        let sql = format!(
            "WITH k AS (UPDATE api_keys SET project_id = $3, public_key = $4, secret_salt = $5, \
             secret_hash = $6, lookup_tag = $7, scopes = $8, import = $9, note = $10, expires_at = $11, \
             revoked_at = $12, version = version + 1 WHERE id = $2 AND version = $13 \
             RETURNING id, version) {FEED}"
        );
        let changed = self
            .write(
                &sql,
                &[
                    P::Text(change.as_str()),
                    P::Uuid(key.id.uuid()),
                    P::Uuid(key.project_id.uuid()),
                    P::Text(&key.public_key),
                    P::Bytes(&key.secret_salt),
                    P::Bytes(&key.secret_hash),
                    P::Bytes(&key.lookup_tag),
                    P::TextArray(key.scopes.iter().map(KeyScope::as_str).collect()),
                    P::Bool(key.import),
                    P::Text(&key.note),
                    P::OptTs(key.expires_at),
                    P::OptTs(key.revoked_at),
                    int(expected)?,
                ],
            )
            .await?;
        Ok(changed == 1)
    }

    async fn set_last_used(&self, id: ApiKeyId, at: Timestamp) -> Result<bool, TxError> {
        let changed = self
            .write(
                "UPDATE api_keys SET last_used_at = $2 WHERE id = $1 RETURNING 1",
                &[P::Uuid(id.uuid()), P::Ts(at)],
            )
            .await?;
        Ok(changed == 1)
    }
}

#[async_trait]
impl KeyFeed for Handle {
    async fn poll(&self, after_seq: u64, overlap: Duration) -> Result<Vec<KeyChange>, TxError> {
        let sql = format!(
            "SELECT f.seq, f.change, f.key_version, f.at, {API_KEY} FROM key_feed f \
             JOIN api_keys k ON k.id = f.key_id \
             WHERE f.seq > $1 OR f.at > clock_timestamp() - $2 * interval '1 microsecond' \
             ORDER BY f.seq"
        );
        let rows = self
            .query(
                Which::Reserved,
                false,
                &sql,
                &[int(after_seq)?, micros(overlap)],
            )
            .await?;
        every(&rows, |c| {
            Ok(KeyChange {
                seq: c.u64()?,
                change: c.parsed(KeyChangeKind::parse)?,
                key_version: c.u64()?,
                at: c.ts()?,
                key: api_key(c)?,
            })
        })
    }

    async fn snapshot(&self) -> Result<KeySnapshot, TxError> {
        // The sequence number is read first: a key that changes between the two statements is then in the
        // snapshot at its newer version, and the poll that follows applies the change again, which is
        // harmless.
        let rows = self
            .query(
                Which::Reserved,
                false,
                "SELECT COALESCE(max(seq), 0) FROM key_feed",
                &[],
            )
            .await?;
        let max_seq = first(&rows, |c| c.u64())?.unwrap_or(0);
        let sql = format!("SELECT {API_KEY} FROM api_keys k ORDER BY k.id");
        let rows = self.query(Which::Reserved, false, &sql, &[]).await?;
        Ok(KeySnapshot {
            max_seq,
            keys: every(&rows, api_key)?,
        })
    }

    async fn prune(&self, older_than: Duration) -> Result<u64, TxError> {
        let rows = self
            .query(
                Which::Reserved,
                true,
                "DELETE FROM key_feed WHERE at < clock_timestamp() - $1 * interval '1 microsecond' \
                 RETURNING 1",
                &[micros(older_than)],
            )
            .await?;
        Ok(rows.len() as u64)
    }
}

const SIGNING_KEY: &str = "kid, purpose, sealed_key, state, created_at";

fn signing_key(c: &mut Cols<'_>) -> Result<SigningKey, TxError> {
    Ok(SigningKey {
        kid: c.text()?,
        purpose: c.parsed(SigningPurpose::parse)?,
        sealed_key: c.bytes()?,
        state: c.parsed(SigningKeyState::parse)?,
        created_at: c.ts()?,
    })
}

#[async_trait]
impl SigningKeyRepo for Handle {
    async fn get(&self, kid: &str) -> Result<Option<SigningKey>, TxError> {
        let sql = format!("SELECT {SIGNING_KEY} FROM signing_keys WHERE kid = $1");
        first(&self.read(&sql, &[P::Text(kid)]).await?, signing_key)
    }

    async fn list(&self, purpose: SigningPurpose) -> Result<Vec<SigningKey>, TxError> {
        let sql = format!(
            "SELECT {SIGNING_KEY} FROM signing_keys WHERE purpose = $1 ORDER BY created_at, kid"
        );
        every(
            &self.read(&sql, &[P::Text(purpose.as_str())]).await?,
            signing_key,
        )
    }

    async fn insert(&self, key: &SigningKey) -> Result<(), TxError> {
        let sql = format!(
            "INSERT INTO signing_keys ({SIGNING_KEY}) VALUES ($1, $2, $3, $4, $5) RETURNING 1"
        );
        self.write(
            &sql,
            &[
                P::Text(&key.kid),
                P::Text(key.purpose.as_str()),
                P::Bytes(&key.sealed_key),
                P::Text(key.state.as_str()),
                P::Ts(key.created_at),
            ],
        )
        .await?;
        Ok(())
    }

    async fn set_state(&self, kid: &str, state: SigningKeyState) -> Result<bool, TxError> {
        let changed = self
            .write(
                "UPDATE signing_keys SET state = $2 WHERE kid = $1 RETURNING 1",
                &[P::Text(kid), P::Text(state.as_str())],
            )
            .await?;
        Ok(changed == 1)
    }
}

#[async_trait]
impl SecretRepo for Handle {
    async fn get(&self, id: Uuid) -> Result<Option<Secret>, TxError> {
        let rows = self
            .read(
                "SELECT id, scope, kind, key_id, nonce, ciphertext FROM secrets WHERE id = $1",
                &[P::Uuid(id)],
            )
            .await?;
        first(&rows, |c| {
            Ok(Secret {
                id: c.uuid()?,
                scope: c.text()?,
                kind: c.text()?,
                key_id: c.text()?,
                nonce: c.bytes()?,
                ciphertext: c.bytes()?,
            })
        })
    }

    async fn insert(&self, secret: &Secret) -> Result<(), TxError> {
        self.write(
            "INSERT INTO secrets (id, scope, kind, key_id, nonce, ciphertext) \
             VALUES ($1, $2, $3, $4, $5, $6) RETURNING 1",
            &[
                P::Uuid(secret.id),
                P::Text(&secret.scope),
                P::Text(&secret.kind),
                P::Text(&secret.key_id),
                P::Bytes(&secret.nonce),
                P::Bytes(&secret.ciphertext),
            ],
        )
        .await?;
        Ok(())
    }

    async fn delete(&self, id: Uuid) -> Result<bool, TxError> {
        let deleted = self
            .write(
                "DELETE FROM secrets WHERE id = $1 RETURNING 1",
                &[P::Uuid(id)],
            )
            .await?;
        Ok(deleted == 1)
    }
}

fn lease(c: &mut Cols<'_>) -> Result<Lease, TxError> {
    Ok(Lease {
        name: c.parsed(LeaseName::parse)?,
        holder: c.text()?,
        fencing_token: c.u64()?,
        expires_at: c.ts()?,
    })
}

const LEASE: &str = "name, holder, fencing_token, expires_at";

#[async_trait]
impl Leases for Handle {
    async fn acquire(
        &self,
        name: LeaseName,
        holder: &str,
        ttl: Duration,
    ) -> Result<Option<Lease>, TxError> {
        // One statement (contract 11.7). The expiry is the database clock.
        let sql = format!(
            "INSERT INTO job_leases AS l (name, holder, fencing_token, expires_at) \
             VALUES ($1, $2, 1, clock_timestamp() + $3 * interval '1 microsecond') \
             ON CONFLICT (name) DO UPDATE SET holder = EXCLUDED.holder, \
             fencing_token = l.fencing_token + 1, expires_at = EXCLUDED.expires_at \
             WHERE l.expires_at < clock_timestamp() RETURNING {LEASE}"
        );
        let rows = self
            .query(
                Which::Reserved,
                true,
                &sql,
                &[P::Text(name.as_str()), P::Text(holder), micros(ttl)],
            )
            .await?;
        first(&rows, lease)
    }

    async fn renew(&self, held: &Lease, ttl: Duration) -> Result<Option<Lease>, TxError> {
        let sql = format!(
            "UPDATE job_leases SET expires_at = clock_timestamp() + $4 * interval '1 microsecond' \
             WHERE name = $1 AND holder = $2 AND fencing_token = $3 AND expires_at >= clock_timestamp() \
             RETURNING {LEASE}"
        );
        let rows = self
            .query(
                Which::Reserved,
                true,
                &sql,
                &[
                    P::Text(held.name.as_str()),
                    P::Text(&held.holder),
                    int(held.fencing_token)?,
                    micros(ttl),
                ],
            )
            .await?;
        first(&rows, lease)
    }

    async fn release(&self, held: &Lease) -> Result<bool, TxError> {
        let rows = self
            .query(
                Which::Reserved,
                true,
                "UPDATE job_leases SET expires_at = to_timestamp(0) \
                 WHERE name = $1 AND holder = $2 AND fencing_token = $3 RETURNING 1",
                &[
                    P::Text(held.name.as_str()),
                    P::Text(&held.holder),
                    int(held.fencing_token)?,
                ],
            )
            .await?;
        Ok(rows.len() == 1)
    }

    async fn get(&self, name: LeaseName) -> Result<Option<Lease>, TxError> {
        let sql = format!("SELECT {LEASE} FROM job_leases WHERE name = $1");
        let rows = self
            .query(Which::Reserved, false, &sql, &[P::Text(name.as_str())])
            .await?;
        first(&rows, lease)
    }

    async fn cursor(&self, name: LeaseName) -> Result<Option<String>, TxError> {
        let rows = self
            .query(
                Which::Reserved,
                false,
                "SELECT cursor FROM job_cursors WHERE name = $1",
                &[P::Text(name.as_str())],
            )
            .await?;
        first(&rows, |c| c.text())
    }

    async fn set_cursor(&self, held: &Lease, cursor: &str) -> Result<(), TxError> {
        // The lease row is share-locked by the statement, so an acquisition cannot slip between the check
        // and the write.
        let rows = self
            .query(
                Which::Reserved,
                true,
                "WITH held AS (SELECT name FROM job_leases \
                 WHERE name = $1 AND holder = $2 AND fencing_token = $3 FOR SHARE) \
                 INSERT INTO job_cursors (name, cursor) SELECT name, $4 FROM held \
                 ON CONFLICT (name) DO UPDATE SET cursor = EXCLUDED.cursor RETURNING 1",
                &[
                    P::Text(held.name.as_str()),
                    P::Text(&held.holder),
                    int(held.fencing_token)?,
                    P::Text(cursor),
                ],
            )
            .await?;
        if rows.is_empty() {
            let error = TxError::Fenced;
            if let Some(tx) = &self.tx {
                tx.fail(&error);
            }
            return Err(error);
        }
        Ok(())
    }
}

/// The table of a queue. The names are constants, never input.
const fn table(queue: Queue) -> &'static str {
    match queue {
        Queue::NotificationOutbox => "notification_outbox",
    }
}

/// How a claim ends.
enum Settle {
    Done,
    Retry(Timestamp),
    Parked,
}

impl Handle {
    async fn settle(&self, claim: &Claim, how: Settle) -> Result<bool, TxError> {
        let table = table(claim.queue);
        let set = match how {
            Settle::Retry(_) => "next_attempt_at = $3",
            Settle::Done => "status = 'done'",
            Settle::Parked => "status = 'parked'",
        };
        let sql = format!(
            "UPDATE {table} SET lease_until = NULL, {set} \
             WHERE id = $1 AND attempts = $2 AND status = 'pending' RETURNING 1"
        );
        let mut params = vec![P::Uuid(claim.id), P::Int4(claim.attempt)];
        if let Settle::Retry(at) = how {
            params.push(P::Ts(at));
        }
        Ok(self.write(&sql, &params).await? == 1)
    }
}

#[async_trait]
impl Claims for Handle {
    async fn enqueue(&self, queue: Queue, job: &NewJob) -> Result<(), TxError> {
        let table = table(queue);
        let sql = format!(
            "INSERT INTO {table} (id, dedup_key, payload, attempts, next_attempt_at, status, lease_until) \
             VALUES ($1, $2, $3, 0, $4, 'pending', NULL) RETURNING 1"
        );
        self.write(
            &sql,
            &[
                P::Uuid(job.id),
                P::Text(&job.dedup_key),
                P::Text(&job.payload),
                P::Ts(job.run_at),
            ],
        )
        .await?;
        Ok(())
    }

    async fn claim(
        &self,
        queue: Queue,
        limit: u32,
        lease_for: Duration,
    ) -> Result<Vec<Claim>, TxError> {
        let table = table(queue);
        let sql = format!(
            "WITH due AS (SELECT id FROM {table} WHERE status = 'pending' \
             AND next_attempt_at <= clock_timestamp() \
             AND (lease_until IS NULL OR lease_until < clock_timestamp()) \
             ORDER BY next_attempt_at, id LIMIT $1 FOR UPDATE SKIP LOCKED) \
             UPDATE {table} q SET attempts = q.attempts + 1, \
             lease_until = clock_timestamp() + $2 * interval '1 microsecond' \
             FROM due WHERE q.id = due.id \
             RETURNING q.next_attempt_at, q.id, q.payload, q.attempts, q.lease_until"
        );
        let rows = self
            .query(
                Which::Main,
                true,
                &sql,
                &[P::Int(i64::from(limit)), micros(lease_for)],
            )
            .await?;
        let mut claims = every(&rows, |c| {
            Ok((
                c.ts()?,
                Claim {
                    queue,
                    id: c.uuid()?,
                    payload: c.text()?,
                    attempt: c.u32()?,
                    lease_until: c.ts()?,
                },
            ))
        })?;
        claims.sort_by_key(|(due, claim)| (*due, claim.id));
        Ok(claims.into_iter().map(|(_, claim)| claim).collect())
    }

    async fn complete(&self, claim: &Claim) -> Result<bool, TxError> {
        self.settle(claim, Settle::Done).await
    }

    async fn fail(&self, claim: &Claim, retry_at: Timestamp) -> Result<bool, TxError> {
        self.settle(claim, Settle::Retry(retry_at)).await
    }

    async fn park(&self, claim: &Claim) -> Result<bool, TxError> {
        self.settle(claim, Settle::Parked).await
    }
}

#[async_trait]
impl Migrations for Handle {
    async fn applied(&self) -> Result<Vec<LedgerEntry>, TxError> {
        let rows = self
            .read(
                "SELECT id, kind, applied_at, checksum FROM schema_migrations ORDER BY id",
                &[],
            )
            .await?;
        every(&rows, |c| {
            Ok(LedgerEntry {
                id: c.text()?,
                kind: c.parsed(MigrationKind::parse)?,
                applied_at: c.ts()?,
                checksum: c.text()?,
            })
        })
    }

    async fn apply(&self, entry: &LedgerEntry) -> Result<bool, TxError> {
        let inserted = self
            .write(
                "INSERT INTO schema_migrations (id, kind, applied_at, checksum) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (id) DO NOTHING RETURNING 1",
                &[
                    P::Text(&entry.id),
                    P::Text(entry.kind.as_str()),
                    P::Ts(entry.applied_at),
                    P::Text(&entry.checksum),
                ],
            )
            .await?;
        Ok(inserted == 1)
    }
}
