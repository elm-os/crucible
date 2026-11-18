//! The conformance suite files (`conformance/`): constants, scenarios and the capability manifest, and the
//! loader that parses and cross-checks every file of the suite.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::golden::{Common, Golden};
use crate::registry::Registry;
use crate::scrub::ScrubRules;
use crate::spec;

/// Schema strings of the suite's files.
pub mod schema {
    /// `suite.json`.
    pub const SUITE: &str = "crucible.conformance.suite/v1";
    /// `scenarios/*.json`.
    pub const SCENARIO: &str = "crucible.conformance.scenario/v1";
    /// `goldens/*.json`.
    pub const GOLDEN: &str = "crucible.conformance.golden/v1";
    /// `goldens/common.json`.
    pub const COMMON: &str = "crucible.conformance.common/v1";
    /// `registry.json`.
    pub const REGISTRY: &str = "crucible.conformance.registry/v1";
    /// `scrub-rules.json`.
    pub const SCRUB: &str = "crucible.conformance.scrub/v1";
    /// A capability manifest.
    pub const MANIFEST: &str = "crucible.conformance.manifest/v1";
    /// The run spec the driver hands a runner.
    pub const RUN: &str = "crucible.conformance.run/v1";
}

/// A suite file that cannot be used, with the file and the reason.
#[derive(Debug, thiserror::Error)]
#[error("{file}: {message}")]
pub struct SuiteError {
    /// The file, relative to the suite directory.
    pub file: String,
    /// What is wrong and where.
    pub message: String,
}

impl SuiteError {
    pub(crate) fn new(file: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            file: file.into(),
            message: message.into(),
        }
    }
}

/// Parses a JSON file into `T`, naming the file in the error.
pub(crate) fn parse<T: for<'de> Deserialize<'de>>(
    file: &str,
    bytes: &[u8],
) -> Result<T, SuiteError> {
    serde_json::from_slice(bytes).map_err(|e| SuiteError::new(file, e.to_string()))
}

pub(crate) fn check_schema(file: &str, found: &str, expected: &str) -> Result<(), SuiteError> {
    if found == expected {
        Ok(())
    } else {
        Err(SuiteError::new(
            file,
            format!("schema is {found:?}; this driver reads {expected:?}"),
        ))
    }
}

/// Scenario kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// An agent program using the SDK's agent helpers.
    Agent,
    /// A reference MCP server instrumented by the SDK.
    Mcp,
}

/// Protocol era.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Era {
    /// Protocol versions with an `initialize` handshake (2025-11-25 and earlier).
    Legacy,
    /// Stateless protocol versions (2026-07-28 and later).
    Modern,
}

impl Era {
    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Modern => "modern",
        }
    }
}

/// MCP transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    /// Newline-delimited JSON over a child process's stdin and stdout.
    Stdio,
    /// Streamable HTTP.
    StreamableHttp,
}

impl Transport {
    /// The wire name (contract 1.6 `Transport`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::StreamableHttp => "streamable_http",
        }
    }
}

/// An era and a transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Variant {
    /// Legacy era over stdio.
    #[serde(rename = "legacy-stdio")]
    LegacyStdio,
    /// Legacy era over session-based streamable HTTP.
    #[serde(rename = "legacy-http")]
    LegacyHttp,
    /// Modern era over stdio.
    #[serde(rename = "modern-stdio")]
    ModernStdio,
    /// Modern era over stateless streamable HTTP.
    #[serde(rename = "modern-http")]
    ModernHttp,
}

impl Variant {
    /// Every variant, in report order.
    pub const ALL: [Self; 4] = [
        Self::LegacyStdio,
        Self::LegacyHttp,
        Self::ModernStdio,
        Self::ModernHttp,
    ];

    /// The name used in case ids.
    pub const fn name(self) -> &'static str {
        match self {
            Self::LegacyStdio => "legacy-stdio",
            Self::LegacyHttp => "legacy-http",
            Self::ModernStdio => "modern-stdio",
            Self::ModernHttp => "modern-http",
        }
    }

    /// The era.
    pub const fn era(self) -> Era {
        match self {
            Self::LegacyStdio | Self::LegacyHttp => Era::Legacy,
            Self::ModernStdio | Self::ModernHttp => Era::Modern,
        }
    }

    /// The transport.
    pub const fn transport(self) -> Transport {
        match self {
            Self::LegacyStdio | Self::ModernStdio => Transport::Stdio,
            Self::LegacyHttp | Self::ModernHttp => Transport::StreamableHttp,
        }
    }
}

/// `suite.json`: the values every case shares.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Constants {
    /// [`schema::SUITE`].
    pub schema: String,
    /// The project keys the SDK exports with.
    pub keys: Keys,
    /// The MCP client identity.
    pub client: Client,
    /// The protocol version of each era.
    pub eras: Eras,
    /// MCP case values.
    pub mcp: McpConstants,
    /// Agent case values.
    pub agent: AgentConstants,
}

/// Project keys.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Keys {
    /// `cpk_...`.
    pub public_key: String,
    /// `csk_...`.
    pub secret_key: String,
}

/// The MCP client identity the driver puts in every case.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Client {
    /// `clientInfo.name`.
    pub name: String,
    /// `clientInfo.version`.
    pub version: String,
    /// The client capabilities object.
    pub capabilities: Json,
}

/// The protocol version of each era.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Eras {
    /// The legacy protocol version.
    pub legacy: String,
    /// The modern protocol version.
    pub modern: String,
}

impl Eras {
    /// The version of `era`.
    pub fn version(&self, era: Era) -> &str {
        match era {
            Era::Legacy => &self.legacy,
            Era::Modern => &self.modern,
        }
    }
}

/// MCP case values.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConstants {
    /// The reference server's identity.
    pub server: ServerInfo,
    /// The SDK options of the reference server.
    pub sdk: SdkOptions,
}

/// Agent case values.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConstants {
    /// The SDK options of the agent program.
    pub sdk: SdkOptions,
}

/// An MCP server's `serverInfo`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerInfo {
    /// Name.
    pub name: String,
    /// Version.
    pub version: String,
}

/// The options a runner passes to the SDK's `init`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SdkOptions {
    /// The resource `service.name`.
    pub service_name: String,
    /// The resource `crucible.deploy.id` (MCP).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deploy_id: Option<String>,
    /// The environment (agent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    /// The release (agent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    /// The batch export interval; long, so spans leave only through flush, shutdown or a full batch.
    pub flush_interval_ms: u64,
    /// Content capture rules (MCP).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capture: Vec<CaptureRule>,
    /// Tool name to the argument whose value is the handle id (MCP, contract 9.8).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub handle_params: BTreeMap<String, String>,
}

/// One content capture rule.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureRule {
    /// The tool name.
    pub tool: String,
    /// Capture the arguments.
    pub args: bool,
    /// Capture the result.
    pub result: bool,
    /// Sampling rate in [0, 1].
    pub sample_rate: f64,
    /// Per-field cap in bytes.
    pub max_bytes: usize,
}

/// A scenario file.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    /// [`schema::SCENARIO`].
    pub schema: String,
    /// The id; equals the file stem.
    pub id: String,
    /// Agent or MCP.
    pub kind: Kind,
    /// One line for the report.
    pub title: String,
    /// The variants an MCP scenario runs in; empty for an agent scenario.
    #[serde(default)]
    pub variants: Vec<Variant>,
    /// The steps.
    pub steps: Vec<Step>,
}

/// One step, in a scenario or in a resolved run spec. Exactly one action field is set; see
/// `docs/testing/sdk-conformance.md` for each.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// The step id, unique in its case.
    pub step: String,
    /// A JSON-RPC message to send.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send: Option<Json>,
    /// Extra HTTP headers for `send` (resolved, HTTP only).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// For a request: wait for its response before the next step (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<bool>,
    /// Keep the step only in these eras (scenario only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eras: Option<Vec<Era>>,
    /// Wait for the response of an earlier request sent with `wait: false`.
    #[serde(default, rename = "await", skip_serializing_if = "Option::is_none")]
    pub await_step: Option<String>,
    /// Pause.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sleep_ms: Option<u64>,
    /// Cancel an earlier request (scenario only; resolved per transport).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel: Option<String>,
    /// The cancellation reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Abort the in-flight HTTP exchange of an earlier request (resolved, HTTP only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close: Option<String>,
    /// Run one span tree with the SDK (agent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observe: Option<Node>,
    /// Call `flush()` (agent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flush: Option<Empty>,
    /// Post a checkpoint to the collector (agent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
    /// Call `shutdown()` (agent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shutdown: Option<Empty>,
}

/// An empty JSON object.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

/// The action of a step.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action<'a> {
    /// `send`.
    Send(&'a Json),
    /// `await`.
    Await(&'a str),
    /// `sleep_ms`.
    Sleep(u64),
    /// `cancel`.
    Cancel(&'a str),
    /// `close`.
    Close(&'a str),
    /// `observe`.
    Observe(&'a Node),
    /// `flush`.
    Flush,
    /// `checkpoint`.
    Checkpoint(&'a str),
    /// `shutdown`.
    Shutdown,
}

impl Step {
    /// The step's one action.
    pub fn action(&self) -> Result<Action<'_>, String> {
        let mut actions = Vec::new();
        if let Some(message) = &self.send {
            actions.push(Action::Send(message));
        }
        if let Some(target) = &self.await_step {
            actions.push(Action::Await(target));
        }
        if let Some(ms) = self.sleep_ms {
            actions.push(Action::Sleep(ms));
        }
        if let Some(target) = &self.cancel {
            actions.push(Action::Cancel(target));
        }
        if let Some(target) = &self.close {
            actions.push(Action::Close(target));
        }
        if let Some(node) = &self.observe {
            actions.push(Action::Observe(node));
        }
        if self.flush.is_some() {
            actions.push(Action::Flush);
        }
        if let Some(name) = &self.checkpoint {
            actions.push(Action::Checkpoint(name));
        }
        if self.shutdown.is_some() {
            actions.push(Action::Shutdown);
        }
        match actions.as_slice() {
            [one] => Ok(*one),
            [] => Err(format!("step {:?} has no action", self.step)),
            _ => Err(format!("step {:?} has more than one action", self.step)),
        }
    }

    /// Whether this step sends a JSON-RPC request (a message with an `id`).
    pub fn is_request(&self) -> bool {
        self.send.as_ref().is_some_and(|m| m.get("id").is_some())
    }

    /// Whether the runner waits for this request's response before the next step.
    pub fn waits(&self) -> bool {
        self.wait.unwrap_or(true)
    }
}

/// One span of an agent scenario.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    /// Span name.
    pub name: String,
    /// Observation type (contract 10.4 names).
    #[serde(rename = "type")]
    pub observation: String,
    /// `crucible.trace.name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_name: Option<String>,
    /// Context set before the span starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Context>,
    /// `crucible.input`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Json>,
    /// `crucible.output`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Json>,
    /// `gen_ai.request.model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `gen_ai.provider.name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// `gen_ai.input.messages`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_messages: Option<Json>,
    /// `gen_ai.output.messages`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_messages: Option<Json>,
    /// `gen_ai.response.finish_reasons`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reasons: Option<Vec<String>>,
    /// `gen_ai.usage.<key>`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub usage: BTreeMap<String, u64>,
    /// `crucible.cost.<key>`, USD decimal strings.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub cost_usd: BTreeMap<String, String>,
    /// Child spans, run in order inside this one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Node>,
}

/// Context set with the SDK's context setters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Context {
    /// Session id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// User id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// Tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Metadata: string, integer, double or boolean values.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, Json>,
}

/// What a tool handler's exception becomes in an SDK.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThrowOutcome {
    /// A result with `isError: true`.
    ToolError,
    /// A JSON-RPC error with code -32603.
    Internal,
}

impl ThrowOutcome {
    /// The manifest name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ToolError => "tool_error",
            Self::Internal => "internal",
        }
    }
}

/// Whether an SDK observes progress notifications.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressCount {
    /// `crucible.mcp.progress.count` equals the notifications sent.
    Observed,
    /// The key is absent.
    Unobserved,
}

impl ProgressCount {
    /// The manifest name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Observed => "observed",
            Self::Unobserved => "unobserved",
        }
    }
}

/// A declared skip.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Skip {
    /// A case id or a scenario id.
    pub case: String,
    /// Why.
    pub reason: String,
}

/// A capability manifest (`conformance/capability-manifest.schema.json`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// [`schema::MANIFEST`].
    pub schema: String,
    /// Report name.
    pub sdk: String,
    /// Implementation language.
    pub language: String,
    /// SDK version.
    pub sdk_version: String,
    /// Runner program and arguments.
    pub runner: Vec<String>,
    /// Scenario kinds the SDK implements.
    pub kinds: Vec<Kind>,
    /// Variants the SDK implements.
    pub variants: Vec<Variant>,
    /// What a tool handler's exception becomes.
    pub throw_outcome: ThrowOutcome,
    /// Whether progress notifications are counted.
    pub progress_count: ProgressCount,
    /// Declared skips.
    pub skips: Vec<Skip>,
}

impl Manifest {
    /// Parses and checks a manifest. `file` names it in errors.
    pub fn parse(file: &str, bytes: &[u8]) -> Result<Self, SuiteError> {
        let manifest: Self = parse(file, bytes)?;
        check_schema(file, &manifest.schema, schema::MANIFEST)?;
        let valid_name = !manifest.sdk.is_empty()
            && manifest.sdk.len() <= 64
            && manifest
                .sdk
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !manifest.sdk.starts_with('-');
        if !valid_name {
            return Err(SuiteError::new(
                file,
                "sdk must be 1 to 64 characters of a-z, 0-9 and '-', not starting with '-'",
            ));
        }
        if manifest.runner.is_empty() || manifest.runner.iter().any(String::is_empty) {
            return Err(SuiteError::new(
                file,
                "runner must be a non-empty list of non-empty strings",
            ));
        }
        for skip in &manifest.skips {
            if skip.reason.trim().is_empty() {
                return Err(SuiteError::new(
                    file,
                    format!("skip of {:?} needs a reason", skip.case),
                ));
            }
        }
        Ok(manifest)
    }

    /// Checks that every skip names a case or scenario of `suite`.
    pub fn check_against(&self, file: &str, suite: &Suite) -> Result<(), SuiteError> {
        let cases = suite.cases();
        for skip in &self.skips {
            let known = cases
                .iter()
                .any(|case| case.id == skip.case || case.scenario.id == skip.case);
            if !known {
                return Err(SuiteError::new(
                    file,
                    format!(
                        "skip names {:?}, which is neither a case nor a scenario of the suite",
                        skip.case
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Why the manifest skips `case`, or `None` when it runs.
    pub fn skip_reason(&self, case: &spec::Case<'_>) -> Option<String> {
        if !self.kinds.contains(&case.scenario.kind) {
            return Some(format!(
                "the manifest's kinds do not include {:?}",
                case.scenario.kind
            ));
        }
        if let Some(variant) = case.variant
            && !self.variants.contains(&variant)
        {
            return Some(format!(
                "the manifest's variants do not include {}",
                variant.name()
            ));
        }
        self.skips
            .iter()
            .find(|s| s.case == case.id || s.case == case.scenario.id)
            .map(|s| format!("declared skip: {}", s.reason))
    }
}

/// The whole suite, parsed and cross-checked.
#[derive(Debug)]
pub struct Suite {
    /// `suite.json`.
    pub constants: Constants,
    /// `registry.json`.
    pub registry: Registry,
    /// `scrub-rules.json`.
    pub scrub: ScrubRules,
    /// Scenarios, by id.
    pub scenarios: BTreeMap<String, Scenario>,
    /// Goldens, by scenario id.
    pub goldens: BTreeMap<String, Golden>,
    /// `goldens/common.json`.
    pub common: Common,
}

impl Suite {
    /// Loads a suite from its files: `(path relative to the suite directory with '/' separators, contents)`.
    /// Files the suite does not use (manifests, the manifest schema) are ignored.
    pub fn from_files<I, P, B>(files: I) -> Result<Self, SuiteError>
    where
        I: IntoIterator<Item = (P, B)>,
        P: AsRef<str>,
        B: AsRef<[u8]>,
    {
        let mut constants = None;
        let mut registry = None;
        let mut scrub = None;
        let mut common = None;
        let mut scenarios = BTreeMap::new();
        let mut golden_files = BTreeMap::new();
        for (path, bytes) in files {
            let (path, bytes) = (path.as_ref(), bytes.as_ref());
            match path {
                "suite.json" => constants = Some(parse::<Constants>(path, bytes)?),
                "registry.json" => registry = Some(Registry::parse(path, bytes)?),
                "scrub-rules.json" => scrub = Some(ScrubRules::parse(path, bytes)?),
                "goldens/common.json" => common = Some(Common::parse(path, bytes)?),
                _ => {
                    if let Some(stem) = json_stem(path, "scenarios/") {
                        let scenario: Scenario = parse(path, bytes)?;
                        check_schema(path, &scenario.schema, schema::SCENARIO)?;
                        if scenario.id != stem {
                            return Err(SuiteError::new(
                                path,
                                format!("id {:?} differs from the file name", scenario.id),
                            ));
                        }
                        scenarios.insert(stem.to_owned(), scenario);
                    } else if let Some(stem) = json_stem(path, "goldens/") {
                        golden_files.insert(stem.to_owned(), (path.to_owned(), bytes.to_vec()));
                    }
                }
            }
        }
        let missing = |name: &str| SuiteError::new(name, "file is missing from the suite");
        let constants = constants.ok_or_else(|| missing("suite.json"))?;
        check_schema("suite.json", &constants.schema, schema::SUITE)?;
        let registry = registry.ok_or_else(|| missing("registry.json"))?;
        let scrub = scrub.ok_or_else(|| missing("scrub-rules.json"))?;
        let common = common.ok_or_else(|| missing("goldens/common.json"))?;
        let mut goldens = BTreeMap::new();
        for (stem, (path, bytes)) in &golden_files {
            let scenario = scenarios.get(stem).ok_or_else(|| {
                SuiteError::new(path.as_str(), "no scenario has this golden's name")
            })?;
            let golden = Golden::parse(path, bytes, scenario, &registry)?;
            goldens.insert(stem.clone(), golden);
        }
        if let Some(id) = scenarios.keys().find(|id| !goldens.contains_key(*id)) {
            return Err(SuiteError::new(
                format!("scenarios/{id}.json"),
                format!("has no golden; add goldens/{id}.json"),
            ));
        }
        let suite = Self {
            constants,
            registry,
            scrub,
            scenarios,
            goldens,
            common,
        };
        suite.check_scenarios()?;
        Ok(suite)
    }

    fn check_scenarios(&self) -> Result<(), SuiteError> {
        for scenario in self.scenarios.values() {
            let file = format!("scenarios/{}.json", scenario.id);
            match scenario.kind {
                Kind::Mcp if scenario.variants.is_empty() => {
                    return Err(SuiteError::new(file, "an MCP scenario lists its variants"));
                }
                Kind::Agent if !scenario.variants.is_empty() => {
                    return Err(SuiteError::new(file, "an agent scenario has no variants"));
                }
                _ => {}
            }
            let mut seen = Vec::new();
            for variant in &scenario.variants {
                if seen.contains(variant) {
                    return Err(SuiteError::new(
                        file,
                        format!("variant {} is listed twice", variant.name()),
                    ));
                }
                seen.push(*variant);
            }
        }
        for case in self.cases() {
            let file = format!("scenarios/{}.json", case.scenario.id);
            let steps = spec::resolve_steps(&self.constants, &case)
                .map_err(|e| SuiteError::new(file.as_str(), format!("{}: {e}", case.id)))?;
            spec::check_steps(&self.constants, &case, &steps)
                .map_err(|e| SuiteError::new(file.as_str(), format!("{}: {e}", case.id)))?;
        }
        Ok(())
    }

    /// Every case, by scenario id, then variant in [`Variant::ALL`] order.
    pub fn cases(&self) -> Vec<spec::Case<'_>> {
        let mut cases = Vec::new();
        for scenario in self.scenarios.values() {
            if scenario.variants.is_empty() {
                cases.push(spec::Case::new(scenario, None));
            }
            for variant in Variant::ALL {
                if scenario.variants.contains(&variant) {
                    cases.push(spec::Case::new(scenario, Some(variant)));
                }
            }
        }
        cases
    }
}

fn json_stem<'a>(path: &'a str, dir: &str) -> Option<&'a str> {
    let name = path.strip_prefix(dir)?;
    let stem = name.strip_suffix(".json")?;
    (!stem.contains('/') && stem != "common").then_some(stem)
}
