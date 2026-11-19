//! Crucible's configuration (contract 12.1): a TOML file, then `CRUCIBLE_*` environment variables, then
//! command-line flags, each layer overriding the one before.
//!
//! - Every setting has one dotted key (`server.listen`), which is its place in the TOML file, and one
//!   environment variable (`CRUCIBLE_SERVER_LISTEN`). [`SETTINGS`] is the whole list.
//! - An unknown key in the file is an error that names the nearest known key. An unknown `CRUCIBLE_*`
//!   variable is a warning, because test harnesses and other tools share the prefix.
//! - A secret is given as a value or as a path in its `_file` twin. Given both in one layer, the file is used
//!   and a warning says so. [`Secret`] never prints its value.
//! - Every problem is reported at once ([`ConfigErrors`]), and each message says how to fix it.
//!
//! [`resolve`] is pure; [`load`] feeds it the process environment and the file system, and is the only code
//! in the workspace that reads the process environment.

mod resolve;
mod settings;
mod types;

use std::path::Path;

pub use resolve::{Layers, Loaded, resolve};
pub use settings::{SETTINGS, Setting};
pub use types::{
    Auth, ClickHouse, Config, ConfigErrors, Data, Mcp, PgOutageMode, Postgres, Secret, Server,
    Source, WalBackend,
};

/// The environment variable that names the configuration file, like the `--config` flag.
pub const CONFIG_FILE_VARIABLE: &str = "CRUCIBLE_CONFIG";

/// Loads the configuration of this process: the file named by `config_flag` or by `CRUCIBLE_CONFIG`, the
/// `CRUCIBLE_*` variables of the process environment, and `flags` as `(dotted key, value)` pairs.
#[allow(clippy::disallowed_methods)] // This crate is where the environment and the configuration file are read.
pub fn load(config_flag: Option<&Path>, flags: &[(&str, String)]) -> Result<Loaded, ConfigErrors> {
    let mut errors = Vec::new();
    let mut env = Vec::new();
    for (name, value) in std::env::vars_os() {
        let Some(name) = name.to_str().filter(|name| name.starts_with("CRUCIBLE_")) else {
            continue;
        };
        match value.into_string() {
            Ok(value) => env.push((name.to_owned(), value)),
            Err(_) => errors.push(format!(
                "the environment variable {name} is not valid UTF-8"
            )),
        }
    }
    let from_env = env
        .iter()
        .find(|(name, value)| name == CONFIG_FILE_VARIABLE && !value.is_empty())
        .map(|(_, value)| Path::new(value).to_path_buf());
    let path = config_flag.map(Path::to_path_buf).or(from_env);
    let file = match &path {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(text) => Some((path.display().to_string(), text)),
            Err(error) => {
                errors.push(format!(
                    "cannot read the configuration file {}: {error} (it is named by --config or {CONFIG_FILE_VARIABLE})",
                    path.display()
                ));
                None
            }
        },
        None => None,
    };
    let layers = Layers {
        file: file
            .as_ref()
            .map(|(name, text)| (name.as_str(), text.as_str())),
        env: &env,
        flags,
    };
    match resolve(&layers, &|path| std::fs::read_to_string(path)) {
        Ok(loaded) if errors.is_empty() => Ok(loaded),
        Ok(_) => Err(ConfigErrors(errors)),
        Err(ConfigErrors(more)) => {
            errors.extend(more);
            Err(ConfigErrors(errors))
        }
    }
}
