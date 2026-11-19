//! The rig of the A/B gate: the two images under test, the load generator binary, and the Docker containers
//! of a session. Everything reaches Docker through the `docker` CLI and moves files with `docker cp` and
//! stdout, never with bind mounts, so the gate also works from a container that talks to the host's Docker
//! socket (the daemon resolves bind paths on its own filesystem). Every container, volume and network of a
//! session carries the label `crucible.perf-gate=<session>` and is removed when the session ends.

use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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

    /// The requests per second and seconds of a measurement.
    pub fn schedule(&self, kind: Kind) -> (u64, u64) {
        match kind {
            Kind::Throughput => (self.throughput_rps, self.throughput_s),
            Kind::Latency => (self.latency_rps, self.latency_s),
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
};

/// The span size of a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    /// 1 KiB spans, 100 per request.
    Kib1,
    /// 10 KiB spans, 10 per request, which keep the generator accurate (`docs/testing/perf-baseline.md`).
    Kib10,
}

impl Lane {
    /// `1kb` or `10kb`.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "1kb" => Some(Self::Kib1),
            "10kb" => Some(Self::Kib10),
            _ => None,
        }
    }

    /// The lane's name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Kib1 => "1kb",
            Self::Kib10 => "10kb",
        }
    }

    /// The load generator's `--span-size`.
    pub fn span_size(self) -> &'static str {
        match self {
            Self::Kib1 => "1k",
            Self::Kib10 => "10k",
        }
    }

    /// The load generator's `--spans-per-request`.
    pub fn spans_per_request(self) -> u32 {
        match self {
            Self::Kib1 => 100,
            Self::Kib10 => 10,
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

/// A project key: the prefix, a leading zero and 42 base62 digits, so the value stays below 2^256.
fn project_key(prefix: &str) -> Result<String, String> {
    const DIGITS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let digits: String = random(42)?
        .iter()
        .map(|b| char::from(DIGITS[usize::from(*b) % DIGITS.len()]))
        .collect();
    Ok(format!("{prefix}0{digits}"))
}

/// The throwaway secrets of a session's rig, which no published port exposes.
struct Secrets {
    clickhouse: String,
    postgres: String,
    master_key: String,
    secret_key: String,
    keys_toml: String,
}

impl Secrets {
    fn new() -> Result<Self, String> {
        let mut project = random(16)?;
        project[6] = 0x40 | (project[6] & 0x0f);
        project[8] = 0x80 | (project[8] & 0x3f);
        let project = hex(&project);
        let project_id = format!(
            "{}-{}-{}-{}-{}",
            &project[..8],
            &project[8..12],
            &project[12..16],
            &project[16..20],
            &project[20..]
        );
        let secret_key = project_key("csk_")?;
        let keys_toml = format!(
            "[[project_key]]\nproject_id = \"{project_id}\"\npublic_key = \"{}\"\nsecret = \"{secret_key}\"\n",
            project_key("cpk_")?
        );
        Ok(Self {
            clickhouse: hex(&random(16)?),
            postgres: hex(&random(16)?),
            master_key: hex(&random(32)?),
            secret_key,
            keys_toml,
        })
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
}

/// A gate session: its network, the load generator's container, and the rig of the arm in progress.
#[derive(Debug)]
pub struct Session<'r> {
    rig: &'r RigSpec,
    id: String,
    secrets: Secrets,
    /// A local directory with `keys.toml`, copied to `/etc/crucible` in each Crucible container.
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
        fs::write(config.join("keys.toml"), &secrets.keys_toml)
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
        for role in ["crucible", "clickhouse", "postgres"] {
            let _ = docker(["rm", "--force", "--volumes", &self.name(role)]);
        }
        let _ = docker(["volume", "rm", "--force", &self.name("wal")]);
        result
    }

    fn measure(
        &self,
        image: &str,
        lane: Lane,
        kind: Kind,
        dir: &Path,
        tag: &str,
    ) -> Result<ArmOutput, String> {
        let client = self
            .client
            .as_deref()
            .ok_or("the session has not started")?;
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
            "--keys-file=/etc/crucible/keys.toml".to_owned(),
            format!("--recent-max-bytes={}", self.rig.recent_max_bytes),
        ]);
        self.container(&["create"], &crucible, self.rig.crucible, &rest)?;
        self.copy_in(&self.config, &format!("{crucible}:/etc/crucible"))?;
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
        if let Err(e) =
            ready.and_then(|()| exited.map_or(Ok(()), |stopped| Err(format!("stopped: {stopped}"))))
        {
            let tail = Self::save_log(&crucible, &log);
            return Err(format!(
                "crucible from {image}: {e}; the end of its log:\n{tail}"
            ));
        }

        let target = format!("{crucible}:4318");
        let warmup = self.loadgen(&target, lane, self.rig.latency_rps, self.rig.warmup_s)?;
        let runs = dir.join("runs");
        fs::write(runs.join(format!("{tag}-warmup.json")), warmup)
            .map_err(|e| format!("{}: {e}", runs.display()))?;
        let (rps, secs) = self.rig.schedule(kind);
        let report = self.loadgen(&target, lane, rps, secs);
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
        })
    }

    /// One load generator run against `target`; an overloaded run lasts until every request is sent, so the
    /// deadline allows twenty times the schedule.
    fn loadgen(&self, target: &str, lane: Lane, rps: u64, secs: u64) -> Result<String, String> {
        let client = self
            .client
            .as_deref()
            .ok_or("the session has not started")?;
        let header = format!("Authorization: Bearer {}", self.secrets.secret_key);
        let (rate, duration) = (rps.to_string(), secs.to_string());
        let (spans, connections) = (
            lane.spans_per_request().to_string(),
            CONCURRENCY.to_string(),
        );
        capture_within(
            Command::new("docker").args([
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
            ]),
            Duration::from_secs(120 + 20 * secs),
        )
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
    fn lanes() {
        assert_eq!(Lane::parse("1kb"), Some(Lane::Kib1));
        assert_eq!(Lane::parse("10kb"), Some(Lane::Kib10));
        assert_eq!(Lane::parse("1k"), None);
        assert_eq!(
            (Lane::Kib10.span_size(), Lane::Kib10.spans_per_request()),
            ("10k", 10)
        );
    }

    #[test]
    fn secrets_have_the_documented_shapes() {
        let secrets = Secrets::new().unwrap();
        assert_eq!(secrets.master_key.len(), 64);
        assert!(secrets.secret_key.starts_with("csk_0") && secrets.secret_key.len() == 47);
        assert!(
            secrets.secret_key[4..]
                .chars()
                .all(|c| c.is_ascii_alphanumeric())
        );
        assert!(secrets.keys_toml.contains("public_key = \"cpk_0"));
        assert_eq!(format!("{secrets:?}"), "Secrets(redacted)");
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
