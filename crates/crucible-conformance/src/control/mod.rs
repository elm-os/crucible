//! The control-store conformance scripts (contract 2.7): repository, transaction, key-feed, lease, claim, job
//! and identity scripts that run unchanged against any `ControlStore`.
//!
//! Each script writes a transcript of what it observed. The differential runs every script against the
//! simulation's `PgModel` and against real Postgres and compares the transcripts line by line; a script also
//! fails on its own when an observation contradicts the contract. Transcripts hold no raw database time and no
//! raw feed sequence number, because those differ between two stores that behave the same.

mod claims;
mod feed;
mod identity;
mod jobs;
mod leases;
mod lifecycle;
mod pools;
mod repos;
mod transactions;

use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use crucible_control::{
    ApiKey, ControlStore, Email, JsonDoc, KeyScope, Project, TxError, User, UserStatus,
};
use crucible_env::Clock;
use crucible_records::{ApiKeyId, ProjectId, Timestamp, UserId, Uuid};
use futures::future::BoxFuture;

/// What a script works with.
#[derive(Debug)]
pub struct Ctl {
    /// The store under test, empty at the start of each script.
    pub store: Arc<dyn ControlStore>,
    /// The clock the script waits on.
    pub clock: Arc<dyn Clock>,
}

impl Ctl {
    async fn sleep(&self, d: Duration) {
        self.clock.sleep(d).await;
    }
}

/// What a script observed, one line per observation.
#[derive(Debug, Default)]
pub struct Transcript(Vec<String>);

impl Transcript {
    fn note(&mut self, label: &str, value: impl Debug) {
        self.0.push(format!("{label}: {value:?}"));
    }

    /// Notes a result, with errors reduced to what both stores must agree on.
    fn outcome<T: Debug>(&mut self, label: &str, result: Result<T, TxError>) {
        match result {
            Ok(value) => self.note(label, value),
            Err(error) => self.0.push(format!("{label}: error {}", kind(&error))),
        }
    }

    /// The observations.
    pub fn lines(&self) -> &[String] {
        &self.0
    }

    /// The observations as one text.
    pub fn text(&self) -> String {
        self.0.join("\n")
    }
}

/// An error as both stores must report it: the variant and the constraint, never the free text.
fn kind(error: &TxError) -> String {
    match error {
        TxError::Unavailable(_) => "Unavailable".to_owned(),
        TxError::Invalid(_) => "Invalid".to_owned(),
        other => format!("{other:?}"),
    }
}

/// A conformance script.
pub type Script = for<'a> fn(&'a Ctl, &'a mut Transcript) -> BoxFuture<'a, Result<()>>;

/// Every script, by name.
pub const SCRIPTS: &[(&str, Script)] = &[
    ("repositories", |c, t| Box::pin(repos::run(c, t))),
    ("transactions", |c, t| Box::pin(transactions::run(c, t))),
    ("key_feed", |c, t| Box::pin(feed::run(c, t))),
    ("leases", |c, t| Box::pin(leases::run(c, t))),
    ("claims", |c, t| Box::pin(claims::run(c, t))),
    ("identity", |c, t| Box::pin(identity::run(c, t))),
    ("pools", |c, t| Box::pin(pools::run(c, t))),
    ("jobs", |c, t| Box::pin(jobs::run(c, t))),
    ("lifecycle", |c, t| Box::pin(lifecycle::run(c, t))),
];

/// The main pool size the scripts are written for; both stores under test are built with it.
pub const MAIN_POOL: usize = 2;
/// How long both stores under test wait for a pooled connection.
pub const ACQUIRE_TIMEOUT: Duration = Duration::from_millis(500);

/// Runs one script against an empty store.
pub async fn run_script(ctl: &Ctl, script: Script) -> Result<Transcript> {
    let mut transcript = Transcript::default();
    script(ctl, &mut transcript).await?;
    Ok(transcript)
}

/// The first line on which two transcripts differ, as a message; `None` when they are equal.
pub fn first_difference(model: &Transcript, real: &Transcript) -> Option<String> {
    let (model, real) = (model.lines(), real.lines());
    for at in 0..model.len().max(real.len()) {
        let (m, r) = (model.get(at), real.get(at));
        if m != r {
            let show = |line: Option<&String>| line.map_or("(nothing)", String::as_str).to_owned();
            return Some(format!(
                "line {}: the model observed `{}`, postgres observed `{}`",
                at + 1,
                show(m),
                show(r)
            ));
        }
    }
    None
}

/// A deterministic id: the scripts mint no id from a clock or a random stream.
fn id(n: u64) -> Uuid {
    Uuid::from_v7_parts(1_800_000_000_000 + n, u128::from(n))
}

/// A fixed instant for the timestamps the scripts store: 2027-01-15T08:00:00Z plus `secs`.
fn at(secs: i64) -> Timestamp {
    Timestamp::from_micros(1_800_000_000_000_000 + secs * 1_000_000)
}

fn user(n: u64) -> Result<User> {
    Ok(User {
        id: UserId::from_uuid(id(n)),
        email: Email::new(&format!("user{n}@example.com")).context("the fixture email")?,
        name: format!("User {n}"),
        status: UserStatus::Active,
        email_verified_at: None,
    })
}

fn project(n: u64) -> Project {
    Project {
        id: ProjectId::from_uuid(id(n)),
        name: format!("Project {n}"),
        slug: format!("project-{n}"),
        settings: JsonDoc::empty(),
        retention_days: Some(30),
        payload_retention_days: None,
        mcp_content_retention_days: None,
        config_version: 1,
        version: 1,
        deleted_at: None,
    }
}

fn api_key(n: u64, project: ProjectId) -> ApiKey {
    let byte = u8::try_from(n % 251).unwrap_or(0);
    ApiKey {
        id: ApiKeyId::from_uuid(id(n)),
        project_id: project,
        public_key: format!("cpk_{n:043}"),
        secret_salt: [byte; 16],
        secret_hash: [byte.wrapping_add(1); 32],
        lookup_tag: [byte.wrapping_add(2); 8],
        scopes: vec![KeyScope::Ingest, KeyScope::Read, KeyScope::Write],
        import: false,
        note: format!("key {n}"),
        expires_at: None,
        last_used_at: None,
        revoked_at: None,
        version: 1,
    }
}
