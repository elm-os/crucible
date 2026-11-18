//! The project keys that ingest accepts in milestone 1, from a TOML file passed with `--keys-file`:
//!
//! ```toml
//! [[project_key]]
//! project_id = "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b"
//! public_key = "cpk_..."
//! secret = "csk_..."
//! ```
//!
//! The secret authenticates alone as `Authorization: Bearer`, or with the public key as
//! `Authorization: Basic` (contract 12.2). Each key gets an id from `Env::new_id` at startup; the secrets are
//! kept only as salted hashes. Key rows in the control plane replace this file in a later milestone.

use std::path::Path;

use anyhow::Context as _;
use crucible_auth::{PublicKey, SecretKey, StaticKey, StaticKeys};
use crucible_env::Env;
use crucible_records::{ApiKeyId, ProjectId};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct KeysFile {
    #[serde(default)]
    project_key: Vec<Entry>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    project_id: String,
    public_key: String,
    secret: String,
}

/// Parses a keys file. Error messages never quote the file, so they cannot echo a key.
pub(crate) fn parse(text: &str, env: &Env) -> anyhow::Result<StaticKeys> {
    let file: KeysFile = toml::from_str(text).map_err(|e| {
        let message = e.message();
        // Keys files written before Basic auth have no public keys.
        let hint = if message.contains("missing field `public_key`") {
            "; every [[project_key]] needs the public key of its pair as public_key = \"cpk_...\" \
             (docs/development.md shows how to make a pair)"
        } else {
            ""
        };
        anyhow::anyhow!("the keys file is not valid: {message}{hint}")
    })?;
    let keys = file
        .project_key
        .into_iter()
        .enumerate()
        .map(|(index, entry)| {
            let n = index + 1;
            Ok(StaticKey {
                id: ApiKeyId::from_uuid(env.new_id()),
                project_id: ProjectId::parse(&entry.project_id)
                    .with_context(|| format!("project_key {n}: project_id"))?,
                public_key: PublicKey::parse(&entry.public_key)
                    .with_context(|| format!("project_key {n}: public_key"))?,
                secret: SecretKey::parse(&entry.secret)
                    .with_context(|| format!("project_key {n}: secret"))?,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(StaticKeys::new(
        env.rng.fork("crucible-auth/static-keys").as_ref(),
        keys,
    )?)
}

/// Reads and parses the keys file at `path`.
#[allow(clippy::disallowed_methods)] // The binary reads its own configuration file.
pub(crate) fn load(path: &Path, env: &Env) -> anyhow::Result<StaticKeys> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse(&text, env)
}
