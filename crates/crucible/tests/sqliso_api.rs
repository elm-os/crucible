//! `sqliso` layer C (contract 7.5): the SQL workspace attacked and exercised through the REST API of the real
//! `crucible serve` binary, in front of a real ClickHouse and a real Postgres.
//!
//! Two projects, A and B, each with an API key that holds the `read` and `sql` scopes, made with
//! `crucible admin create-key`; the owner, made with `crucible admin init`, signs in for what keys never do
//! (saved-query writes). B's rows hold a random canary. Every response that a caller receives on project A,
//! grids, exports and problem documents alike, passes one oracle: B's canary is not in it. On top of that the
//! test checks what the API adds to ClickHouse's isolation: the validation messages (each refusal says how to
//! fix the query), the mapping of server limits to problem codes, the concurrency cap, cancellation, the audit
//! rows, saved queries with declared parameters, and the CSV formula guard.
//!
//! The test runs only when `CRUCIBLE_SQLISO_CLICKHOUSE_URL` is set and passes as skipped otherwise:
//!
//! | Variable | Meaning |
//! |---|---|
//! | `CRUCIBLE_SQLISO_CLICKHOUSE_URL` | The HTTP endpoint of a ClickHouse 26.3 or newer whose admin may manage access |
//! | `CRUCIBLE_SQLISO_CLICKHOUSE_USER` | The admin user; `crucible_admin` when unset |
//! | `CRUCIBLE_SQLISO_CLICKHOUSE_PASSWORD` | Its password |
//! | `CRUCIBLE_SQLISO_POSTGRES_URL` | `postgres://user:password@host:5432/database`, an empty database |
//! | `CRUCIBLE_SQLISO_BIN` | The server binary, built on its own with `cargo build -p crucible` |
//!
//! `.github/workflows/sqliso.yml` runs it on ClickHouse 26.3, 26.8 and latest.

#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::disallowed_macros,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crucible_control::{JsonDoc, Project, Repos as _};
use crucible_control_pg::{PgConfig, PgControlStore};
use crucible_env::TlsRoots;
use crucible_env_prod::ProdConfig;
use crucible_records::ProjectId;
use crucible_schema::ServiceIdentity;
use crucible_store_clickhouse::render::Statement;
use crucible_store_clickhouse::{ClickHouseStore, Credentials, StoreConfig};
use serde_json::{Value, json};

const OWNER_EMAIL: &str = "owner@sqliso.test";
const OWNER_PASSWORD: &str = "correct horse battery staple";
const MASTER_KEY: &str = "5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e";
const WINDOW: &str = "start_time >= {from:DateTime64(6)} AND start_time < {to:DateTime64(6)}";

fn free_addr() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

/// One HTTP/1.1 exchange on a fresh connection: the status, the head and the body.
fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> std::io::Result<(u16, String, Vec<u8>)> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: sqliso\r\nConnection: close\r\n");
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
    let chunked = head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked");
    let raw = &response[end + 4..];
    Ok((
        status,
        head,
        if chunked { dechunk(raw) } else { raw.to_vec() },
    ))
}

/// The body of a chunked response.
fn dechunk(mut raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(line_end) = raw.windows(2).position(|w| w == b"\r\n") {
        let size = std::str::from_utf8(&raw[..line_end])
            .ok()
            .and_then(|hex| usize::from_str_radix(hex.trim(), 16).ok())
            .unwrap_or(0);
        if size == 0 {
            break;
        }
        let start = line_end + 2;
        out.extend_from_slice(&raw[start..start + size]);
        raw = &raw[start + size + 2..];
    }
    out
}

struct Server {
    child: Child,
    log: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if std::thread::panicking() {
            let log = std::fs::read_to_string(&self.log).unwrap_or_default();
            let tail: Vec<&str> = log.lines().rev().take(60).collect();
            eprintln!("--- {} (last lines) ---", self.log.display());
            for line in tail.into_iter().rev() {
                eprintln!("{line}");
            }
        }
    }
}

/// A caller on one project: an API key or the owner's session. Every response passes the canary oracle.
#[derive(Clone)]
struct Client {
    api: SocketAddr,
    project: String,
    /// The headers that authenticate the caller.
    auth: Vec<(&'static str, String)>,
    /// What must never appear in a response to this client.
    forbidden: String,
}

impl Client {
    fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        extra: &[(&str, &str)],
    ) -> (u16, String, Vec<u8>) {
        let path = format!("/api/v1/projects/{}/sql/{path}", self.project);
        let bytes = body
            .map(|body| serde_json::to_vec(body).unwrap())
            .unwrap_or_default();
        let mut headers: Vec<(&str, &str)> = self
            .auth
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        if body.is_some() {
            headers.push(("Content-Type", "application/json"));
        }
        headers.extend_from_slice(extra);
        let (status, head, body) = http(self.api, method, &path, &headers, &bytes).unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(
            !text.contains(&self.forbidden) && !head.contains(&self.forbidden),
            "LEAK: another project's canary reached this caller through {method} {path}: {text}"
        );
        (status, head, body)
    }

    fn json(&self, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
        let (status, _, bytes) = self.send(method, path, body, &[]);
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn run(&self, sql: &str) -> (u16, Value) {
        self.json("POST", "run", Some(&run_body(sql)))
    }

    /// Runs `sql` and expects a problem with this status and code; returns the document.
    fn refused(&self, sql: &str, status: u16, code: &str) -> Value {
        let (got, body) = self.run(sql);
        assert_eq!(
            (got, body["code"].as_str()),
            (status, Some(code)),
            "{sql}: {body:#}"
        );
        assert!(
            body["detail"]
                .as_str()
                .is_some_and(|detail| detail.len() > 15),
            "a refusal says how to fix the query: {body:#}"
        );
        body
    }
}

fn run_body(sql: &str) -> Value {
    json!({ "sql": sql, "from": "2020-01-01T00:00:00Z", "to": "2100-01-01T00:00:00Z" })
}

fn project(id: ProjectId, slug: &str) -> Project {
    Project {
        id,
        name: slug.to_owned(),
        slug: slug.to_owned(),
        settings: JsonDoc::empty(),
        retention_days: None,
        payload_retention_days: None,
        mcp_content_retention_days: None,
        config_version: 1,
        version: 1,
        deleted_at: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn the_sql_workspace_holds_through_the_api() {
    let Ok(clickhouse_url) = std::env::var("CRUCIBLE_SQLISO_CLICKHOUSE_URL") else {
        eprintln!("skipped: CRUCIBLE_SQLISO_CLICKHOUSE_URL is not set");
        return;
    };
    let postgres_url =
        std::env::var("CRUCIBLE_SQLISO_POSTGRES_URL").expect("CRUCIBLE_SQLISO_POSTGRES_URL is set");
    let binary = std::env::var_os("CRUCIBLE_SQLISO_BIN")
        .map(PathBuf::from)
        .expect("CRUCIBLE_SQLISO_BIN is set");
    let admin = Credentials {
        user: std::env::var("CRUCIBLE_SQLISO_CLICKHOUSE_USER")
            .unwrap_or_else(|_| "crucible_admin".to_owned()),
        password: std::env::var("CRUCIBLE_SQLISO_CLICKHOUSE_PASSWORD")
            .expect("CRUCIBLE_SQLISO_CLICKHOUSE_PASSWORD is set"),
    };
    let prod = crucible_env_prod::build(tokio::runtime::Handle::current(), &ProdConfig::default())
        .unwrap();
    let env = &prod.env;
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().to_path_buf();

    // Fresh projects on every run, so servers that earlier runs wrote to are as good as empty ones.
    let (a, b) = (
        ProjectId::from_uuid(env.new_id()),
        ProjectId::from_uuid(env.new_id()),
    );
    let canary_a = format!("cnry_a_{}", env.new_id().to_string().replace('-', ""));
    let canary_b = format!("cnry_b_{}", env.new_id().to_string().replace('-', ""));
    std::fs::write(dir.join("master-key"), format!("{MASTER_KEY}\n")).unwrap();
    std::fs::write(dir.join("clickhouse-password"), &admin.password).unwrap();

    // 1. The server, with migrations: Postgres, the ClickHouse schema and access steps A1 and A2.
    let log = dir.join("serve.log");
    let (api, ingest, grpc, metrics) = (free_addr(), free_addr(), free_addr(), free_addr());
    let child = Command::new(&binary)
        .arg("serve")
        .args(["--listen", &api.to_string()])
        .args(["--ingest-listen", &ingest.to_string()])
        .args(["--grpc-listen", &grpc.to_string()])
        .args(["--metrics-listen", &metrics.to_string()])
        .arg("--data-dir")
        .arg(dir.join("data"))
        .args(["--clickhouse-url", &clickhouse_url])
        .args(["--clickhouse-user", &admin.user])
        .arg("--clickhouse-password-file")
        .arg(dir.join("clickhouse-password"))
        .arg("--master-key-file")
        .arg(dir.join("master-key"))
        .arg("--migrate")
        .env_remove("CRUCIBLE_CLICKHOUSE_URL")
        .env("CRUCIBLE_POSTGRES_URL", &postgres_url)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    let mut server = Server { child, log };
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if matches!(http(api, "GET", "/readyz", &[], b""), Ok((200, _, _))) {
            break;
        }
        assert!(
            server.child.try_wait().unwrap().is_none(),
            "crucible serve exited during startup"
        );
        assert!(
            Instant::now() < deadline,
            "crucible serve was not ready in time"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // 2. The two projects in the control plane, and their rows in ClickHouse, written around the API.
    let control = PgControlStore::new(env, &PgConfig::new(postgres_url.clone())).unwrap();
    control
        .projects()
        .insert(&project(a, &format!("a-{a}")))
        .await
        .unwrap();
    control
        .projects()
        .insert(&project(b, &format!("b-{b}")))
        .await
        .unwrap();
    let store = ClickHouseStore::new(
        env.clone(),
        StoreConfig {
            url: clickhouse_url.clone(),
            read_url: None,
            admin: admin.clone(),
            ingest: admin.clone(),
            read_ui: admin.clone(),
            read_agent: admin.clone(),
            maint: admin.clone(),
            flavour: None,
            tls_roots: TlsRoots::WebPki,
            connect_timeout: Duration::from_secs(10),
            idle_timeout: None,
        },
    );
    let admin_sql = async |sql: String| {
        let statement = Statement {
            sql: sql.clone(),
            params: Vec::new(),
            settings: Vec::new(),
        };
        store
            .raw_command(ServiceIdentity::Admin, &statement)
            .await
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
    };
    for (project, canary, formula) in [(a, &canary_a, "=1+1"), (b, &canary_b, "=2+2")] {
        admin_sql(format!(
            "INSERT INTO crucible.span_core (project_id, start_time, name, user_id) SELECT toUUID('{project}'), now(), \
             '{canary}', '{formula}'"
        ))
        .await;
    }
    // The owner and the keys, made with the binary's own admin commands. With a control plane the keys are
    // Postgres rows with scopes; the keys file is not read.
    std::fs::write(dir.join("owner-password"), format!("{OWNER_PASSWORD}\n")).unwrap();
    let admin_command = |args: &[&str]| {
        let output = Command::new(&binary)
            .arg("admin")
            .args(args)
            .arg("--master-key-file")
            .arg(dir.join("master-key"))
            .env_remove("CRUCIBLE_CLICKHOUSE_URL")
            .env("CRUCIBLE_POSTGRES_URL", &postgres_url)
            .output()
            .unwrap();
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    let password_file = dir.join("owner-password");
    // On a database an earlier run used, the owner exists already and signing in below is the check.
    let (created, _, stderr) = admin_command(&[
        "init",
        "--org-name",
        "sqliso",
        "--email",
        OWNER_EMAIL,
        "--name",
        "Owner",
        "--password-file",
        password_file.to_str().unwrap(),
    ]);
    if !created {
        eprintln!("crucible admin init did not create an owner: {stderr}");
    }
    let key = |project: ProjectId, scopes: &str| {
        let (ok, stdout, stderr) = admin_command(&[
            "create-key",
            "--project",
            &project.to_string(),
            "--scopes",
            scopes,
        ]);
        assert!(ok, "crucible admin create-key: {stderr}");
        let secret = stdout
            .lines()
            .find_map(|line| line.strip_prefix("secret_key = \""))
            .and_then(|rest| rest.strip_suffix('"'))
            .unwrap_or_else(|| panic!("no secret key in {stdout:?}"))
            .to_owned();
        vec![("Authorization", format!("Bearer {secret}"))]
    };
    let client = |project: ProjectId, auth: Vec<(&'static str, String)>, forbidden: &str| Client {
        api,
        project: project.to_string(),
        auth,
        forbidden: forbidden.to_owned(),
    };
    let alice = client(a, key(a, "read,sql"), &canary_b);
    let bob = client(b, key(b, "read,sql"), &canary_a);
    let reader = client(a, key(a, "read"), &canary_b);
    // The owner signs in: the session cookie, and the CSRF token and origin that its writes carry.
    let origin = "http://sqliso";
    let login = json!({ "email": OWNER_EMAIL, "password": OWNER_PASSWORD });
    let (status, head, body) = http(
        api,
        "POST",
        "/api/v1/auth/login",
        &[
            ("Content-Type", "application/json"),
            ("Origin", origin),
            ("X-Crucible-CSRF", "prelogin"),
        ],
        &serde_json::to_vec(&login).unwrap(),
    )
    .unwrap();
    assert_eq!(status, 200, "sign-in: {}", String::from_utf8_lossy(&body));
    let cookie = head
        .lines()
        .filter_map(|line| line.split_once(": "))
        .filter(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
        .find_map(|(_, value)| {
            value
                .split(';')
                .next()
                .filter(|c| c.starts_with("crucible_session="))
        })
        .expect("a session cookie")
        .to_owned();
    let session: Value = serde_json::from_slice(&body).unwrap();
    let owner_auth = vec![
        ("Cookie", cookie),
        (
            "X-Crucible-CSRF",
            session["csrf_token"].as_str().unwrap().to_owned(),
        ),
        ("Origin", origin.to_owned()),
    ];
    let owner = client(a, owner_auth.clone(), &canary_b);
    let owner_on_b = client(b, owner_auth, &canary_a);
    // The server learns of the new keys from the key feed.
    let deadline = Instant::now() + Duration::from_secs(60);
    while [&alice, &bob, &reader]
        .iter()
        .any(|client| client.json("GET", "schema", None).0 == 401)
    {
        assert!(
            Instant::now() < deadline,
            "the server never accepted the new keys"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    // A key without the `sql` scope reads the schema and runs nothing; no key saves a query.
    let (status, body) = reader.run(&format!(
        "SELECT name FROM crucible_sql.spans WHERE {WINDOW}"
    ));
    assert_eq!(
        (status, body["code"].as_str()),
        (403, Some("forbidden")),
        "{body:#}"
    );
    assert!(
        body["detail"].as_str().unwrap().contains("`run_sql`"),
        "{body:#}"
    );

    // 3. The workspace is enabled once the runtime self-probe has passed.
    let deadline = Instant::now() + Duration::from_secs(90);
    let schema = loop {
        let (status, schema) = alice.json("GET", "schema", None);
        assert_eq!(status, 200, "{schema:#}");
        if schema["status"]["enabled"] == true {
            break schema;
        }
        assert!(
            Instant::now() < deadline,
            "the workspace did not become available: {schema:#}"
        );
        std::thread::sleep(Duration::from_millis(500));
    };
    let relations = schema["relations"].as_array().unwrap();
    assert!(relations.iter().any(|r| r["name"] == "crucible_sql.spans"));
    assert_eq!(schema["limits"]["max_rows"], 10_000);

    // 4. A run returns the caller's rows and only those; the first use provisioned the identity.
    let spans = format!(
        "SELECT name, toString(project_id) AS project FROM crucible_sql.spans WHERE {WINDOW}"
    );
    let (status, grid) = alice.run(&spans);
    assert_eq!(status, 200, "{grid:#}");
    assert_eq!(grid["rows"], json!([[canary_a, a.to_string()]]));
    assert_eq!(
        grid["columns"][0],
        json!({ "name": "name", "type": "String" })
    );
    assert_eq!(grid["truncated"], false);
    let (status, grid) = bob.run(&spans);
    assert_eq!(status, 200, "{grid:#}");
    assert_eq!(grid["rows"], json!([[canary_b, b.to_string()]]));

    // 5. Attacks through the API. The oracle of `Client::send` judges every answer; these also must not show
    //    more than A's own row.
    let b_id = b.to_string();
    for sql in [
        format!(
            "SELECT name FROM crucible_sql.spans WHERE {WINDOW} AND project_id = toUUID('{b_id}')"
        ),
        format!(
            "SELECT name FROM crucible.span_core WHERE {WINDOW} AND project_id != toUUID('{a}')"
        ),
        format!(
            "SELECT name FROM crucible.span_core PREWHERE project_id = toUUID('{b_id}') WHERE {WINDOW}"
        ),
        format!("SELECT name FROM crucible.span_core WHERE {WINDOW} OR 1"),
        format!("SELECT name FROM merge('crucible', '^span_core$') WHERE {WINDOW}"),
        format!(
            "SELECT name FROM view(SELECT name, start_time FROM crucible.span_core) WHERE {WINDOW}"
        ),
        format!("WITH x AS (SELECT name FROM crucible.span_core WHERE {WINDOW}) SELECT * FROM x"),
        format!(
            "SELECT l.name, r.name FROM crucible.span_core AS l CROSS JOIN crucible.span_core AS r WHERE l.{WINDOW}"
        ),
        format!("SELECT groupArray(name) FROM crucible.span_core WHERE {WINDOW}"),
        format!(
            "SELECT name FROM crucible.span_core WHERE {WINDOW} UNION ALL SELECT name FROM crucible.span_core WHERE {WINDOW}"
        ),
        format!("SELECT query FROM system.query_log WHERE {WINDOW}"),
        "SELECT query FROM system.processes WHERE {from:DateTime64(6)} < {to:DateTime64(6)}".to_owned(),
        format!("SELECT * FROM remote('127.0.0.1', 'crucible', 'span_core') WHERE {WINDOW}"),
        "SELECT * FROM url('http://127.0.0.1:8123/?query=SELECT+name+FROM+crucible.span_core', 'LineAsString', 'l String') WHERE {from:DateTime64(6)} < {to:DateTime64(6)}".to_owned(),
        "SELECT * FROM mergeTreeIndex('crucible', 'span_core') WHERE {from:DateTime64(6)} < {to:DateTime64(6)}".to_owned(),
        format!("SELECT name, wal_epoch FROM crucible.span_core WHERE {WINDOW}"),
        format!("SELECT throwIf(1, name) FROM crucible.span_core WHERE {WINDOW}"),
        format!("SELECT toUInt64(name) FROM crucible.span_core WHERE {WINDOW}"),
        format!(
            "EXPLAIN PLAN SELECT name FROM crucible.span_core WHERE {WINDOW} AND project_id = toUUID('{b_id}')"
        ),
    ] {
        let (status, body) = alice.run(&sql);
        if status == 200 {
            for row in body["rows"].as_array().unwrap() {
                assert!(!row.to_string().contains("cnry_b_"), "{sql}: {row}");
            }
        } else {
            assert!(body["code"].is_string(), "{sql}: {status} {body:#}");
        }
        // The limiter allows 30 runs a minute for one key; the attacks stay under it with the pauses below.
        std::thread::sleep(Duration::from_millis(50));
    }
    // A key cannot name another project in the path.
    let cross = Client {
        project: b.to_string(),
        ..alice.clone()
    };
    let (status, body) = cross.run(&spans);
    assert_eq!((status, body["code"].as_str()), (404, Some("not_found")));
    let (status, _, _) = http(
        api,
        "POST",
        &format!("/api/v1/projects/{a}/sql/run"),
        &[],
        b"{}",
    )
    .unwrap();
    assert_eq!(status, 401);

    // The rate limit is 30 runs a minute per key; the next checks use a fresh minute for A.
    let wait_for_rate = || std::thread::sleep(Duration::from_secs(61));
    wait_for_rate();

    // 6. Validation messages: each refusal has its code and says how to fix the query.
    let fix = |body: &Value| body["detail"].as_str().unwrap().to_owned();
    let body = alice.refused("SELECT 1", 422, "sql_time_window_required");
    assert!(fix(&body).contains("{from:DateTime64(6)}"));
    let body = alice.refused("DROP TABLE crucible.span_core", 422, "sql_invalid");
    assert!(fix(&body).contains("SELECT or WITH"));
    assert_eq!(
        (body["line"].as_u64(), body["column"].as_u64()),
        (Some(1), Some(1))
    );
    let body = alice.refused(
        &format!("{spans} SETTINGS max_execution_time = 600"),
        422,
        "sql_invalid",
    );
    assert!(fix(&body).contains("SETTINGS"));
    let body = alice.refused(&format!("{spans} FORMAT CSV"), 422, "sql_invalid");
    assert!(fix(&body).contains("Export"));
    let body = alice.refused(&format!("{spans}; SELECT 2"), 422, "sql_invalid");
    assert!(fix(&body).contains("one statement"));
    // The server's own parser: a syntax error carries ClickHouse's message and its position.
    let body = alice.refused(
        &format!("SELECT name FROMM crucible_sql.spans WHERE {WINDOW}"),
        422,
        "sql_invalid",
    );
    assert_eq!(body["errors"][0]["code"], "clickhouse");
    assert!(
        body["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("Syntax error")
    );
    assert_eq!(body["line"].as_u64(), Some(1));
    let body = alice.refused(
        &format!("SELECT nme FROM crucible_sql.spans WHERE {WINDOW}"),
        422,
        "sql_invalid",
    );
    assert!(fix(&body).contains("schema browser"));
    // A missing parameter value is told before the server is asked.
    let body = alice.refused(
        &format!("{spans} AND name = {{name:String}}"),
        422,
        "validation_failed",
    );
    assert!(fix(&body).contains("name"));

    // 7. Limit mapping: what ClickHouse refuses arrives under the contract's codes.
    let always = "{from:DateTime64(6)} < {to:DateTime64(6)}";
    let body = alice.refused(
        &format!("SELECT count() FROM crucible.span_core WHERE {always}"),
        422,
        "sql_time_window_required",
    );
    assert!(fix(&body).contains("time column"));
    alice.refused(
        &format!("SELECT count() FROM numbers(100000000000) WHERE {always}"),
        422,
        "query_too_broad",
    );
    alice.refused(
        &format!("SELECT sleep(3) WHERE {always}"),
        422,
        "query_too_broad",
    );
    alice.refused(
        &format!("SELECT count() FROM system.query_log WHERE {always}"),
        403,
        "sql_forbidden",
    );
    alice.refused(
        &format!("SELECT wal_epoch FROM crucible.span_core WHERE {WINDOW}"),
        403,
        "sql_forbidden",
    );
    alice.refused(
        &format!("SELECT * FROM url('http://127.0.0.1:8123/ping', 'LineAsString', 'l String') WHERE {always}"),
        403,
        "sql_forbidden",
    );
    // A statement that outlives the profile's 30 s ends with its own problem, not with the request deadline,
    // and its audit row says failed.
    let started = Instant::now();
    let body = alice.refused(
        &format!(
            "SELECT count() FROM numbers(900000000) ARRAY JOIN range(100) AS x WHERE sipHash64(x, number, 1) = 0 AND \
             {always}"
        ),
        504,
        "query_timeout",
    );
    assert!(fix(&body).contains("30 s"));
    assert!(
        started.elapsed() >= Duration::from_secs(25),
        "{:?}",
        started.elapsed()
    );
    // The result cap cuts instead of failing.
    let (status, grid) = alice.run(&format!("SELECT number FROM numbers(50000) WHERE {always}"));
    assert_eq!(status, 200, "{grid:#}");
    assert_eq!(grid["rows"].as_array().unwrap().len(), 10_000);
    assert_eq!(grid["truncated"], true);

    // 8. The concurrency cap and cancellation. A slow statement runs; the same key may not start another; the
    //    history shows it running; a cancel ends it with `conflict` and the audit row says cancelled.
    wait_for_rate();
    let slow =
        format!("SELECT count() FROM numbers(900000000) WHERE sipHash64(number) = 1 AND {always}");
    let runner = {
        let alice = alice.clone();
        let slow = slow.clone();
        std::thread::spawn(move || alice.run(&slow))
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    let running = loop {
        let (_, runs) = alice.json("GET", "runs?limit=5", None);
        let found = runs["data"]
            .as_array()
            .and_then(|runs| runs.iter().find(|run| run["outcome"] == "running").cloned());
        if let Some(run) = found {
            break run;
        }
        assert!(
            Instant::now() < deadline,
            "the slow run never showed as running: {runs:#}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let body = alice.refused(&spans, 429, "sql_quota_exceeded");
    assert!(fix(&body).contains("already have a query running"));
    assert!(body["retry_after_s"].as_u64().is_some());
    // Another project is not held up, and cannot cancel A's run.
    let (status, _) = bob.run(&spans);
    assert_eq!(status, 200);
    let run_id = running["id"].as_str().unwrap();
    let (status, _) = bob.json("DELETE", &format!("runs/{run_id}"), None);
    assert_eq!(status, 404);
    let (status, _, _) = alice.send("DELETE", &format!("runs/{run_id}"), None, &[]);
    assert_eq!(status, 204);
    let (status, body) = runner.join().unwrap();
    // The statement is killed (`conflict`) unless it finished in the instant before the kill landed.
    assert!(
        (status == 409 && body["code"] == "conflict") || status == 200,
        "{status} {body:#}"
    );
    let cancelled = status == 409;

    // 8b. Cancel by disconnect, the path the UI takes: the client closes the connection of a running statement.
    //     The audit row must say cancelled, and ClickHouse must hold no statement of project A's identity.
    // This statement would run into the 30 s limit; it cannot end by itself before the connection closes.
    let endless = format!(
        "SELECT count() FROM numbers(900000000) ARRAY JOIN range(100) AS x WHERE sipHash64(x, number, 2) = 0 AND {always}"
    );
    {
        let body = serde_json::to_vec(&run_body(&endless)).unwrap();
        let mut stream = TcpStream::connect_timeout(&api, Duration::from_secs(5)).unwrap();
        let head = format!(
            "POST /api/v1/projects/{a}/sql/run HTTP/1.1\r\nHost: sqliso\r\nAuthorization: {}\r\nContent-Type: \
             application/json\r\nContent-Length: {}\r\n\r\n",
            alice.auth[0].1,
            body.len()
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(&body).unwrap();
        // Long enough for the two validations and the start of the statement.
        std::thread::sleep(Duration::from_millis(1500));
        drop(stream);
    }
    let workspace_user = format!("crucible_sql_{}", a.to_string().replace('-', ""));
    let deadline = Instant::now() + Duration::from_secs(20);
    let dropped = loop {
        let (_, runs) = alice.json("GET", "runs?limit=1", None);
        let newest = runs["data"][0].clone();
        assert_eq!(newest["sql"], endless, "{newest:#}");
        let still_running = store
            .raw_text(
                ServiceIdentity::Admin,
                &Statement {
                    sql: format!(
                        "SELECT count() FROM system.processes WHERE user = '{workspace_user}'"
                    ),
                    params: Vec::new(),
                    settings: Vec::new(),
                },
            )
            .await
            .unwrap();
        if newest["outcome"] != "running" && still_running.trim() == "0" {
            break newest;
        }
        assert!(
            Instant::now() < deadline,
            "after the client went away the run is {newest:#} and ClickHouse still runs {still_running} statements"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(dropped["outcome"], "cancelled", "{dropped:#}");

    // 9. Audit rows: every run above is recorded with its outcome and code.
    let (status, runs) = alice.json("GET", "runs?limit=200", None);
    assert_eq!(status, 200);
    let runs = runs["data"].as_array().unwrap();
    let outcome = |code: &str| runs.iter().filter(|run| run["error_code"] == code).count();
    assert!(outcome("sql_forbidden") >= 3, "{runs:#?}");
    assert!(outcome("query_too_broad") >= 2);
    let timed_out: Vec<&Value> = runs
        .iter()
        .filter(|run| run["error_code"] == "query_timeout")
        .collect();
    assert_eq!(timed_out.len(), 1, "{runs:#?}");
    assert_eq!(timed_out[0]["outcome"], "failed");
    assert!(
        outcome("sql_invalid") >= 2,
        "server-side syntax errors are recorded"
    );
    assert!(
        runs.iter()
            .any(|run| run["outcome"] == "succeeded" && run["truncated"] == true)
    );
    if cancelled {
        let row = runs.iter().find(|run| run["id"] == run_id).unwrap();
        assert_eq!(row["outcome"], "cancelled", "{row:#}");
    }
    assert!(runs.iter().all(|run| run["actor"]["kind"] == "api_key"));
    // The refused run of the key without the `sql` scope never reached the workspace: it has no audit row.
    // Lexical refusals never reached ClickHouse, so they have no row.
    assert!(!runs.iter().any(|run| run["sql"] == "SELECT 1"));
    // B's history is its own.
    let (_, theirs) = bob.json("GET", "runs?limit=200", None);
    assert!(
        theirs["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|run| run["sql"] == spans)
    );

    // 10. Saved queries: declared parameters only, versions, visibility between projects.
    let input = json!({
        "name": "Spans named",
        "visibility": "project",
        "sql": format!("SELECT name FROM crucible_sql.spans WHERE {WINDOW} AND name = {{name:String}} LIMIT {{n:Int64}}"),
        "parameters": [{ "name": "name", "type": "string" }, { "name": "n", "type": "int", "default": "5" }],
    });
    let (status, body) = alice.json("POST", "saved-queries", Some(&input));
    assert_eq!(
        (status, body["code"].as_str()),
        (403, Some("forbidden")),
        "{body:#}"
    );
    assert!(
        body["detail"].as_str().unwrap().contains("`edit_content`"),
        "{body:#}"
    );
    let (status, head, body) = owner.send("POST", "saved-queries", Some(&input), &[]);
    assert_eq!(status, 201, "{}", String::from_utf8_lossy(&body));
    assert!(head.to_ascii_lowercase().contains("etag: \"1\""), "{head}");
    let saved: Value = serde_json::from_slice(&body).unwrap();
    let saved_id = saved["id"].as_str().unwrap();
    assert_eq!(
        (saved["can_edit"].as_bool(), saved["can_run"].as_bool()),
        (Some(true), Some(true))
    );
    let window = json!({ "from": "2020-01-01T00:00:00Z", "to": "2100-01-01T00:00:00Z" });
    let with = |params: Value| {
        let mut body = window.clone();
        body["params"] = params;
        body
    };
    let run_path = format!("saved-queries/{saved_id}/run");
    let (status, grid) = alice.json("POST", &run_path, Some(&with(json!({ "name": canary_a }))));
    assert_eq!(status, 200, "{grid:#}");
    assert_eq!(grid["rows"], json!([[canary_a]]));
    for bad in [
        json!({}),
        json!({ "name": "x", "extra": "1" }),
        json!({ "name": "x", "n": "1 OR 1" }),
    ] {
        let (status, body) = alice.json("POST", &run_path, Some(&with(bad.clone())));
        assert_eq!(
            (status, body["code"].as_str()),
            (422, Some("validation_failed")),
            "{bad}"
        );
    }
    // Another project cannot see, run or change it: not its key, and not the owner acting on project B.
    let (status, _) = bob.json("GET", &format!("saved-queries/{saved_id}"), None);
    assert_eq!(status, 404);
    for (method, path) in [
        ("GET", format!("saved-queries/{saved_id}")),
        ("DELETE", format!("saved-queries/{saved_id}")),
    ] {
        let (status, _) = owner_on_b.json(method, &path, None);
        assert_eq!(status, 404, "{method} {path}");
    }
    // The key sees it and may run it, not edit it.
    let (status, seen) = alice.json("GET", &format!("saved-queries/{saved_id}"), None);
    assert_eq!(status, 200, "{seen:#}");
    assert_eq!(
        (seen["can_edit"].as_bool(), seen["can_run"].as_bool()),
        (Some(false), Some(true))
    );
    let (status, _) = bob.json("POST", &run_path, Some(&with(json!({ "name": "x" }))));
    assert_eq!(status, 404);
    let (_, listed) = bob.json("GET", "saved-queries", None);
    assert_eq!(listed["data"], json!([]));
    // Changing it needs the current version.
    let (status, _, _) = owner.send(
        "PATCH",
        &format!("saved-queries/{saved_id}"),
        Some(&input),
        &[],
    );
    assert_eq!(status, 428);
    let (status, _, _) = owner.send(
        "PATCH",
        &format!("saved-queries/{saved_id}"),
        Some(&input),
        &[("If-Match", "\"1\"")],
    );
    assert_eq!(status, 200);
    let (status, _, _) = owner.send(
        "PATCH",
        &format!("saved-queries/{saved_id}"),
        Some(&input),
        &[("If-Match", "\"1\"")],
    );
    assert_eq!(status, 412);
    // A saved query whose text uses an undeclared parameter is refused when it is saved.
    let mut undeclared = input.clone();
    undeclared["parameters"] = json!([]);
    let (status, body) = owner.json("POST", "saved-queries", Some(&undeclared));
    assert_eq!(
        (status, body["code"].as_str()),
        (422, Some("validation_failed"))
    );
    let (status, _, _) = owner.send("DELETE", &format!("saved-queries/{saved_id}"), None, &[]);
    assert_eq!(status, 204);

    // 11. Exports: CSV carries the formula guard; JSON lines and Parquet are the server's own encodings.
    let export = |format: &str| {
        let mut body = run_body(&format!(
            "SELECT name, user_id FROM crucible_sql.spans WHERE {WINDOW}"
        ));
        body["format"] = json!(format);
        alice.send("POST", "export", Some(&body), &[])
    };
    let (status, head, body) = export("csv");
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert!(head.contains("crucible-sql.csv"), "{head}");
    assert_eq!(
        String::from_utf8_lossy(&body),
        format!("name,user_id\r\n{canary_a},'=1+1\r\n")
    );
    let (status, _, body) = export("jsonl");
    assert_eq!(status, 200);
    let line: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(line, json!({ "name": canary_a, "user_id": "=1+1" }));
    let (status, head, body) = export("parquet");
    assert_eq!(status, 200);
    assert!(head.contains("crucible-sql.parquet"));
    assert!(
        body.starts_with(b"PAR1") && body.ends_with(b"PAR1"),
        "a Parquet file"
    );

    // 12. Method and path discipline.
    let (status, head, _) = alice.send("GET", "run", None, &[]);
    assert_eq!(status, 405);
    assert!(head.to_ascii_lowercase().contains("allow: post"), "{head}");
    // An export never cuts a result without a word: a result longer than it holds is refused.
    for format in ["csv", "jsonl"] {
        let mut body = run_body(&format!("SELECT number FROM numbers(50000) WHERE {always}"));
        body["format"] = json!(format);
        let (status, _, bytes) = alice.send("POST", "export", Some(&body), &[]);
        let problem: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        assert_eq!(
            (status, problem["code"].as_str()),
            (422, Some("query_too_broad")),
            "{format}: {problem:#}"
        );
        assert!(fix(&problem).contains("10,000 rows"));
    }
    eprintln!(
        "sqliso layer C: {} audit rows for project A; cancellation observed: {cancelled}; a dropped connection \
         cancelled its statement",
        runs.len()
    );
}
