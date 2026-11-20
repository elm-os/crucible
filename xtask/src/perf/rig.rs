//! The rig of the A/B gate: the two images under test, the load generator binary, and the Docker containers
//! of a session. Everything reaches Docker through the `docker` CLI and moves files with `docker cp` and
//! stdout, never with bind mounts, so the gate also works from a container that talks to the host's Docker
//! socket (the daemon resolves bind paths on its own filesystem). Every container, volume and network of a
//! session carries the label `crucible.perf-gate=<session>` and is removed when the session ends.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::gate::{Kind, Power};

/// The ClickHouse of the rig, the version the Compose file bundles.
pub const CLICKHOUSE_IMAGE: &str = "clickhouse/clickhouse-server:26.8";
/// The Postgres of the rig, the version the Compose file bundles.
pub const POSTGRES_IMAGE: &str = "postgres:18";
/// The image of the load generator's container and of the probe: the dev image, whose C library matches
/// the binary that cargo builds in it.
pub const CLIENT_IMAGE: &str = "crucible-dev:1";
/// Images under test are tagged `crucible-gate:<commit>`.
pub const IMAGE_REPOSITORY: &str = "crucible-gate";
/// Connections of the load generator in both measurements.
pub const CONCURRENCY: u32 = 128;
/// The host's AC-power state, as a wrapper that can see it passes it in.
pub const AC_POWER_VAR: &str = "CRUCIBLE_PERF_AC_POWER";
const LABEL: &str = "crucible.perf-gate";
const LOADGEN_PATH: &str = "/usr/local/bin/crucible-loadgen";

/// One container's CPUs and memory.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// `--cpuset-cpus`: one CPU (`16`) or a range (`0-5`).
    pub cpuset: &'static str,
    /// `--memory`.
    pub memory: &'static str,
}

impl Limits {
    fn range(&self) -> (usize, usize) {
        let (first, last) = self
            .cpuset
            .split_once('-')
            .unwrap_or((self.cpuset, self.cpuset));
        (first.parse().unwrap_or(0), last.parse().unwrap_or(0))
    }

    /// The CPUs in the set.
    pub fn cpus(&self) -> usize {
        let (first, last) = self.range();
        last + 1 - first.min(last)
    }

    /// `memory` in bytes: a number with an optional `k`, `m` or `g` suffix, as `docker run --memory` takes it.
    pub fn memory_bytes(&self) -> u64 {
        let (digits, unit) = match self.memory.strip_suffix(['k', 'm', 'g']) {
            Some(digits) if self.memory.ends_with('k') => (digits, 1 << 10),
            Some(digits) if self.memory.ends_with('m') => (digits, 1 << 20),
            Some(digits) => (digits, 1 << 30),
            None => (self.memory, 1),
        };
        digits.parse::<u64>().map_or(0, |n| n.saturating_mul(unit))
    }

    fn args(&self) -> Vec<String> {
        vec![
            "--cpuset-cpus".to_owned(),
            self.cpuset.to_owned(),
            "--cpus".to_owned(),
            self.cpus().to_string(),
            "--memory".to_owned(),
            self.memory.to_owned(),
        ]
    }
}

/// A rig size: the containers' limits and the two measurements' schedules.
#[derive(Debug)]
pub struct RigSpec {
    /// `full` or `small`.
    pub name: &'static str,
    /// Small-rig results are smoke runs: labelled, never posted.
    pub smoke: bool,
    /// The server under test.
    pub crucible: Limits,
    /// The telemetry store.
    pub clickhouse: Limits,
    /// The control plane.
    pub postgres: Limits,
    /// The load generator.
    pub loadgen: Limits,
    /// `--recent-max-bytes`. The default is a share of the host's memory, which in a container is the VM's.
    pub recent_max_bytes: u64,
    /// Seconds of discarded load at the latency rate before each measurement.
    pub warmup_s: u64,
    /// Requests per second the throughput measurement offers, above what either build can take.
    pub throughput_rps: u64,
    /// The throughput schedule's length; an overloaded run lasts longer, until every request is sent.
    pub throughput_s: u64,
    /// Requests per second of the latency measurement, below what either build can take.
    pub latency_rps: u64,
    /// The latency schedule's length.
    pub latency_s: u64,
    /// Requests per second the sustained measurement offers: above what the end-to-end path keeps up with,
    /// below what the ack path takes.
    pub sustained_rps: u64,
    /// The sustained schedule's length; the arm then waits for the drain.
    pub sustained_s: u64,
    /// Requests per second of the freshness measurement, below what the end-to-end path keeps up with.
    pub freshness_rps: u64,
    /// The freshness schedule's length.
    pub freshness_s: u64,
    /// The longest an end-to-end arm waits, after its load, until the store holds every acknowledged span.
    pub drain_s: u64,
}

impl RigSpec {
    /// The containers' limits by role.
    pub fn containers(&self) -> [(&'static str, Limits); 4] {
        [
            ("crucible", self.crucible),
            ("clickhouse", self.clickhouse),
            ("postgres", self.postgres),
            ("loadgen", self.loadgen),
        ]
    }

    /// Every CPU that one of the rig's containers pins.
    pub fn cpus(&self) -> Vec<usize> {
        self.containers()
            .iter()
            .flat_map(|(_, limits)| {
                let (first, last) = limits.range();
                first..=last
            })
            .collect()
    }

    /// The requests per second and seconds of a measurement.
    pub fn schedule(&self, kind: Kind) -> (u64, u64) {
        match kind {
            Kind::Throughput => (self.throughput_rps, self.throughput_s),
            Kind::Latency => (self.latency_rps, self.latency_s),
            Kind::Sustained => (self.sustained_rps, self.sustained_s),
            Kind::Freshness => (self.freshness_rps, self.freshness_s),
        }
    }
}

/// The reference rig of `docs/testing/perf.md`: disjoint cpusets on a 24-CPU VM, 14 GB in all.
pub const FULL: RigSpec = RigSpec {
    name: "full",
    smoke: false,
    crucible: Limits {
        cpuset: "0-5",
        memory: "3g",
    },
    clickhouse: Limits {
        cpuset: "6-15",
        memory: "9g",
    },
    postgres: Limits {
        cpuset: "16",
        memory: "1g",
    },
    loadgen: Limits {
        cpuset: "17-22",
        memory: "1g",
    },
    recent_max_bytes: 256 << 20,
    warmup_s: 3,
    throughput_rps: 20_000,
    throughput_s: 10,
    latency_rps: 1_000,
    latency_s: 30,
    sustained_rps: 1_500,
    sustained_s: 20,
    freshness_rps: 300,
    freshness_s: 30,
    drain_s: 300,
};

/// Reduced limits and short schedules for smoke runs on a busy machine, 3.5 GB in all.
pub const SMALL: RigSpec = RigSpec {
    name: "small",
    smoke: true,
    crucible: Limits {
        cpuset: "0-1",
        memory: "1g",
    },
    clickhouse: Limits {
        cpuset: "2-3",
        memory: "1536m",
    },
    postgres: Limits {
        cpuset: "4",
        memory: "512m",
    },
    loadgen: Limits {
        cpuset: "5-6",
        memory: "512m",
    },
    recent_max_bytes: 64 << 20,
    warmup_s: 2,
    throughput_rps: 10_000,
    throughput_s: 3,
    latency_rps: 100,
    latency_s: 5,
    sustained_rps: 1_000,
    sustained_s: 10,
    freshness_rps: 100,
    freshness_s: 10,
    drain_s: 180,
};

/// The span size and encoding of a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    /// 1 KiB spans, 100 per request.
    Kib1,
    /// 10 KiB spans, 10 per request, which keep the generator accurate (`docs/testing/perf-baseline.md`).
    Kib10,
    /// [`Lane::Kib1`] with gzip bodies.
    Kib1Gzip,
    /// [`Lane::Kib10`] with gzip bodies.
    Kib10Gzip,
}

impl Lane {
    /// `1kb`, `10kb`, `1kb-gzip` or `10kb-gzip`.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "1kb" => Some(Self::Kib1),
            "10kb" => Some(Self::Kib10),
            "1kb-gzip" => Some(Self::Kib1Gzip),
            "10kb-gzip" => Some(Self::Kib10Gzip),
            _ => None,
        }
    }

    /// The lane's name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Kib1 => "1kb",
            Self::Kib10 => "10kb",
            Self::Kib1Gzip => "1kb-gzip",
            Self::Kib10Gzip => "10kb-gzip",
        }
    }

    /// The load generator's `--span-size`.
    pub fn span_size(self) -> &'static str {
        match self {
            Self::Kib1 | Self::Kib1Gzip => "1k",
            Self::Kib10 | Self::Kib10Gzip => "10k",
        }
    }

    /// The load generator's `--spans-per-request`.
    pub fn spans_per_request(self) -> u32 {
        match self {
            Self::Kib1 | Self::Kib1Gzip => 100,
            Self::Kib10 | Self::Kib10Gzip => 10,
        }
    }

    /// Whether the bodies are gzipped (the load generator's `--gzip`).
    pub fn gzip(self) -> bool {
        matches!(self, Self::Kib1Gzip | Self::Kib10Gzip)
    }
}

/// The path a gate run measures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GatePath {
    /// Receive to durable acknowledgement: acknowledged spans/s in overload, and durable-ack p99 at a fixed rate.
    Ack,
    /// Receive to readable in the store: shipped spans/s over an overloaded run and its drain, and freshness p95
    /// at a fixed rate, each with the zero-loss check.
    E2e,
}

impl GatePath {
    /// `ack` or `e2e`.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "ack" => Some(Self::Ack),
            "e2e" => Some(Self::E2e),
            _ => None,
        }
    }

    /// The path's name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Ack => "ack",
            Self::E2e => "e2e",
        }
    }

    /// The path's two measurements: capacity, then latency.
    pub fn kinds(self) -> [Kind; 2] {
        match self {
            Self::Ack => [Kind::Throughput, Kind::Latency],
            Self::E2e => [Kind::Sustained, Kind::Freshness],
        }
    }
}

/// Describes a command by its program and first argument; the rest can hold secrets.
fn describe(command: &Command) -> String {
    let first = command
        .get_args()
        .next()
        .map(|arg| arg.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("{} {first}", command.get_program().to_string_lossy())
}

fn finish(
    what: &str,
    ok: bool,
    status: &str,
    stdout: String,
    stderr: &str,
) -> Result<String, String> {
    if ok {
        Ok(stdout)
    } else {
        Err(format!("{what} failed ({status}): {}", stderr.trim()))
    }
}

/// Runs `command` and returns its stdout. A failure carries stderr.
fn capture(command: &mut Command) -> Result<String, String> {
    let what = describe(command);
    let output = command
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{what}: {e}"))?;
    finish(
        &what,
        output.status.success(),
        &output.status.to_string(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        &String::from_utf8_lossy(&output.stderr),
    )
}

fn drain(pipe: Option<impl Read + Send + 'static>) -> JoinHandle<String> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

/// [`capture`] with a deadline, after which the process is killed.
fn capture_within(command: &mut Command, deadline: Duration) -> Result<String, String> {
    let what = describe(command);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{what}: {e}"))?;
    let (stdout, stderr) = (drain(child.stdout.take()), drain(child.stderr.take()));
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| format!("{what}: {e}"))? {
            break status;
        }
        if started.elapsed() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "{what} gave no result within {} s and was stopped",
                deadline.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    finish(
        &what,
        status.success(),
        &status.to_string(),
        stdout,
        &stderr,
    )
}

fn docker<I, S>(args: I) -> Result<String, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    capture(Command::new("docker").args(args))
}

/// Polls `ready` every second until it holds, or fails after `limit`.
fn wait(what: &str, limit: Duration, mut ready: impl FnMut() -> bool) -> Result<(), String> {
    let started = Instant::now();
    while !ready() {
        if started.elapsed() > limit {
            return Err(format!("{what} was not ready after {} s", limit.as_secs()));
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    Ok(())
}

/// The full SHA of the commit `reference` names.
pub fn resolve(root: &Path, reference: &str) -> Result<String, String> {
    capture(Command::new("git").current_dir(root).args([
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("{reference}^{{commit}}"),
    ]))
    .map(|sha| sha.trim().to_owned())
    .map_err(|_| {
        format!(
            "`{reference}` is not a commit in this repository: fetch it first (git fetch origin) or pass a \
             commit SHA"
        )
    })
}

/// The image of commit `sha` and its id: `crucible-gate:<sha>` when it exists, otherwise built from
/// `deploy/docker/Dockerfile` in a clean export of the commit (`git archive`), so that nothing uncommitted
/// reaches the build and the shared git directory is only read. `CARGO_BUILD_JOBS`, when set, is passed on.
pub fn image(root: &Path, sha: &str) -> Result<(String, String), String> {
    let tag = format!("{IMAGE_REPOSITORY}:{sha}");
    let inspect = |tag: &str| {
        docker(["image", "inspect", "--format", "{{.Id}}", tag]).map(|id| id.trim().to_owned())
    };
    if let Ok(id) = inspect(&tag) {
        println!("perf: reusing image {tag}");
        return Ok((tag, id));
    }
    let dir = std::env::temp_dir().join(format!("crucible-gate-src-{sha}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let built = export(root, sha, &dir).and_then(|()| build(&dir, &tag, sha));
    let _ = fs::remove_dir_all(&dir);
    built?;
    Ok((tag.clone(), inspect(&tag)?))
}

fn export(root: &Path, sha: &str, dir: &Path) -> Result<(), String> {
    let mut git = Command::new("git")
        .current_dir(root)
        .args(["archive", "--format=tar", sha])
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("git archive: {e}"))?;
    let tar = match git.stdout.take() {
        Some(archive) => Command::new("tar")
            .arg("-x")
            .arg("-C")
            .arg(dir)
            .stdin(archive)
            .status()
            .map_err(|e| format!("tar: {e}")),
        None => Err("git archive: no output".to_owned()),
    };
    let git = git.wait().map_err(|e| format!("git archive: {e}"))?;
    match tar? {
        tar if tar.success() && git.success() => Ok(()),
        tar => Err(format!(
            "exporting {sha} failed (git archive {git}, tar {tar})"
        )),
    }
}

fn build(dir: &Path, tag: &str, sha: &str) -> Result<(), String> {
    println!(
        "perf: building {tag} from a clean export of the commit with deploy/docker/Dockerfile"
    );
    let mut command = Command::new("docker");
    command
        .arg("build")
        .arg("--file")
        .arg(dir.join("deploy").join("docker").join("Dockerfile"))
        .args(["--label", &format!("{LABEL}.commit={sha}"), "--tag", tag]);
    if let Ok(jobs) = std::env::var("CARGO_BUILD_JOBS") {
        command.args(["--build-arg", &format!("CARGO_BUILD_JOBS={jobs}")]);
    }
    let status = command
        .arg(dir)
        .stdin(Stdio::null())
        .status()
        .map_err(|e| format!("docker build: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "docker build of {tag} failed ({status}); its output is above"
        ))
    }
}

/// Builds `crucible-loadgen` in release from this checkout and returns the binary's path. One binary
/// measures both arms.
pub fn build_loadgen(root: &Path) -> Result<PathBuf, String> {
    println!("perf: building crucible-loadgen (release) from this checkout");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .current_dir(root)
        .args([
            "build",
            "--release",
            "--locked",
            "--package",
            "crucible-loadgen",
            "--bin",
            "crucible-loadgen",
            "--message-format=json-render-diagnostics",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("cargo build: {e}"))?;
    if !output.status.success() {
        return Err("building crucible-loadgen failed; cargo's errors are above".to_owned());
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|m| m["reason"] == "compiler-artifact" && m["target"]["name"] == "crucible-loadgen")
        .find_map(|m| m["executable"].as_str().map(PathBuf::from))
        .ok_or_else(|| "cargo did not report the crucible-loadgen executable".to_owned())
}

/// `docker info` in one line, and the Docker host's CPU count.
pub fn docker_info() -> Result<(String, usize), String> {
    let text = docker([
        "info",
        "--format",
        "{{.ServerVersion}}|{{.NCPU}}|{{.MemTotal}}|{{.KernelVersion}}|{{.OperatingSystem}}",
    ])?;
    let parts: Vec<&str> = text.trim().split('|').collect();
    let number = |i: usize| parts.get(i).and_then(|p| p.parse::<u64>().ok());
    let cpus = number(1).ok_or_else(|| format!("docker info: unexpected `{}`", text.trim()))?;
    let gib = number(2).unwrap_or(0) as f64 / f64::from(1u32 << 30);
    let info = format!(
        "Docker {}, {cpus} CPUs, {gib:.1} GiB, kernel {}, {}",
        parts[0],
        parts.get(3).unwrap_or(&"?"),
        parts.get(4).unwrap_or(&"?")
    );
    Ok((info, usize::try_from(cpus).unwrap_or(usize::MAX)))
}

/// Refuses a rig whose cpusets name CPUs the Docker host does not have.
pub fn check_cpus(rig: &RigSpec, cpus: usize) -> Result<(), String> {
    let needed = rig
        .containers()
        .iter()
        .map(|(_, limits)| limits.range().1 + 1)
        .max()
        .unwrap_or(0);
    if cpus >= needed {
        Ok(())
    } else {
        Err(format!(
            "the {} rig pins CPUs 0 to {} but this Docker host has {cpus}: use --rig small, or a host with at \
             least {needed} CPUs",
            rig.name,
            needed - 1
        ))
    }
}

/// The AC-power state: what the host reported through [`AC_POWER_VAR`], or else what Linux shows in
/// `/sys/class/power_supply`. A container on a Windows or macOS host sees neither, so the state is unknown
/// unless the wrapper `tools/perf/xtask.sh` passes it in.
pub fn ac_power() -> Power {
    match std::env::var(AC_POWER_VAR) {
        Ok(state) if state == "online" || state == "battery" => Power {
            state,
            source: format!("reported by the host through {AC_POWER_VAR}"),
        },
        Ok(other) => Power {
            state: "unknown".to_owned(),
            source: format!("{AC_POWER_VAR} is `{other}`, not online or battery"),
        },
        Err(_) => sysfs_power(Path::new("/sys/class/power_supply")),
    }
}

/// Online when an AC adapter (a supply of type `Mains`) in `dir` is online, battery when every adapter is
/// offline, unknown without an adapter.
pub fn sysfs_power(dir: &Path) -> Power {
    let read = |path: PathBuf| {
        fs::read_to_string(path)
            .map(|text| text.trim().to_owned())
            .unwrap_or_default()
    };
    let (mut adapters, mut online) = (0, false);
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        if read(entry.path().join("type")) == "Mains" {
            adapters += 1;
            online |= read(entry.path().join("online")) == "1";
        }
    }
    let source = format!("{} on this machine", dir.display());
    match (adapters, online) {
        (0, _) => Power {
            state: "unknown".to_owned(),
            source: format!(
                "no AC adapter in {}; a container cannot see its host's, so run through tools/perf/xtask.sh, \
                 which passes {AC_POWER_VAR}",
                dir.display()
            ),
        },
        (_, true) => Power {
            state: "online".to_owned(),
            source,
        },
        (_, false) => Power {
            state: "battery".to_owned(),
            source,
        },
    }
}

/// Random bytes from the operating system.
fn random(n: usize) -> Result<Vec<u8>, String> {
    let mut bytes = vec![0; n];
    getrandom::fill(&mut bytes).map_err(|e| format!("random bytes: {e}"))?;
    Ok(bytes)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The throwaway secrets of a session's rig, which no published port exposes.
struct Secrets {
    clickhouse: String,
    postgres: String,
    master_key: String,
    /// The password of the first owner, which `crucible admin init` reads from a file.
    owner_password: String,
}

impl Secrets {
    fn new() -> Result<Self, String> {
        Ok(Self {
            clickhouse: hex(&random(16)?),
            postgres: hex(&random(16)?),
            master_key: hex(&random(32)?),
            owner_password: hex(&random(16)?),
        })
    }
}

/// The path of the server binary in the images under test.
const CRUCIBLE_PATH: &str = "/usr/local/bin/crucible";
/// The port of Crucible's metrics listener on the session's network.
pub const METRICS_PORT: u16 = 9464;
/// Where each Crucible container finds the session's files, among them the owner's password.
const CONFIG_DIR: &str = "/etc/crucible";

/// The project key of one arm's rig, made with `crucible admin create-key`: with a control plane, keys live in
/// Postgres, and the server refuses a keys file (contract 11).
#[derive(Clone)]
pub struct ArmKey {
    /// The project the key belongs to, whose rows the end-to-end checks read.
    pub project_id: String,
    /// The secret key, sent as a Bearer token.
    pub secret: String,
}

impl std::fmt::Debug for ArmKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArmKey")
            .field("project_id", &self.project_id)
            .finish_non_exhaustive()
    }
}

impl ArmKey {
    /// Reads the `project_id = "..."` and `secret_key = "..."` lines that `crucible admin create-key` prints.
    /// The project id goes into SQL text and the secret into a header, so both are checked for their shapes.
    pub fn parse(printed: &str) -> Result<Self, String> {
        let field = |name: &str| {
            printed.lines().find_map(|line| {
                let (key, value) = line.split_once('=')?;
                (key.trim() == name).then(|| value.trim().trim_matches('"').to_owned())
            })
        };
        let project_id = field("project_id").unwrap_or_default();
        let secret = field("secret_key").unwrap_or_default();
        let uuid = project_id.len() == 36
            && project_id.chars().enumerate().all(|(i, c)| {
                if [8, 13, 18, 23].contains(&i) {
                    c == '-'
                } else {
                    c.is_ascii_hexdigit()
                }
            });
        let key = secret.starts_with("csk_")
            && secret.len() > 4
            && secret
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_');
        if uuid && key {
            Ok(Self { project_id, secret })
        } else {
            Err("crucible admin create-key printed no project id and secret key of the expected shapes".to_owned())
        }
    }
}

/// What one arm measured.
#[derive(Debug)]
pub struct ArmOutput {
    /// The load generator's JSON report.
    pub report: String,
    /// The spans per second the schedule offered.
    pub offered_spans_per_s: f64,
    /// Rig containers that were not running after the measurement.
    pub stopped: Vec<String>,
    /// What else the machine did during the measurement.
    pub context: Context,
    /// What the store held after an end-to-end measurement drained.
    pub stored: Option<Stored>,
}

/// What the store held after an end-to-end measurement, read from ClickHouse once it held every acknowledged
/// span of the run (or the drain limit passed).
#[derive(Clone, Debug, Serialize)]
pub struct Stored {
    /// Spans the measured run acknowledged.
    pub acked_spans: u64,
    /// Seconds from the schedule's start to the end of the last `span_core` insert (`system.query_log`), once
    /// the store held every acknowledged span; `None` when it did not within the drain limit.
    pub drained_s: Option<f64>,
    /// Of `drained_s`, the seconds after the run's last response: the backlog the store still had to take.
    pub drain_after_run_s: Option<f64>,
    /// Acknowledged spans per second over the run and its drain; 0 when it did not drain.
    pub shipped_spans_per_s: f64,
    /// Freshness p95 over the run's spans, in microseconds: from `received_at` to the end of the `span_core`
    /// insert that made the span readable.
    pub freshness_p95_us: u64,
    /// The spans that the freshness covers.
    pub freshness_spans: u64,
    /// Acknowledged spans of the run whose sequence number `span_payload` lacks.
    pub lost_spans: u64,
    /// Rows beyond one per span: in `span_payload` by sequence number, plus in `span_core` by span.
    pub duplicate_spans: u64,
}

/// The fields of a `crucible-loadgen.run.v1` report that the end-to-end checks read.
#[derive(Debug, Deserialize)]
struct RunFields {
    run_id: String,
    started_at: String,
    lane: RunLane,
    outcome: RunOutcomeFields,
    not_acked_requests: Vec<[u64; 2]>,
}

#[derive(Debug, Deserialize)]
struct RunLane {
    spans_per_request: u64,
}

#[derive(Debug, Deserialize)]
struct RunOutcomeFields {
    acked_spans: u64,
    elapsed_s: f64,
}

impl RunFields {
    fn parse(report: &str) -> Result<Self, String> {
        let fields: Self = serde_json::from_str(report)
            .map_err(|e| format!("the load generator's report: {e}"))?;
        // Both go into SQL text: allow only the characters of a UUID and of an RFC 3339 time.
        let safe = |s: &str| {
            s.chars()
                .all(|c| c.is_ascii_hexdigit() || "-:.TZ".contains(c))
        };
        if safe(&fields.run_id) && safe(&fields.started_at) {
            Ok(fields)
        } else {
            Err("the load generator's report has an unexpected run id or start time".to_owned())
        }
    }
}

/// A JSON number, or a string that holds one: ClickHouse quotes 64-bit integers in JSON.
fn number(value: &serde_json::Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}

/// What else the Docker host did while an arm measured, to tell an order effect from other jobs' load: the
/// busy CPU time of the CPUs the rig pins and of the other CPUs, the load average, and the containers that ran
/// outside the session. The other jobs' containers have no cpuset, so they can run on the rig's CPUs too.
#[derive(Clone, Debug, Serialize)]
pub struct Context {
    /// When the measurement started and finished, UTC.
    pub started_at: String,
    /// See `started_at`.
    pub finished_at: String,
    /// Busy cores, averaged over the measurement, on the CPUs that the rig's containers pin.
    pub rig_cpus_busy: f64,
    /// Busy cores, averaged over the measurement, on the Docker host's other CPUs.
    pub other_cpus_busy: f64,
    /// The host's 1-minute load average before the measurement.
    pub load_before: f64,
    /// The 1-minute load average after it.
    pub load_after: f64,
    /// The share of the measurement during which the busiest block device of the Docker host had I/O in flight
    /// (from `/proc/diskstats`): the WAL's flushes wait behind other jobs' writes on a shared disk.
    pub disk_busy: f64,
    /// Times the Docker host's page cache was dropped during the measurement (`drop_pagecache` in
    /// `/proc/vmstat`): the store's reads after a drop go to the disk until the cache fills again.
    pub page_cache_drops: u64,
    /// The containers that ran outside the session when the measurement started.
    pub others: Vec<String>,
}

/// The most column statistics a read may load once the store has settled, in microseconds per read. A fresh
/// snapshot costs a read nothing; a stale one makes it load every part's statistics, which for the small rig's
/// whole store, 81 parts, cost about 75 ms per read.
pub const SETTLED_STATISTICS_US: u64 = 5_000;

/// The ClickHouse users of Crucible's reads: the service identities `crucible_read_ui` and `crucible_read_agent`.
const READ_USERS: &str = "'crucible_read_ui', 'crucible_read_agent'";

/// The column statistics that Crucible's reads of one set of tables loaded, from [`Session::read_statistics`].
#[derive(Clone, Debug, Serialize)]
pub struct StatisticsLoad {
    /// The tables the reads touched, as `system.query_log` names them.
    pub tables: String,
    /// The reads.
    pub reads: u64,
    /// Their `LoadedStatisticsMicroseconds`, in total.
    pub load_us: u64,
}

/// The wait of [`Session::settle_statistics`].
#[derive(Clone, Copy, Debug)]
pub struct Settle {
    /// How long it waited.
    pub waited: Duration,
    /// Whether the reads settled within the limit.
    pub settled: bool,
    /// The column statistics its last probe loaded, in microseconds per read.
    pub load_us: u64,
}

/// Milliseconds spent with I/O in flight per block device, from `/proc/diskstats` (its 13th field).
pub fn disk_ticks(diskstats: &str) -> BTreeMap<String, u64> {
    diskstats
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            Some(((*fields.get(2)?).to_owned(), fields.get(12)?.parse().ok()?))
        })
        .collect()
}

/// The busiest device's share of `elapsed` with I/O in flight between two `/proc/diskstats` readings.
pub fn disk_busy(
    before: &BTreeMap<String, u64>,
    after: &BTreeMap<String, u64>,
    elapsed: Duration,
) -> f64 {
    let elapsed_ms = elapsed.as_secs_f64() * 1e3;
    if elapsed_ms <= 0.0 {
        return 0.0;
    }
    after
        .iter()
        .filter_map(|(device, ticks)| Some(ticks.saturating_sub(*before.get(device)?)))
        .map(|busy| (busy as f64 / elapsed_ms).min(1.0))
        .fold(0.0, f64::max)
}

/// One reading of the Docker host's activity: CPU ticks, the 1-minute load average, the block devices' busy
/// time, the page-cache drops so far, and when it was taken.
struct HostSample {
    cpus: BTreeMap<usize, (u64, u64)>,
    load: f64,
    disks: BTreeMap<String, u64>,
    page_cache_drops: u64,
    at: Instant,
}

/// The page-cache drops since boot, from the `drop_pagecache` line of `/proc/vmstat` (0 when it has none).
pub fn page_cache_drops(vmstat: &str) -> u64 {
    vmstat
        .lines()
        .find_map(|line| line.strip_prefix("drop_pagecache "))
        .and_then(|count| count.trim().parse().ok())
        .unwrap_or(0)
}

/// Busy and total time per CPU, in clock ticks, from the `cpuN` lines of `/proc/stat`.
pub fn cpu_ticks(stat: &str) -> BTreeMap<usize, (u64, u64)> {
    stat.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let cpu = fields.next()?.strip_prefix("cpu")?.parse().ok()?;
            // user nice system idle iowait irq softirq steal; guest time is already in user.
            let ticks: Vec<u64> = fields.take(8).map(|f| f.parse().unwrap_or(0)).collect();
            let total: u64 = ticks.iter().sum();
            let idle = ticks.get(3).copied().unwrap_or(0) + ticks.get(4).copied().unwrap_or(0);
            Some((cpu, (total.saturating_sub(idle), total)))
        })
        .collect()
}

/// Busy cores over the interval between two `/proc/stat` readings, on the CPUs of `set` and on the others.
pub fn busy_cores(
    before: &BTreeMap<usize, (u64, u64)>,
    after: &BTreeMap<usize, (u64, u64)>,
    set: &[usize],
) -> (f64, f64) {
    let (mut inside, mut outside) = (0.0, 0.0);
    for (cpu, (busy, total)) in after {
        let Some((busy0, total0)) = before.get(cpu) else {
            continue;
        };
        let elapsed = total.saturating_sub(*total0);
        if elapsed == 0 {
            continue;
        }
        let share = busy.saturating_sub(*busy0) as f64 / elapsed as f64;
        if set.contains(cpu) {
            inside += share;
        } else {
            outside += share;
        }
    }
    (inside, outside)
}

/// The 1-minute load average from `/proc/loadavg`.
fn load_1m(loadavg: &str) -> f64 {
    loadavg
        .split_whitespace()
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0.0)
}

/// A gate session: its network, the load generator's container, and the rig of the arm in progress.
#[derive(Debug)]
pub struct Session<'r> {
    rig: &'r RigSpec,
    id: String,
    secrets: Secrets,
    /// A local directory with the owner's password, copied to [`CONFIG_DIR`] in each Crucible container.
    config: PathBuf,
    network: Option<String>,
    client: Option<String>,
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secrets(redacted)")
    }
}

impl<'r> Session<'r> {
    /// A session with fresh secrets; nothing runs yet.
    pub fn new(rig: &'r RigSpec) -> Result<Self, String> {
        let id = hex(&random(4)?);
        let secrets = Secrets::new()?;
        let config = std::env::temp_dir()
            .join(format!("crucible-gate-{id}"))
            .join("crucible");
        fs::create_dir_all(&config).map_err(|e| format!("{}: {e}", config.display()))?;
        fs::write(
            config.join("owner-password"),
            format!("{}\n", secrets.owner_password),
        )
        .map_err(|e| format!("{}: {e}", config.display()))?;
        Ok(Self {
            rig,
            id,
            secrets,
            config,
            network: None,
            client: None,
        })
    }

    fn name(&self, role: &str) -> String {
        format!("crucible-gate-{}-{role}", self.id)
    }

    fn label(&self) -> String {
        format!("{LABEL}={}", self.id)
    }

    /// `docker run -d` or `docker create` of a labelled container of this session with `limits`.
    fn container(
        &self,
        verb: &[&str],
        name: &str,
        limits: Limits,
        rest: &[String],
    ) -> Result<(), String> {
        let mut args: Vec<String> = verb.iter().map(|&s| s.to_owned()).collect();
        args.extend(["--name".to_owned(), name.to_owned()]);
        args.extend(["--label".to_owned(), self.label()]);
        if let Some(network) = &self.network {
            args.extend(["--network".to_owned(), network.clone()]);
        }
        args.extend(limits.args());
        args.extend(rest.iter().cloned());
        docker(&args).map(drop)
    }

    fn copy_in(&self, from: &Path, to: &str) -> Result<(), String> {
        capture(Command::new("docker").arg("cp").arg(from).arg(to)).map(drop)
    }

    fn fresh_volume(&self) -> Result<String, String> {
        let volume = self.name("wal");
        let _ = docker(["volume", "rm", "--force", &volume]);
        docker(["volume", "create", "--label", &self.label(), &volume])?;
        Ok(volume)
    }

    /// Creates the session's network and starts the load generator's container with `loadgen` copied in.
    /// Returns the SHA-256 of the binary in the container.
    pub fn start(&mut self, loadgen: &Path) -> Result<String, String> {
        let network = self.name("net");
        docker(["network", "create", "--label", &self.label(), &network])?;
        self.network = Some(network);
        let client = self.name("loadgen");
        self.container(
            &["run", "--detach"],
            &client,
            self.rig.loadgen,
            &[
                CLIENT_IMAGE.to_owned(),
                "sleep".to_owned(),
                "infinity".to_owned(),
            ],
        )?;
        self.client = Some(client.clone());
        self.copy_in(loadgen, &format!("{client}:{LOADGEN_PATH}"))?;
        let sum = docker(["exec", &client, "sha256sum", LOADGEN_PATH])?;
        Ok(sum.split_whitespace().next().unwrap_or_default().to_owned())
    }

    /// Runs the fsync probe of `loadgen` on a fresh WAL volume of this session, with Crucible's CPUs and
    /// memory, and returns its JSON report.
    pub fn probe(&self, loadgen: &Path) -> Result<String, String> {
        let volume = self.fresh_volume()?;
        let name = self.name("probe");
        self.container(
            &["create"],
            &name,
            self.rig.crucible,
            &[
                "--volume".to_owned(),
                format!("{volume}:/probe"),
                CLIENT_IMAGE.to_owned(),
                LOADGEN_PATH.to_owned(),
                "fsync-probe".to_owned(),
                "--dir".to_owned(),
                "/probe".to_owned(),
            ],
        )?;
        let report = self
            .copy_in(loadgen, &format!("{name}:{LOADGEN_PATH}"))
            .and_then(|()| {
                capture_within(
                    Command::new("docker").args(["start", "--attach", &name]),
                    Duration::from_secs(600),
                )
            });
        let _ = docker(["rm", "--force", "--volumes", &name]);
        let _ = docker(["volume", "rm", "--force", &volume]);
        report
    }

    /// Whether rig container `name` (role `role`) still runs; `Some(reason)` when it does not.
    fn stopped(name: &str, role: &str) -> Option<String> {
        let state = docker([
            "inspect",
            "--format",
            "{{.State.Running}} {{.State.OOMKilled}} {{.State.ExitCode}}",
            name,
        ]);
        match state
            .as_deref()
            .map(str::split_whitespace)
            .map(Iterator::collect::<Vec<_>>)
        {
            Ok(parts) => match parts[..] {
                [_, "true", code] => Some(format!("{role} (OOM killed, exit code {code})")),
                ["true", _, _] => None,
                [_, _, code] => Some(format!("{role} (exit code {code})")),
                _ => Some(format!("{role} (unexpected state)")),
            },
            Err(e) => Some(format!("{role} ({e})")),
        }
    }

    /// Saves container `name`'s output to `path` and returns its last 20 lines.
    fn save_log(name: &str, path: &Path) -> String {
        let log = Command::new("docker")
            .args(["logs", name])
            .stdin(Stdio::null())
            .output()
            .map(|out| {
                String::from_utf8_lossy(&out.stdout).into_owned()
                    + &String::from_utf8_lossy(&out.stderr)
            })
            .unwrap_or_else(|e| format!("docker logs {name}: {e}"));
        let _ = fs::write(path, &log);
        let lines: Vec<&str> = log.lines().collect();
        lines[lines.len().saturating_sub(20)..].join("\n")
    }

    /// Measures one arm on a fresh rig: ClickHouse and Postgres, then Crucible from `image` on a fresh WAL
    /// volume; a warm-up at the latency rate; then the measurement of `kind`. Writes the load generator's
    /// reports to `dir/runs/<tag>.json` and `<tag>-warmup.json`, and Crucible's log to `dir/logs/<tag>.log`,
    /// then removes the rig.
    pub fn arm(
        &self,
        image: &str,
        lane: Lane,
        kind: Kind,
        dir: &Path,
        tag: &str,
    ) -> Result<ArmOutput, String> {
        let result = self.measure(image, lane, kind, dir, tag);
        self.down();
        result
    }

    /// Removes the rig of the arm in progress: Crucible, ClickHouse and Postgres with their volumes.
    pub fn down(&self) {
        for role in ["crucible", "clickhouse", "postgres"] {
            let _ = docker(["rm", "--force", "--volumes", &self.name(role)]);
        }
        let _ = docker(["volume", "rm", "--force", &self.name("wal")]);
    }

    /// The container name of the rig's Crucible, which is also its host name on the session's network.
    pub fn crucible(&self) -> String {
        self.name("crucible")
    }

    /// The load generator's container.
    pub fn client(&self) -> Result<&str, String> {
        self.client
            .as_deref()
            .ok_or_else(|| "the session has not started".to_owned())
    }

    /// Starts a fresh rig for one arm: ClickHouse and Postgres, then Crucible from `image` (`serve --migrate`)
    /// on a fresh WAL volume, with its metrics on port 9464; then makes the arm's project key. On failure the
    /// end of Crucible's log is saved to `dir/logs/<tag>.log` and returned.
    pub fn up(&self, image: &str, dir: &Path, tag: &str) -> Result<ArmKey, String> {
        let client = self.client()?;
        let (clickhouse, postgres, crucible) = (
            self.name("clickhouse"),
            self.name("postgres"),
            self.name("crucible"),
        );
        let env = |pairs: &[(&str, &str)]| -> Vec<String> {
            pairs
                .iter()
                .flat_map(|(key, value)| ["--env".to_owned(), format!("{key}={value}")])
                .collect()
        };
        let secrets = &self.secrets;
        let mut rest = vec!["--ulimit".to_owned(), "nofile=262144:262144".to_owned()];
        rest.extend(env(&[
            ("CLICKHOUSE_USER", "crucible_admin"),
            ("CLICKHOUSE_PASSWORD", secrets.clickhouse.as_str()),
            ("CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT", "1"),
        ]));
        rest.push(CLICKHOUSE_IMAGE.to_owned());
        self.container(
            &["run", "--detach"],
            &clickhouse,
            self.rig.clickhouse,
            &rest,
        )?;
        let mut rest = env(&[
            ("POSTGRES_USER", "crucible"),
            ("POSTGRES_PASSWORD", secrets.postgres.as_str()),
            ("POSTGRES_DB", "crucible"),
        ]);
        rest.push(POSTGRES_IMAGE.to_owned());
        self.container(&["run", "--detach"], &postgres, self.rig.postgres, &rest)?;
        let ping = "http://127.0.0.1:8123/ping";
        wait("ClickHouse", Duration::from_secs(120), || {
            docker([
                "exec",
                &clickhouse,
                "wget",
                "--quiet",
                "--output-document=/dev/null",
                ping,
            ])
            .is_ok()
        })?;
        // Over TCP: while it initializes a new cluster, Postgres listens on its Unix socket only.
        wait("Postgres", Duration::from_secs(120), || {
            docker([
                "exec",
                &postgres,
                "pg_isready",
                "--host=127.0.0.1",
                "--username=crucible",
                "--dbname=crucible",
            ])
            .is_ok()
        })?;

        let volume = self.fresh_volume()?;
        let mut rest = vec!["--volume".to_owned(), format!("{volume}:/var/lib/crucible")];
        let clickhouse_url = format!("http://{clickhouse}:8123");
        let postgres_url = format!("postgres://crucible@{postgres}:5432/crucible");
        rest.extend(env(&[
            ("CRUCIBLE_CLICKHOUSE_URL", clickhouse_url.as_str()),
            ("CRUCIBLE_CLICKHOUSE_PASSWORD", secrets.clickhouse.as_str()),
            ("CRUCIBLE_POSTGRES_URL", postgres_url.as_str()),
            ("CRUCIBLE_POSTGRES_PASSWORD", secrets.postgres.as_str()),
            ("CRUCIBLE_MASTER_KEY", secrets.master_key.as_str()),
        ]));
        rest.extend([
            image.to_owned(),
            "serve".to_owned(),
            "--migrate".to_owned(),
            "--data-dir=/var/lib/crucible/data".to_owned(),
            format!("--metrics-listen=0.0.0.0:{METRICS_PORT}"),
            format!("--recent-max-bytes={}", self.rig.recent_max_bytes),
        ]);
        self.container(&["create"], &crucible, self.rig.crucible, &rest)?;
        self.copy_in(&self.config, &format!("{crucible}:{CONFIG_DIR}"))?;
        docker(["start", &crucible])?;
        let log = dir.join("logs").join(format!("{tag}.log"));
        let readyz = format!("http://{crucible}:8080/readyz");
        let mut exited = None;
        let ready = wait("crucible", Duration::from_secs(180), || {
            exited = Self::stopped(&crucible, "crucible");
            exited.is_some()
                || docker([
                    "exec",
                    client,
                    "curl",
                    "--silent",
                    "--fail",
                    "--output",
                    "/dev/null",
                    &readyz,
                ])
                .is_ok()
        });
        let key = ready
            .and_then(|()| exited.map_or(Ok(()), |stopped| Err(format!("stopped: {stopped}"))))
            .and_then(|()| self.provision(&crucible, client));
        let key = match key {
            Ok(key) => key,
            Err(e) => {
                let tail = Self::save_log(&crucible, &log);
                return Err(format!(
                    "crucible from {image}: {e}; the end of its log:\n{tail}"
                ));
            }
        };
        Ok(key)
    }

    fn measure(
        &self,
        image: &str,
        lane: Lane,
        kind: Kind,
        dir: &Path,
        tag: &str,
    ) -> Result<ArmOutput, String> {
        let key = self.up(image, dir, tag)?;
        let (clickhouse, postgres, crucible) = (
            self.name("clickhouse"),
            self.name("postgres"),
            self.name("crucible"),
        );
        let log = dir.join("logs").join(format!("{tag}.log"));
        let target = format!("{crucible}:4318");
        let warmup = self.loadgen(&target, &key, lane, self.rig.latency_rps, self.rig.warmup_s)?;
        let runs = dir.join("runs");
        fs::write(runs.join(format!("{tag}-warmup.json")), &warmup)
            .map_err(|e| format!("{}: {e}", runs.display()))?;
        // An end-to-end measurement starts with the warm-up in the store, so no backlog carries into it.
        let warmed = if kind.end_to_end() {
            let acked = RunFields::parse(&warmup)?.outcome.acked_spans;
            if !self.drained(&key, acked, Duration::from_secs(self.rig.drain_s))? {
                return Err(format!(
                    "the store did not take the warm-up's {acked} spans within {} s",
                    self.rig.drain_s
                ));
            }
            acked
        } else {
            0
        };
        let (rps, secs) = self.rig.schedule(kind);
        let (report, context) = self.observed(|| self.loadgen(&target, &key, lane, rps, secs))?;
        let stored = match &report {
            Ok(report) if kind.end_to_end() => Some(self.check_store(&key, report, warmed)),
            _ => None,
        };
        let stopped: Vec<String> = [
            (&crucible, "crucible"),
            (&clickhouse, "clickhouse"),
            (&postgres, "postgres"),
        ]
        .into_iter()
        .filter_map(|(name, role)| Self::stopped(name, role))
        .collect();
        let tail = Self::save_log(&crucible, &log);
        let report = report.map_err(|e| format!("{e}; the end of crucible's log:\n{tail}"))?;
        fs::write(runs.join(format!("{tag}.json")), &report)
            .map_err(|e| format!("{}: {e}", runs.display()))?;
        Ok(ArmOutput {
            report,
            offered_spans_per_s: (rps * u64::from(lane.spans_per_request())) as f64,
            stopped,
            context,
            stored: stored.transpose()?,
        })
    }

    /// Runs `sql` with `clickhouse-client` in the rig's ClickHouse container, as the admin user, and returns
    /// what it printed.
    fn clickhouse_text(&self, sql: &str) -> Result<String, String> {
        docker([
            "exec",
            &self.name("clickhouse"),
            "clickhouse-client",
            "--user",
            "crucible_admin",
            "--password",
            &self.secrets.clickhouse,
            "--query",
            sql,
        ])
    }

    /// The rows of a `SELECT` as JSON objects.
    fn clickhouse(&self, sql: &str) -> Result<Vec<serde_json::Value>, String> {
        self.clickhouse_text(&format!("{sql} FORMAT JSONEachRow"))?
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str(line).map_err(|e| format!("ClickHouse answered `{line}`: {e}"))
            })
            .collect()
    }

    /// The first row's numeric field `field` of a `SELECT`, or 0.
    fn clickhouse_number(&self, sql: &str, field: &str) -> Result<f64, String> {
        Ok(self
            .clickhouse(sql)?
            .first()
            .and_then(|row| number(&row[field]))
            .unwrap_or(0.0))
    }

    /// Waits until `span_core` holds at least `rows` rows of the key's project, polling twice a second for at
    /// most `limit`; whether it did.
    fn drained(&self, key: &ArmKey, rows: u64, limit: Duration) -> Result<bool, String> {
        let sql = format!(
            "SELECT count() AS n FROM crucible.span_core WHERE project_id = toUUID('{}')",
            key.project_id
        );
        let started = Instant::now();
        loop {
            if self.clickhouse_number(&sql, "n")? >= rows as f64 {
                return Ok(true);
            }
            if started.elapsed() > limit {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Waits for the drain of the run of `report`, after `before` spans of the warm-up, then reads from the store
    /// when it ended, the freshness of the run's spans, and which acknowledged spans are missing or stored
    /// twice (invariant I1).
    fn check_store(&self, key: &ArmKey, report: &str, before: u64) -> Result<Stored, String> {
        let run = RunFields::parse(report)?;
        let acked = run.outcome.acked_spans;
        let drained = self.drained(key, before + acked, Duration::from_secs(self.rig.drain_s))?;
        self.clickhouse_text("SYSTEM FLUSH LOGS")?;
        let project = &key.project_id;
        let start = format!("parseDateTime64BestEffort('{}', 6, 'UTC')", run.started_at);
        let drained_s = if drained {
            Some(self.clickhouse_number(
                &format!(
                    "SELECT dateDiff('microsecond', {start}, max(event_time_microseconds)) / 1e6 AS s \
                     FROM system.query_log WHERE type = 'QueryFinish' AND query_kind = 'Insert' \
                     AND event_date >= yesterday() AND has(tables, 'crucible.span_core') \
                     AND event_time_microseconds >= {start}"
                ),
                "s",
            )?)
        } else {
            None
        };
        let spans = run.lane.spans_per_request;
        let not_acked: Vec<String> = run
            .not_acked_requests
            .iter()
            .map(|[from, to]| format!("(seq >= {} AND seq < {})", from * spans, to * spans))
            .collect();
        let acked_seq = if not_acked.is_empty() {
            "1".to_owned()
        } else {
            format!("NOT ({})", not_acked.join(" OR "))
        };
        let payload = self.clickhouse(&format!(
            "SELECT count() AS rows, uniqExact(seq) AS distinct_seqs, uniqExactIf(seq, {acked_seq}) AS acked \
             FROM (SELECT toUInt64OrZero(JSONExtractString(attributes, 'loadgen.seq')) AS seq \
             FROM crucible.span_payload WHERE project_id = toUUID('{project}') \
             AND JSONExtractString(attributes, 'loadgen.run_id') = '{}')",
            run.run_id
        ))?;
        let field = |name: &str| {
            payload
                .first()
                .and_then(|row| number(&row[name]))
                .unwrap_or(0.0) as u64
        };
        let core_duplicates = self.clickhouse_number(
            &format!(
                "SELECT count() - uniqExact(trace_id, span_id) AS d FROM crucible.span_core \
                 WHERE project_id = toUUID('{project}')"
            ),
            "d",
        )? as u64;
        let fresh = self.clickhouse(&format!(
            "SELECT quantileExact(0.95)(f) AS p95, count() AS spans FROM ( \
               SELECT dateDiff('microsecond', s.received_at, i.visible_at) AS f \
               FROM crucible.span_core AS s INNER JOIN ( \
                 SELECT toUUID(t[1]) AS epoch, toUInt64(splitByChar('-', t[2])[1]) AS seq, \
                        min(event_time_microseconds) AS visible_at \
                 FROM (SELECT splitByChar(':', Settings['insert_deduplication_token']) AS t, \
                              event_time_microseconds \
                       FROM system.query_log WHERE type = 'QueryFinish' AND query_kind = 'Insert' \
                       AND event_date >= yesterday() AND Settings['insert_deduplication_token'] != '') \
                 WHERE t[3] = '1' GROUP BY epoch, seq \
               ) AS i ON s.wal_epoch = i.epoch AND s.batch_seq = i.seq \
               WHERE s.project_id = toUUID('{project}') AND s.received_at >= {start})"
        ))?;
        let fresh_field = |name: &str| {
            fresh
                .first()
                .and_then(|row| number(&row[name]))
                .unwrap_or(0.0)
        };
        Ok(Stored {
            acked_spans: acked,
            drained_s,
            drain_after_run_s: drained_s.map(|s| s - run.outcome.elapsed_s),
            shipped_spans_per_s: drained_s
                .filter(|s| *s > 0.0)
                .map_or(0.0, |s| acked as f64 / s),
            freshness_p95_us: fresh_field("p95").max(0.0) as u64,
            freshness_spans: fresh_field("spans") as u64,
            lost_spans: acked.saturating_sub(field("acked")),
            duplicate_spans: field("rows").saturating_sub(field("distinct_seqs")) + core_duplicates,
        })
    }

    /// The host's CPU ticks and 1-minute load average, read in the load generator's container: neither file
    /// is namespaced, so the container sees the Docker host's. Empty when the read fails.
    /// Runs `measurement` and records what else the Docker host did meanwhile.
    pub fn observed<T>(&self, measurement: impl FnOnce() -> T) -> Result<(T, Context), String> {
        let client = self.client()?;
        let others = self.others();
        let before = self.host_sample(client);
        let started_at = super::now().0;
        let value = measurement();
        let finished_at = super::now().0;
        let after = self.host_sample(client);
        let (rig_cpus_busy, other_cpus_busy) =
            busy_cores(&before.cpus, &after.cpus, &self.rig.cpus());
        let context = Context {
            started_at,
            finished_at,
            rig_cpus_busy,
            other_cpus_busy,
            load_before: before.load,
            load_after: after.load,
            disk_busy: disk_busy(
                &before.disks,
                &after.disks,
                after.at.saturating_duration_since(before.at),
            ),
            page_cache_drops: after
                .page_cache_drops
                .saturating_sub(before.page_cache_drops),
            others,
        };
        Ok((value, context))
    }

    fn host_sample(&self, client: &str) -> HostSample {
        let script = "cat /proc/stat; echo ---; cat /proc/loadavg; echo ---; cat /proc/diskstats; echo ---; \
                      cat /proc/vmstat";
        let text = docker(["exec", client, "sh", "-c", script]).unwrap_or_default();
        let mut parts = text.split("---\n");
        let (stat, loadavg, diskstats, vmstat) = (
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
        );
        HostSample {
            cpus: cpu_ticks(stat),
            load: load_1m(loadavg),
            disks: disk_ticks(diskstats),
            page_cache_drops: page_cache_drops(vmstat),
            at: Instant::now(),
        }
    }

    /// The names of the running containers that are not this session's.
    fn others(&self) -> Vec<String> {
        let mine = self.name("");
        docker(["ps", "--format", "{{.Names}}"])
            .map(|names| {
                names
                    .lines()
                    .filter(|name| !name.is_empty() && !name.starts_with(&mine))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Runs `crucible-loadgen <args>` in the load generator's container and returns what it printed, stopping it
    /// after `deadline`.
    pub fn loadgen_exec(&self, args: &[String], deadline: Duration) -> Result<String, String> {
        let client = self.client()?;
        let mut command = Command::new("docker");
        command.args(["exec", client, LOADGEN_PATH]).args(args);
        capture_within(&mut command, deadline)
    }

    /// Runs `args` in the load generator's container and returns what it printed.
    pub fn client_exec(&self, args: &[&str]) -> Result<String, String> {
        let client = self.client()?;
        docker(["exec", client].iter().chain(args))
    }

    /// Crucible's metrics, in the Prometheus text format.
    pub fn metrics(&self) -> Result<String, String> {
        let url = format!("http://{}:{METRICS_PORT}/metrics", self.crucible());
        self.client_exec(&["curl", "--silent", "--fail", &url])
    }

    /// Waits until `span_core` holds at least `rows` spans of the key's project; whether it did within `limit`.
    pub fn wait_for_store(&self, key: &ArmKey, rows: u64, limit: Duration) -> Result<bool, String> {
        self.drained(key, rows, limit)
    }

    /// Waits until reads of `span_core` and `trace_heads` load at most [`SETTLED_STATISTICS_US`] of column
    /// statistics each, or until `limit` passes. ClickHouse (26.8, automatic statistics on every column) reuses
    /// its snapshot of a table's statistics, refreshed every `refresh_statistics_interval` (300 s by default),
    /// only while the table's parts are the ones the snapshot saw; after an insert or a merge every read with more
    /// than one condition loads every part's statistics until the next refresh. The probe has the shape of trace
    /// by id's span read and carries none of Crucible's settings.
    pub fn settle_statistics(&self, key: &ArmKey, limit: Duration) -> Result<Settle, String> {
        let started = Instant::now();
        let stamp = super::now().1;
        let mut probe = 0u64;
        loop {
            probe += 1;
            let comment = format!("crucible-perf-settle-{}-{stamp}-{probe}", self.id);
            for table in ["span_core", "trace_heads"] {
                self.clickhouse_text(&format!(
                    "SELECT count() FROM crucible.{table} WHERE project_id = toUUID('{}') AND trace_hash = 0 AND \
                     trace_id = toFixedString('', 16) SETTINGS log_comment = '{comment}'",
                    key.project_id
                ))?;
            }
            self.clickhouse_text("SYSTEM FLUSH LOGS")?;
            let row = self.clickhouse_row(&format!(
                "SELECT count() AS reads, sum(ProfileEvents['LoadedStatisticsMicroseconds']) AS us FROM \
                 system.query_log WHERE type = 'QueryFinish' AND log_comment = '{comment}'"
            ))?;
            let number = |field: &str| {
                row[field]
                    .as_u64()
                    .or_else(|| row[field].as_str().and_then(|s| s.parse().ok()))
            };
            let load_us = number("us").unwrap_or(u64::MAX) / 2;
            let settled = number("reads") == Some(2) && load_us <= SETTLED_STATISTICS_US;
            if settled || started.elapsed() >= limit {
                return Ok(Settle {
                    waited: started.elapsed(),
                    settled,
                    load_us,
                });
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    }

    /// ClickHouse's clock, as `system.query_log` stamps its entries.
    pub fn clickhouse_now(&self) -> Result<String, String> {
        Ok(self
            .clickhouse_text("SELECT toString(now64(6))")?
            .trim()
            .to_owned())
    }

    /// The column statistics that Crucible's own reads loaded between two instants of [`Session::clickhouse_now`],
    /// from `system.query_log`: per set of tables, the reads that started in that time and the
    /// `LoadedStatisticsMicroseconds` they spent. Crucible's reads carry its query settings, which the probe of
    /// [`Session::settle_statistics`] does not.
    pub fn read_statistics(&self, from: &str, to: &str) -> Result<Vec<StatisticsLoad>, String> {
        self.clickhouse_text("SYSTEM FLUSH LOGS")?;
        let rows = self.clickhouse(&format!(
            "SELECT arrayStringConcat(tables, ', ') AS tables, count() AS reads, \
             sum(ProfileEvents['LoadedStatisticsMicroseconds']) AS load_us FROM system.query_log \
             WHERE type = 'QueryFinish' AND event_date >= yesterday() AND user IN ({READ_USERS}) \
             AND query_start_time_microseconds >= toDateTime64('{from}', 6, 'UTC') \
             AND query_start_time_microseconds <= toDateTime64('{to}', 6, 'UTC') \
             GROUP BY tables ORDER BY tables"
        ))?;
        Ok(rows
            .iter()
            .map(|row| StatisticsLoad {
                tables: row["tables"].as_str().unwrap_or_default().to_owned(),
                reads: number(&row["reads"]).unwrap_or(0.0) as u64,
                load_us: number(&row["load_us"]).unwrap_or(0.0) as u64,
            })
            .collect())
    }

    /// Prepares the store for a measurement as on a server that has served reads for a while: loads the marks of
    /// every table of the `crucible` database into ClickHouse's mark cache, since a part's marks otherwise load
    /// on its first read, and with `files` also reads every file of the store once, in its container, so that
    /// the page cache holds the store. The container's page cache counts against its memory limit; a part that
    /// a merge removes meanwhile is skipped.
    pub fn warm_store(&self, files: bool) -> Result<(), String> {
        let tables = self.clickhouse(
            "SELECT name FROM system.tables WHERE database = 'crucible' AND engine LIKE '%MergeTree'",
        )?;
        for table in tables {
            if let Some(name) = table["name"].as_str() {
                self.clickhouse_text(&format!("SYSTEM PREWARM MARK CACHE crucible.`{name}`"))?;
            }
        }
        if files {
            docker([
                "exec",
                &self.name("clickhouse"),
                "sh",
                "-c",
                "find /var/lib/clickhouse/store -type f -exec cat {} + > /dev/null 2>&1; true",
            ])?;
        }
        Ok(())
    }

    /// The first row of a ClickHouse `SELECT` as JSON, for the record of a run.
    pub fn clickhouse_row(&self, sql: &str) -> Result<serde_json::Value, String> {
        Ok(self
            .clickhouse(sql)?
            .into_iter()
            .next()
            .unwrap_or(serde_json::Value::Null))
    }

    /// Makes the arm's project key once Crucible is ready: `crucible admin init` creates the first owner and the
    /// project `default`, `crucible admin create-key` a key for it with the ingest and read scopes, and the arm
    /// waits until the server accepts the key (its snapshot polls Postgres every few seconds). The image has no
    /// shell, so each command runs the binary itself.
    fn provision(&self, crucible: &str, client: &str) -> Result<ArmKey, String> {
        let password = format!("--password-file={CONFIG_DIR}/owner-password");
        docker([
            "exec",
            crucible,
            CRUCIBLE_PATH,
            "admin",
            "init",
            "--org-name=Perf gate",
            "--email=owner@perf-gate.test",
            "--name=Perf gate",
            &password,
        ])
        .map_err(|e| format!("crucible admin init: {e}"))?;
        let printed = docker([
            "exec",
            crucible,
            CRUCIBLE_PATH,
            "admin",
            "create-key",
            "--project=default",
            "--scopes=ingest,read",
            "--note=perf gate",
        ])
        .map_err(|e| format!("crucible admin create-key: {e}"))?;
        let key = ArmKey::parse(&printed)?;
        let header = format!("Authorization: Bearer {}", key.secret);
        let url = format!(
            "http://{crucible}:8080/api/v1/projects/{}/data-bounds",
            key.project_id
        );
        wait("the project key", Duration::from_secs(60), || {
            docker([
                "exec",
                client,
                "curl",
                "--silent",
                "--fail",
                "--output",
                "/dev/null",
                "--header",
                &header,
                &url,
            ])
            .is_ok()
        })?;
        Ok(key)
    }

    /// One load generator run against `target`; an overloaded run lasts until every request is sent, so the
    /// deadline allows twenty times the schedule.
    fn loadgen(
        &self,
        target: &str,
        key: &ArmKey,
        lane: Lane,
        rps: u64,
        secs: u64,
    ) -> Result<String, String> {
        let client = self
            .client
            .as_deref()
            .ok_or("the session has not started")?;
        let header = format!("Authorization: Bearer {}", key.secret);
        let (rate, duration) = (rps.to_string(), secs.to_string());
        let (spans, connections) = (
            lane.spans_per_request().to_string(),
            CONCURRENCY.to_string(),
        );
        let mut command = Command::new("docker");
        command.args([
            "exec",
            client,
            LOADGEN_PATH,
            "run",
            "--target",
            target,
            "--rate",
            &rate,
            "--duration-secs",
            &duration,
            "--span-size",
            lane.span_size(),
            "--spans-per-request",
            &spans,
            "--concurrency",
            &connections,
            "--header",
            &header,
        ]);
        // The generator compresses on half its CPUs and runs its workers on the rest.
        if lane.gzip() {
            command.arg("--gzip");
        }
        capture_within(&mut command, Duration::from_secs(120 + 20 * secs))
    }
}

impl Drop for Session<'_> {
    /// Removes every container and volume of the session, then its network and local files.
    fn drop(&mut self) {
        let filter = format!("label={}", self.label());
        for (list, remove) in [
            (
                vec!["ps", "--all", "--quiet", "--filter", &filter],
                vec!["rm", "--force", "--volumes"],
            ),
            (
                vec!["volume", "ls", "--quiet", "--filter", &filter],
                vec!["volume", "rm", "--force"],
            ),
        ] {
            if let Ok(ids) = docker(&list) {
                let ids: Vec<&str> = ids.split_whitespace().collect();
                if !ids.is_empty() {
                    let _ = docker(remove.iter().chain(&ids));
                }
            }
        }
        if let Some(network) = &self.network {
            let _ = docker(["network", "rm", network]);
        }
        if let Some(session) = self.config.parent() {
            let _ = fs::remove_dir_all(session);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rigs_fit_their_documented_sizes() {
        assert_eq!(FULL.crucible.cpus(), 6);
        assert_eq!(FULL.clickhouse.cpus(), 10);
        assert_eq!(FULL.postgres.cpus(), 1);
        assert_eq!(FULL.loadgen.cpus(), 6);
        assert!(check_cpus(&FULL, 23).is_ok());
        let refused = check_cpus(&FULL, 16).unwrap_err();
        assert!(refused.contains("pins CPUs 0 to 22") && refused.contains("--rig small"));
        assert!(check_cpus(&SMALL, 7).is_ok() && check_cpus(&SMALL, 6).is_err());
        // The small rig of the track: Crucible 1 GB, ClickHouse 1.5 GB, Postgres 512 MB, load generator 512 MB.
        let memory: Vec<&str> = SMALL.containers().iter().map(|(_, l)| l.memory).collect();
        assert_eq!(memory, ["1g", "1536m", "512m", "512m"]);
        // Disjoint cpusets.
        for rig in [&FULL, &SMALL] {
            let mut cpus: Vec<usize> = rig
                .containers()
                .iter()
                .flat_map(|(_, l)| l.range().0..=l.range().1)
                .collect();
            let all = cpus.len();
            cpus.sort_unstable();
            cpus.dedup();
            assert_eq!(cpus.len(), all, "{} rig cpusets overlap", rig.name);
        }
    }

    #[test]
    fn busy_cores_split_the_rigs_cpus_from_the_others() {
        // The aggregate line and the other lines of /proc/stat are not CPUs.
        let before = cpu_ticks(
            "cpu  10 0 10 280 0 0 0 0 0 0\ncpu0 0 0 0 100 0 0 0 0 0 0\ncpu1 0 0 0 100 0 0 0 0 0 0\n\
             cpu2 0 0 0 100 0 0 0 0 0 0\nintr 1 2 3\nctxt 4\n",
        );
        assert_eq!(before.len(), 3);
        // cpu0 is fully busy, cpu1 a quarter busy (iowait counts as idle), cpu2 fully busy.
        let after = cpu_ticks(
            "cpu0 50 0 50 100 0 0 0 0 0 0\ncpu1 0 0 25 150 25 0 0 0 0 0\ncpu2 90 0 0 100 0 0 10 0 0 0\n",
        );
        let (inside, outside) = busy_cores(&before, &after, &[0, 1]);
        assert!((inside - 1.25).abs() < 1e-9, "{inside}");
        assert!((outside - 1.0).abs() < 1e-9, "{outside}");
        // A CPU missing from either reading, or with no elapsed ticks, counts nowhere.
        assert_eq!(busy_cores(&BTreeMap::new(), &after, &[0]), (0.0, 0.0));
        assert_eq!(busy_cores(&after, &after, &[0]), (0.0, 0.0));
        assert!((load_1m("0.52 0.58 0.59 1/467 12345\n") - 0.52).abs() < 1e-9);
        assert_eq!(load_1m(""), 0.0);
        // The busiest device's share of the interval with I/O in flight; a device in one reading only counts not.
        let disks = |text: &str| disk_ticks(text);
        let before = disks(
            "   8       0 sda 1559 0 0 0 0 0 0 0 0 100 0\n   8      32 sdc 3444480 0 0 0 0 0 0 0 0 2000 0\n",
        );
        let after = disks(
            "   8       0 sda 1559 0 0 0 0 0 0 0 0 150 0\n   8      32 sdc 3444480 0 0 0 0 0 0 0 0 2750 0\n\
             259       0 nvme0n1 1 0 0 0 0 0 0 0 0 999 0\n",
        );
        assert_eq!(after.len(), 3);
        let busy = disk_busy(&before, &after, Duration::from_secs(1));
        assert!((busy - 0.75).abs() < 1e-9, "{busy}");
        assert_eq!(disk_busy(&before, &after, Duration::ZERO), 0.0);
        assert_eq!(disk_busy(&before, &after, Duration::from_millis(500)), 1.0);
        assert_eq!(SMALL.cpus(), [0, 1, 2, 3, 4, 5, 6]);
        assert_eq!(FULL.cpus().len(), 23);
    }

    #[test]
    fn page_cache_drops_are_read_from_vmstat() {
        let vmstat = "nr_free_pages 1000\ndrop_pagecache 8\ndrop_slab 8\npgfault 99\n";
        assert_eq!(page_cache_drops(vmstat), 8);
        assert_eq!(page_cache_drops("nr_free_pages 1000\n"), 0);
        assert_eq!(page_cache_drops(""), 0);
    }

    #[test]
    fn memory_limits_are_read_as_docker_takes_them() {
        let limits = |memory| Limits {
            cpuset: "0",
            memory,
        };
        assert_eq!(limits("1536m").memory_bytes(), 1_610_612_736);
        assert_eq!(limits("9g").memory_bytes(), 9_663_676_416);
        assert_eq!(limits("512k").memory_bytes(), 524_288);
        assert_eq!(limits("4096").memory_bytes(), 4_096);
        assert_eq!(limits("x").memory_bytes(), 0);
        assert_eq!(SMALL.clickhouse.memory_bytes(), 1_610_612_736);
    }

    #[test]
    fn lanes_and_paths() {
        for lane in [Lane::Kib1, Lane::Kib10, Lane::Kib1Gzip, Lane::Kib10Gzip] {
            assert_eq!(Lane::parse(lane.name()), Some(lane));
        }
        assert_eq!(Lane::parse("1k"), None);
        assert_eq!(
            (Lane::Kib10.span_size(), Lane::Kib10.spans_per_request()),
            ("10k", 10)
        );
        assert_eq!(
            (
                Lane::Kib10Gzip.span_size(),
                Lane::Kib10Gzip.spans_per_request(),
                Lane::Kib10Gzip.gzip()
            ),
            ("10k", 10, true)
        );
        assert!(!Lane::Kib1.gzip() && Lane::Kib1Gzip.gzip());
        assert_eq!(GatePath::parse("ack"), Some(GatePath::Ack));
        assert_eq!(GatePath::parse("e2e"), Some(GatePath::E2e));
        assert_eq!(GatePath::parse("store"), None);
        assert_eq!(GatePath::E2e.kinds(), [Kind::Sustained, Kind::Freshness]);
        assert!(GatePath::E2e.kinds().iter().all(|kind| kind.end_to_end()));
        assert!(!GatePath::Ack.kinds().iter().any(|kind| kind.end_to_end()));
        // Bytes per request are the same in both span sizes, so one schedule serves both lanes.
        for rig in [&FULL, &SMALL] {
            assert!(rig.sustained_rps > rig.freshness_rps);
            assert_eq!(
                rig.schedule(Kind::Sustained),
                (rig.sustained_rps, rig.sustained_s)
            );
        }
    }

    #[test]
    fn load_generator_reports_give_the_end_to_end_fields() {
        let report = r#"{"schema":"crucible-loadgen.run.v1","run_id":"019a6b2e-7c3d-7e4f-8a5b-6c7d8e9f0a1b",
            "started_at":"2026-11-18T21:30:00.123456Z","lane":{"spans_per_request":100},
            "outcome":{"acked_spans":900,"elapsed_s":1.5},"not_acked_requests":[[3,4]]}"#;
        let fields = RunFields::parse(report).unwrap();
        assert_eq!(
            (fields.outcome.acked_spans, fields.not_acked_requests),
            (900, vec![[3, 4]])
        );
        // Text that reaches SQL holds only the characters of a UUID and an RFC 3339 time.
        let injected = report.replace("019a6b2e", "x' OR 1=1");
        assert!(RunFields::parse(&injected).is_err());
        assert_eq!(number(&serde_json::json!("12")), Some(12.0));
        assert_eq!(number(&serde_json::json!(1.5)), Some(1.5));
        assert_eq!(number(&serde_json::json!(null)), None);
    }

    #[test]
    fn secrets_have_the_documented_shapes() {
        let secrets = Secrets::new().unwrap();
        assert_eq!(secrets.master_key.len(), 64);
        // `crucible admin init` takes a password of 12 to 128 characters.
        assert_eq!(secrets.owner_password.len(), 32);
        assert_eq!(format!("{secrets:?}"), "Secrets(redacted)");
    }

    #[test]
    fn project_keys_are_read_from_create_key_and_checked() {
        let printed = "project_id = \"019a6b2e-7c3d-7e4f-8a5b-6c7d8e9f0a1b\"\npublic_key = \"cpk_0abc\"\n\
                       secret_key = \"csk_0Abc123\"\n";
        let key = ArmKey::parse(printed).unwrap();
        assert_eq!(
            (key.project_id.as_str(), key.secret.as_str()),
            ("019a6b2e-7c3d-7e4f-8a5b-6c7d8e9f0a1b", "csk_0Abc123")
        );
        // The secret never reaches a debug print.
        assert!(!format!("{key:?}").contains("csk_"));
        // The project id goes into SQL and the secret into a header: anything else is refused.
        for bad in [
            printed.replace("019a6b2e", "x' OR 1"),
            printed.replace("csk_0Abc123", "csk_0Abc 123"),
            printed.replace("csk_", "cpk_"),
            "the server already has an owner".to_owned(),
        ] {
            assert!(ArmKey::parse(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn ac_power_from_sysfs() {
        let root = std::env::temp_dir().join(format!("xtask-power-{}", std::process::id()));
        let supply = |name: &str, kind: &str, online: Option<&str>| {
            let dir = root.join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("type"), format!("{kind}\n")).unwrap();
            if let Some(online) = online {
                fs::write(dir.join("online"), format!("{online}\n")).unwrap();
            }
        };
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        assert_eq!(sysfs_power(&root).state, "unknown");
        supply("BAT0", "Battery", None);
        assert_eq!(sysfs_power(&root).state, "unknown");
        supply("AC", "Mains", Some("0"));
        assert_eq!(sysfs_power(&root).state, "battery");
        supply("USBC", "Mains", Some("1"));
        assert_eq!(sysfs_power(&root).state, "online");
        assert_eq!(sysfs_power(&root.join("missing")).state, "unknown");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn failures_never_echo_secret_arguments() {
        let mut command = Command::new("docker-that-does-not-exist");
        command.args(["exec", "--env", "PASSWORD=hunter2"]);
        let error = capture(&mut command).unwrap_err();
        assert!(
            error.starts_with("docker-that-does-not-exist exec:"),
            "{error}"
        );
        assert!(!error.contains("hunter2"));
    }
}
