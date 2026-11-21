//! The SQL workspace across one start with a wrong master key (contracts 6.6 and 7.5), on the real `crucible serve`
//! binary in front of a real ClickHouse and a real Postgres, each start with `--migrate` as Compose runs it:
//!
//! 1. With the right key the workspace runs a query.
//! 2. A start with another key: the service identities refuse it, so the server is not ready and says why. It
//!    changes nothing the right key needs: the migrations ledger leaves the password digests out of its checksums,
//!    so the start applies no migration step (its log says so), and access step A2 is not applied again with the
//!    wrong key's passwords for the probe identities, which still sign in with the right key's.
//! 3. With the right key again the workspace runs queries, and no operator had to repair anything.
//!
//! The test runs only when `CRUCIBLE_SQLISO_CLICKHOUSE_URL` is set and passes as skipped otherwise. It takes the
//! variables of `sqliso_api.rs` (layer C), which `.github/workflows/sqliso.yml` runs before it on the same servers;
//! the right key here is the key layer C migrated them with.

#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::disallowed_macros,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

mod common;

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{ANY_PORT, exit_error, serving};
use crucible_schema::{PROBE_PROJECTS, workspace_user};
use serde_json::{Value, json};

const MASTER_KEY: &str = "5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e";
const WRONG_KEY: &str = "0bad0bad0bad0bad0bad0bad0bad0bad0bad0bad0bad0bad0bad0bad0bad0bad";
const OWNER_PASSWORD: &str = "correct horse battery staple";
const QUERY: &str = "SELECT count() FROM crucible_sql.spans WHERE start_time >= {from:DateTime64(6)} AND \
                     start_time < {to:DateTime64(6)}";

/// One HTTP/1.1 exchange on a fresh connection: the status and the body.
fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> std::io::Result<(u16, Vec<u8>)> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: sqlws\r\nConnection: close\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let end = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| std::io::Error::other("no response head"))?;
    let head = String::from_utf8_lossy(&response[..end]).into_owned();
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|status| status.parse().ok())
        .ok_or_else(|| std::io::Error::other("no status"))?;
    Ok((status, response[end + 4..].to_vec()))
}

/// A running `crucible serve`; dropping it kills the process, and a failing test prints its log.
struct Server {
    child: Child,
    api: SocketAddr,
    log: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if std::thread::panicking() {
            let log = std::fs::read_to_string(&self.log).unwrap_or_default();
            let tail: Vec<&str> = log.lines().rev().take(40).collect();
            eprintln!("--- {} (last lines) ---", self.log.display());
            for line in tail.into_iter().rev() {
                eprintln!("{line}");
            }
        }
    }
}

impl Server {
    /// The status and body of `GET /readyz`, or `None` while nothing answers.
    fn readyz(&self) -> Option<(u16, String)> {
        let (status, body) = http(self.api, "GET", "/readyz", &[], b"").ok()?;
        Some((status, String::from_utf8_lossy(&body).into_owned()))
    }

    /// Panics when the server's log shows a migration step applied, or one recorded with other content.
    fn applied_no_step(&self, when: &str) {
        let log = std::fs::read_to_string(&self.log).unwrap();
        let applied: Vec<&str> = log
            .lines()
            .filter(|line| {
                line.contains("migration step applied") || line.contains("with other content")
            })
            .collect();
        assert!(
            applied.is_empty(),
            "{when}: the server applied migration steps:\n{}",
            applied.join("\n")
        );
    }

    /// Waits until `GET /readyz` answers in a way `done` accepts; panics after `within`, and with the server's
    /// error when it exits.
    fn wait_for(&mut self, within: Duration, what: &str, done: impl Fn(u16, &str) -> bool) {
        let deadline = Instant::now() + within;
        let mut last = None;
        loop {
            if let Some((status, body)) = self.readyz() {
                if done(status, &body) {
                    return;
                }
                last = Some((status, body));
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                let log = std::fs::read_to_string(&self.log).unwrap_or_default();
                panic!(
                    "crucible serve exited ({status}) while waiting until {what}: {}",
                    exit_error(&log)
                );
            }
            assert!(
                Instant::now() < deadline,
                "not {what} within {within:?}; /readyz answered {last:?}"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

/// What every start shares: the binary, the servers and the files of the secrets.
struct Rig {
    binary: PathBuf,
    dir: PathBuf,
    clickhouse_url: String,
    clickhouse_user: String,
    postgres_url: String,
    starts: u32,
}

impl Rig {
    /// Starts `crucible serve --migrate` with the master key in `key_file`, and waits until it listens.
    fn start(&mut self, key_file: &str) -> Server {
        self.starts += 1;
        let log = self.dir.join(format!("serve-{}.log", self.starts));
        let child = Command::new(&self.binary)
            .arg("serve")
            .args(["--listen", ANY_PORT])
            .args(["--ingest-listen", ANY_PORT])
            .args(["--grpc-listen", ANY_PORT])
            .args(["--metrics-listen", ANY_PORT])
            .arg("--data-dir")
            .arg(self.dir.join("data"))
            .args(["--clickhouse-url", &self.clickhouse_url])
            .args(["--clickhouse-user", &self.clickhouse_user])
            .arg("--clickhouse-password-file")
            .arg(self.dir.join("clickhouse-password"))
            .arg("--master-key-file")
            .arg(self.dir.join(key_file))
            .arg("--migrate")
            .env_remove("CRUCIBLE_CLICKHOUSE_URL")
            .env("CRUCIBLE_POSTGRES_URL", &self.postgres_url)
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap();
        let mut server = Server {
            child,
            api: SocketAddr::from(([127, 0, 0, 1], 0)),
            log,
        };
        server.api =
            serving(&mut server.child, &server.log, Duration::from_secs(60)).addr("listen");
        server
    }

    /// `crucible admin <args>` with the right key: whether it succeeded, and what it printed.
    fn admin(&self, args: &[&str]) -> (bool, String, String) {
        let output = Command::new(&self.binary)
            .arg("admin")
            .args(args)
            .arg("--master-key-file")
            .arg(self.dir.join("master-key"))
            .env_remove("CRUCIBLE_CLICKHOUSE_URL")
            .env("CRUCIBLE_POSTGRES_URL", &self.postgres_url)
            .output()
            .unwrap();
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }
}

/// A key's calls to the SQL workspace of its project.
struct Workspace {
    project: String,
    bearer: String,
}

impl Workspace {
    fn call(
        &self,
        server: &Server,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> (u16, Value) {
        let path = format!("/api/v1/projects/{}/sql/{path}", self.project);
        let bytes = body
            .map(|body| serde_json::to_vec(body).unwrap())
            .unwrap_or_default();
        let mut headers = vec![("Authorization", self.bearer.as_str())];
        if body.is_some() {
            headers.push(("Content-Type", "application/json"));
        }
        let (status, body) = http(server.api, method, &path, &headers, &bytes).unwrap();
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    /// Waits until the workspace is enabled (its self-probe passed) and a query runs; panics after `within` with
    /// the workspace's last status.
    fn runs(&self, server: &Server, within: Duration, when: &str) {
        let deadline = Instant::now() + within;
        loop {
            let (status, schema) = self.call(server, "GET", "schema", None);
            if status == 200 && schema["status"]["enabled"] == true {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{when}: the SQL workspace was not enabled within {within:?}: {status} {:#}",
                schema["status"]
            );
            std::thread::sleep(Duration::from_millis(500));
        }
        let run =
            json!({ "sql": QUERY, "from": "2020-01-01T00:00:00Z", "to": "2100-01-01T00:00:00Z" });
        let (status, grid) = self.call(server, "POST", "run", Some(&run));
        assert_eq!(status, 200, "{when}: {grid:#}");
        assert_eq!(grid["columns"][0]["name"], "count()", "{when}: {grid:#}");
    }
}

fn write_secret(dir: &Path, name: &str, value: &str) {
    std::fs::write(dir.join(name), format!("{value}\n")).unwrap();
}

/// Whether the ClickHouse user `user` signs in with the password that `master_key` derives for it: HKDF-SHA256 of
/// the key with the user name and generation 0 (contract 7.1).
fn signs_in(clickhouse: SocketAddr, master_key: &str, user: &str) -> bool {
    use base64::Engine as _;

    let key: Vec<u8> = (0..master_key.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&master_key[at..at + 2], 16).unwrap())
        .collect();
    let mut password = [0u8; 32];
    hkdf::Hkdf::<sha2::Sha256>::new(None, &key)
        .expand(
            format!("crucible/clickhouse-identity/v1/{user}/0").as_bytes(),
            &mut password,
        )
        .unwrap();
    let password: String = password.iter().map(|byte| format!("{byte:02x}")).collect();
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
    let (status, _) = http(
        clickhouse,
        "GET",
        "/?query=SELECT%201",
        &[("Authorization", &format!("Basic {basic}"))],
        b"",
    )
    .unwrap();
    status == 200
}

#[test]
fn a_start_with_a_wrong_master_key_leaves_the_sql_workspace_working() {
    let Ok(clickhouse_url) = std::env::var("CRUCIBLE_SQLISO_CLICKHOUSE_URL") else {
        eprintln!("skipped: CRUCIBLE_SQLISO_CLICKHOUSE_URL is not set");
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().to_path_buf();
    write_secret(&dir, "master-key", MASTER_KEY);
    write_secret(&dir, "wrong-key", WRONG_KEY);
    write_secret(
        &dir,
        "clickhouse-password",
        &std::env::var("CRUCIBLE_SQLISO_CLICKHOUSE_PASSWORD")
            .expect("CRUCIBLE_SQLISO_CLICKHOUSE_PASSWORD is set"),
    );
    write_secret(&dir, "owner-password", OWNER_PASSWORD);
    let mut rig = Rig {
        binary: std::env::var_os("CRUCIBLE_SQLISO_BIN")
            .map(PathBuf::from)
            .expect("CRUCIBLE_SQLISO_BIN is set"),
        dir: dir.clone(),
        clickhouse_url,
        clickhouse_user: std::env::var("CRUCIBLE_SQLISO_CLICKHOUSE_USER")
            .unwrap_or_else(|_| "crucible_admin".to_owned()),
        postgres_url: std::env::var("CRUCIBLE_SQLISO_POSTGRES_URL")
            .expect("CRUCIBLE_SQLISO_POSTGRES_URL is set"),
        starts: 0,
    };

    // 1. The right key: the migrations, the owner and the first project, a key with the `read` and `sql` scopes,
    //    and a query through the workspace.
    let mut server = rig.start("master-key");
    server.wait_for(Duration::from_secs(120), "ready", |status, _| status == 200);
    let password_file = dir.join("owner-password");
    // On a database that layer C used, the owner and the project `default` exist already.
    let (created, _, stderr) = rig.admin(&[
        "init",
        "--org-name",
        "sqlws",
        "--email",
        "owner@sqlws.test",
        "--name",
        "Owner",
        "--password-file",
        password_file.to_str().unwrap(),
    ]);
    if !created {
        eprintln!("crucible admin init did not create an owner: {stderr}");
    }
    let (made, printed, stderr) =
        rig.admin(&["create-key", "--project", "default", "--scopes", "read,sql"]);
    assert!(made, "crucible admin create-key: {stderr}");
    let field = |name: &str| {
        printed
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name} = \"")))
            .map(|value| value.trim_end_matches('"').to_owned())
            .unwrap_or_else(|| panic!("no {name} in {printed}"))
    };
    let workspace = Workspace {
        project: field("project_id"),
        bearer: format!("Bearer {}", field("secret_key")),
    };
    workspace.runs(&server, Duration::from_secs(90), "with the right key");
    drop(server);

    // 2. Another master key: the service identities refuse their derived passwords, so detection fails after the
    //    migrations ran, and the server says which setting to check. It applied no step, and the probe identities
    //    still take the right key's passwords.
    let mut server = rig.start("wrong-key");
    server.wait_for(
        Duration::from_secs(90),
        "refusing the wrong key",
        |status, body| status == 503 && body.contains("different master key"),
    );
    server.applied_no_step("with the wrong key");
    drop(server);
    let clickhouse = rig
        .clickhouse_url
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_socket_addrs()
        .unwrap()
        .min_by_key(|addr| !addr.is_ipv4())
        .unwrap();
    for project in PROBE_PROJECTS {
        let user = workspace_user(project);
        assert!(
            signs_in(clickhouse, MASTER_KEY, &user),
            "after the start with the wrong key, {user} refuses the right key's password"
        );
    }

    // 3. The right key again: ready, and the workspace runs queries without a repair by an operator.
    let mut server = rig.start("master-key");
    server.wait_for(Duration::from_secs(120), "ready", |status, _| status == 200);
    server.applied_no_step("with the right key after a start with a wrong one");
    workspace.runs(
        &server,
        Duration::from_secs(90),
        "with the right key after a start with a wrong one",
    );
    drop(server);
}
