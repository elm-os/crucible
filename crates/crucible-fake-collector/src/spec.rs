//! Cases and run specs. The driver resolves every case into a run spec the runner can play without knowing
//! anything about eras: it adds the legacy handshake, merges the modern `_meta` fields, computes the HTTP headers
//! and turns cancellation into the transport's form. [`check_steps`] is the loader check that every resolved case
//! follows the transport and lifecycle rules of both protocol revisions.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

use crate::suite::{
    Action, Constants, Empty, Era, Keys, Kind, Node, Scenario, SdkOptions, ServerInfo, Step,
    Transport, Variant, schema,
};

/// The step id of the legacy `initialize` request.
pub const INITIALIZE_STEP: &str = "initialize";
/// The step id of the legacy `notifications/initialized` notification.
pub const INITIALIZED_STEP: &str = "initialized";

const PROTOCOL_VERSION_META: &str = "io.modelcontextprotocol/protocolVersion";
const CLIENT_INFO_META: &str = "io.modelcontextprotocol/clientInfo";
const CLIENT_CAPABILITIES_META: &str = "io.modelcontextprotocol/clientCapabilities";
const PROTOCOL_VERSION_HEADER: &str = "MCP-Protocol-Version";
const METHOD_HEADER: &str = "Mcp-Method";
const NAME_HEADER: &str = "Mcp-Name";
/// Methods whose `Mcp-Name` header carries `params.name` or `params.uri`.
const NAMED_METHODS: [&str; 3] = ["tools/call", "resources/read", "prompts/get"];
/// Observation type names (contract 10.4).
const OBSERVATION_TYPES: [&str; 10] = [
    "span",
    "generation",
    "event",
    "embedding",
    "agent",
    "tool",
    "chain",
    "retriever",
    "evaluator",
    "guardrail",
];

/// One scenario in one variant.
#[derive(Clone, Debug)]
pub struct Case<'a> {
    /// `<scenario>@<variant>`, or the scenario id for an agent case.
    pub id: String,
    /// The scenario.
    pub scenario: &'a Scenario,
    /// The variant of an MCP case.
    pub variant: Option<Variant>,
}

impl<'a> Case<'a> {
    /// The case of `scenario` in `variant`.
    pub fn new(scenario: &'a Scenario, variant: Option<Variant>) -> Self {
        let id = match variant {
            Some(v) => format!("{}@{}", scenario.id, v.name()),
            None => scenario.id.clone(),
        };
        Self {
            id,
            scenario,
            variant,
        }
    }

    /// The era of an MCP case.
    pub fn era(&self) -> Option<Era> {
        self.variant.map(Variant::era)
    }

    /// The transport of an MCP case.
    pub fn transport(&self) -> Option<Transport> {
        self.variant.map(Variant::transport)
    }
}

/// Where the runner sends telemetry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Otlp {
    /// The base URL the SDK's `init` takes.
    pub endpoint: String,
    /// `<endpoint>/v1/traces`.
    pub traces_url: String,
}

/// The MCP client identity, for information.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientIdentity {
    /// Name.
    pub name: String,
    /// Version.
    pub version: String,
}

/// The document the driver writes to a runner's stdin.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunSpec {
    /// [`schema::RUN`].
    pub schema: String,
    /// The case id.
    pub case: String,
    /// The scenario id.
    pub scenario: String,
    /// Agent or MCP.
    pub kind: Kind,
    /// The era (MCP).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub era: Option<Era>,
    /// The transport (MCP).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<Transport>,
    /// The protocol version (MCP).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<String>,
    /// The collector.
    pub otlp: Otlp,
    /// Where `checkpoint` steps post.
    pub checkpoint_url: String,
    /// The project keys.
    pub keys: Keys,
    /// The SDK's `init` options.
    pub sdk: SdkOptions,
    /// The reference server's identity (MCP).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<ServerInfo>,
    /// The client identity the steps carry (MCP).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<ClientIdentity>,
    /// The resolved steps.
    pub steps: Vec<Step>,
}

/// The collector's URLs, as a run spec names them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    /// Base URL.
    pub endpoint: String,
    /// Traces URL.
    pub traces_url: String,
    /// Checkpoint URL.
    pub checkpoint_url: String,
}

/// Builds the run spec of `case`.
pub fn run_spec(
    constants: &Constants,
    case: &Case<'_>,
    endpoints: &Endpoints,
) -> Result<RunSpec, String> {
    let steps = resolve_steps(constants, case)?;
    let mcp = case.scenario.kind == Kind::Mcp;
    Ok(RunSpec {
        schema: schema::RUN.to_owned(),
        case: case.id.clone(),
        scenario: case.scenario.id.clone(),
        kind: case.scenario.kind,
        era: case.era(),
        transport: case.transport(),
        protocol_version: case.era().map(|era| constants.eras.version(era).to_owned()),
        otlp: Otlp {
            endpoint: endpoints.endpoint.clone(),
            traces_url: endpoints.traces_url.clone(),
        },
        checkpoint_url: endpoints.checkpoint_url.clone(),
        keys: constants.keys.clone(),
        sdk: if mcp {
            constants.mcp.sdk.clone()
        } else {
            constants.agent.sdk.clone()
        },
        server: mcp.then(|| constants.mcp.server.clone()),
        client: mcp.then(|| ClientIdentity {
            name: constants.client.name.clone(),
            version: constants.client.version.clone(),
        }),
        steps,
    })
}

/// Resolves the steps of a case (module docs).
pub fn resolve_steps(constants: &Constants, case: &Case<'_>) -> Result<Vec<Step>, String> {
    match (case.scenario.kind, case.variant) {
        (Kind::Agent, None) => resolve_agent(&case.scenario.steps),
        (Kind::Mcp, Some(variant)) => resolve_mcp(constants, variant, &case.scenario.steps),
        _ => Err("an MCP case needs a variant and an agent case has none".to_owned()),
    }
}

fn resolve_agent(steps: &[Step]) -> Result<Vec<Step>, String> {
    let mut out = Vec::with_capacity(steps.len() + 1);
    for step in steps {
        match step.action()? {
            Action::Observe(_)
            | Action::Flush
            | Action::Checkpoint(_)
            | Action::Shutdown
            | Action::Sleep(_) => out.push(step.clone()),
            _ => {
                return Err(format!(
                    "step {:?}: agent scenarios use observe, flush, checkpoint, shutdown and sleep_ms",
                    step.step
                ));
            }
        }
    }
    if !out.iter().any(|s| s.shutdown.is_some()) {
        out.push(Step {
            step: "shutdown".to_owned(),
            shutdown: Some(Empty {}),
            ..Step::default()
        });
    }
    Ok(out)
}

fn resolve_mcp(
    constants: &Constants,
    variant: Variant,
    steps: &[Step],
) -> Result<Vec<Step>, String> {
    let era = variant.era();
    let http = variant.transport() == Transport::StreamableHttp;
    let version = constants.eras.version(era);
    let mut out = Vec::new();
    if era == Era::Legacy {
        out.push(Step {
            step: INITIALIZE_STEP.to_owned(),
            send: Some(json!({
                "jsonrpc": "2.0",
                "id": 0,
                "method": "initialize",
                "params": {
                    "protocolVersion": version,
                    "capabilities": constants.client.capabilities,
                    "clientInfo": { "name": constants.client.name, "version": constants.client.version },
                },
            })),
            ..Step::default()
        });
        out.push(Step {
            step: INITIALIZED_STEP.to_owned(),
            send: Some(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })),
            headers: if http {
                legacy_headers(version)
            } else {
                BTreeMap::new()
            },
            ..Step::default()
        });
    }
    for step in steps {
        if step.eras.as_ref().is_some_and(|eras| !eras.contains(&era)) {
            continue;
        }
        match step.action()? {
            Action::Send(message) => {
                let mut message = message.clone();
                if era == Era::Modern && message.get("id").is_some() {
                    merge_meta(constants, version, &mut message)?;
                }
                let headers = match (http, era) {
                    (false, _) => BTreeMap::new(),
                    (true, Era::Legacy) => legacy_headers(version),
                    (true, Era::Modern) => modern_headers(version, &message)?,
                };
                out.push(Step {
                    step: step.step.clone(),
                    send: Some(message),
                    headers,
                    wait: step.wait,
                    ..Step::default()
                });
            }
            Action::Cancel(target) => {
                if http && era == Era::Modern {
                    out.push(Step {
                        step: step.step.clone(),
                        close: Some(target.to_owned()),
                        ..Step::default()
                    });
                    continue;
                }
                let id = out
                    .iter()
                    .find(|s| s.step == target)
                    .and_then(|s| s.send.as_ref())
                    .and_then(|m| m.get("id"))
                    .cloned()
                    .ok_or_else(|| {
                        format!(
                            "step {:?} cancels {target:?}, which is not an earlier request",
                            step.step
                        )
                    })?;
                let reason = step
                    .reason
                    .clone()
                    .unwrap_or_else(|| "cancelled".to_owned());
                out.push(Step {
                    step: step.step.clone(),
                    send: Some(json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/cancelled",
                        "params": { "requestId": id, "reason": reason },
                    })),
                    headers: if http {
                        legacy_headers(version)
                    } else {
                        BTreeMap::new()
                    },
                    ..Step::default()
                });
            }
            Action::Await(_) | Action::Sleep(_) => out.push(Step {
                eras: None,
                ..step.clone()
            }),
            Action::Close(_) => {
                return Err(format!(
                    "step {:?}: scenarios cancel with `cancel`; the driver turns it into `close` on modern HTTP",
                    step.step
                ));
            }
            _ => {
                return Err(format!(
                    "step {:?}: MCP scenarios use send, await, sleep_ms and cancel",
                    step.step
                ));
            }
        }
    }
    Ok(out)
}

fn legacy_headers(version: &str) -> BTreeMap<String, String> {
    BTreeMap::from([(PROTOCOL_VERSION_HEADER.to_owned(), version.to_owned())])
}

fn modern_headers(version: &str, message: &Json) -> Result<BTreeMap<String, String>, String> {
    let method = message
        .get("method")
        .and_then(Json::as_str)
        .ok_or("a message has no method")?;
    let mut headers = BTreeMap::from([
        (PROTOCOL_VERSION_HEADER.to_owned(), version.to_owned()),
        (METHOD_HEADER.to_owned(), method.to_owned()),
    ]);
    if let Some(name) = mcp_name(message) {
        headers.insert(NAME_HEADER.to_owned(), name.to_owned());
    }
    Ok(headers)
}

/// The `Mcp-Name` source value of a request: `params.name` or `params.uri` for the named methods.
fn mcp_name(message: &Json) -> Option<&str> {
    let method = message.get("method").and_then(Json::as_str)?;
    if !NAMED_METHODS.contains(&method) {
        return None;
    }
    let params = message.get("params")?;
    params
        .get("name")
        .or_else(|| params.get("uri"))
        .and_then(Json::as_str)
}

fn merge_meta(constants: &Constants, version: &str, message: &mut Json) -> Result<(), String> {
    let object = message
        .as_object_mut()
        .ok_or("a message is not an object")?;
    let params = object.entry("params").or_insert_with(|| json!({}));
    let params = params.as_object_mut().ok_or("params is not an object")?;
    let meta = params.entry("_meta").or_insert_with(|| json!({}));
    let meta = meta
        .as_object_mut()
        .ok_or("params._meta is not an object")?;
    meta.insert(PROTOCOL_VERSION_META.to_owned(), json!(version));
    meta.insert(
        CLIENT_INFO_META.to_owned(),
        json!({ "name": constants.client.name, "version": constants.client.version }),
    );
    meta.insert(
        CLIENT_CAPABILITIES_META.to_owned(),
        constants.client.capabilities.clone(),
    );
    Ok(())
}

/// A `{"$from": "<step>/<pointer>"}` template: the step and the RFC 6901 pointer.
pub fn template(value: &Json) -> Option<(&str, &str)> {
    let object = value.as_object()?;
    if object.len() != 1 {
        return None;
    }
    let reference = object.get("$from")?.as_str()?;
    let slash = reference.find('/')?;
    Some((&reference[..slash], &reference[slash..]))
}

/// Replaces every template in `message` with the value its pointer selects in the named step's response.
/// `responses` maps step ids to response messages.
pub fn fill_templates(
    message: &mut Json,
    responses: &BTreeMap<String, Json>,
) -> Result<(), String> {
    if let Some((step, pointer)) = template(message) {
        let value = responses
            .get(step)
            .and_then(|response| response.pointer(pointer))
            .cloned()
            .ok_or_else(|| format!("no response of step {step:?} has {pointer}"))?;
        *message = value;
        return Ok(());
    }
    match message {
        Json::Array(items) => items
            .iter_mut()
            .try_for_each(|item| fill_templates(item, responses)),
        Json::Object(object) => object
            .values_mut()
            .try_for_each(|item| fill_templates(item, responses)),
        _ => Ok(()),
    }
}

fn templates<'a>(value: &'a Json, out: &mut Vec<(&'a str, &'a str)>) {
    if let Some(found) = template(value) {
        out.push(found);
        return;
    }
    match value {
        Json::Array(items) => items.iter().for_each(|item| templates(item, out)),
        Json::Object(object) => object.values().for_each(|item| templates(item, out)),
        _ => {}
    }
}

fn header_safe(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| (0x21..=0x7e).contains(&b) || b == b' ')
        && !value.starts_with(' ')
        && !value.ends_with(' ')
}

/// Checks a resolved case against the transport and lifecycle rules (module docs).
pub fn check_steps(constants: &Constants, case: &Case<'_>, steps: &[Step]) -> Result<(), String> {
    let mut ids: Vec<&str> = Vec::new();
    for step in steps {
        if step.step.is_empty() || step.step.contains('/') {
            return Err(format!("step id {:?} is empty or holds '/'", step.step));
        }
        if ids.contains(&step.step.as_str()) {
            return Err(format!("step id {:?} is used twice", step.step));
        }
        ids.push(&step.step);
        step.action()?;
    }
    match (case.scenario.kind, case.variant) {
        (Kind::Mcp, Some(variant)) => check_mcp(constants, variant, steps),
        (Kind::Agent, None) => check_agent(steps),
        _ => Err("an MCP case needs a variant and an agent case has none".to_owned()),
    }
}

/// What the checker remembers about an earlier request.
struct Sent<'a> {
    step: &'a str,
    id: &'a Json,
    waits: bool,
    method: &'a str,
    tool: Option<&'a str>,
    settled: bool,
}

fn check_mcp(constants: &Constants, variant: Variant, steps: &[Step]) -> Result<(), String> {
    let era = variant.era();
    let http = variant.transport() == Transport::StreamableHttp;
    let version = constants.eras.version(era);
    if era == Era::Legacy {
        check_handshake(constants, version, steps)?;
    }
    let mut sent: Vec<Sent<'_>> = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        let name = step.step.as_str();
        match step.action()? {
            Action::Send(message) => {
                check_message(name, message)?;
                let method = message
                    .get("method")
                    .and_then(Json::as_str)
                    .unwrap_or_default();
                if let Some(id) = message.get("id") {
                    if *id == json!(0) && name != INITIALIZE_STEP {
                        return Err(format!(
                            "step {name:?}: JSON-RPC id 0 belongs to the legacy initialize request"
                        ));
                    }
                    if sent.iter().any(|s| s.id == id) {
                        return Err(format!("step {name:?} reuses JSON-RPC id {id}"));
                    }
                    check_templates(name, message, &sent)?;
                    if era == Era::Modern {
                        check_modern_meta(constants, version, name, message)?;
                    }
                    sent.push(Sent {
                        step: name,
                        id,
                        waits: step.waits(),
                        method,
                        tool: message.pointer("/params/name").and_then(Json::as_str),
                        settled: step.waits(),
                    });
                } else {
                    if step.wait.is_some() {
                        return Err(format!("step {name:?}: wait applies to requests only"));
                    }
                    if method == "notifications/cancelled" {
                        let target = message.pointer("/params/requestId");
                        let pending = sent
                            .iter_mut()
                            .find(|s| Some(s.id) == target && !s.waits && !s.settled)
                            .ok_or_else(|| {
                                format!(
                                    "step {name:?} cancels a request that is not in flight (sent with wait: false and not yet awaited)"
                                )
                            })?;
                        pending.settled = true;
                    }
                }
                check_headers(era, http, version, index, step, message)?;
            }
            Action::Await(target) | Action::Close(target) => {
                if step.close.is_some() && !http {
                    return Err(format!("step {name:?}: close exists on HTTP only"));
                }
                let pending = sent
                    .iter_mut()
                    .find(|s| s.step == target && !s.waits && !s.settled)
                    .ok_or_else(|| {
                        format!(
                            "step {name:?} names {target:?}, which is not an earlier request in flight"
                        )
                    })?;
                pending.settled = true;
            }
            Action::Sleep(_) => {}
            _ => return Err(format!("step {name:?} is not an MCP step")),
        }
    }
    for request in &sent {
        if request.method == "tools/call" && request.tool.is_none() {
            return Err(format!(
                "step {:?}: tools/call without params.name",
                request.step
            ));
        }
    }
    Ok(())
}

fn check_message(name: &str, message: &Json) -> Result<(), String> {
    let object = message
        .as_object()
        .ok_or_else(|| format!("step {name:?}: send is not an object"))?;
    if object.get("jsonrpc") != Some(&json!("2.0")) {
        return Err(format!("step {name:?}: jsonrpc is not \"2.0\""));
    }
    if !object
        .get("method")
        .and_then(Json::as_str)
        .is_some_and(|m| !m.is_empty())
    {
        return Err(format!("step {name:?}: method is missing"));
    }
    if object.contains_key("result") || object.contains_key("error") {
        return Err(format!("step {name:?}: the client sends no responses"));
    }
    if let Some(id) = object.get("id")
        && !(id.is_i64() || id.is_u64() || id.is_string())
    {
        return Err(format!(
            "step {name:?}: id is neither an integer nor a string"
        ));
    }
    Ok(())
}

fn check_handshake(constants: &Constants, version: &str, steps: &[Step]) -> Result<(), String> {
    let first = steps.first().and_then(|s| s.send.as_ref());
    let second = steps.get(1).and_then(|s| s.send.as_ref());
    let opens = steps.first().is_some_and(|s| s.step == INITIALIZE_STEP)
        && first.and_then(|m| m.get("method")) == Some(&json!("initialize"))
        && first.and_then(|m| m.get("id")) == Some(&json!(0))
        && first.and_then(|m| m.pointer("/params/protocolVersion")) == Some(&json!(version))
        && first.and_then(|m| m.pointer("/params/clientInfo"))
            == Some(&json!({ "name": constants.client.name, "version": constants.client.version }));
    let initialized = steps.get(1).is_some_and(|s| s.step == INITIALIZED_STEP)
        && second == Some(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    if !opens || !initialized {
        return Err(
            "a legacy case opens with initialize (id 0, the suite's version and client info) and notifications/initialized"
                .to_owned(),
        );
    }
    let later = steps.iter().skip(2).filter_map(|s| s.send.as_ref());
    if later
        .filter_map(|m| m.get("method"))
        .any(|m| *m == json!("initialize"))
    {
        return Err("a legacy case sends initialize once".to_owned());
    }
    Ok(())
}

fn check_modern_meta(
    constants: &Constants,
    version: &str,
    name: &str,
    message: &Json,
) -> Result<(), String> {
    let meta = message.pointer("/params/_meta");
    let expect = [
        (PROTOCOL_VERSION_META, json!(version)),
        (
            CLIENT_INFO_META,
            json!({ "name": constants.client.name, "version": constants.client.version }),
        ),
        (
            CLIENT_CAPABILITIES_META,
            constants.client.capabilities.clone(),
        ),
    ];
    for (key, value) in expect {
        if meta.and_then(|m| m.get(key)) != Some(&value) {
            return Err(format!(
                "step {name:?}: a modern request carries params._meta[{key:?}] = {value}"
            ));
        }
    }
    Ok(())
}

fn check_templates(name: &str, message: &Json, sent: &[Sent<'_>]) -> Result<(), String> {
    let mut found = Vec::new();
    templates(message, &mut found);
    for (step, pointer) in &found {
        if !pointer.starts_with('/') {
            return Err(format!(
                "step {name:?}: template pointer {pointer:?} is not a JSON pointer"
            ));
        }
        if !sent.iter().any(|s| s.step == *step && s.waits) {
            return Err(format!(
                "step {name:?}: template names {step:?}, which is not an earlier awaited request"
            ));
        }
    }
    if let Some(state) = message.pointer("/params/requestState") {
        let Some((step, pointer)) = template(state) else {
            return Err(format!(
                "step {name:?}: requestState must echo an earlier result through a template"
            ));
        };
        let original = sent.iter().find(|s| s.step == step);
        let same_tool = original.is_some_and(|s| {
            s.method == "tools/call"
                && s.tool == message.pointer("/params/name").and_then(Json::as_str)
        });
        if pointer != "/result/requestState" || !same_tool {
            return Err(format!(
                "step {name:?}: an MRTR retry echoes /result/requestState of an earlier call of the same tool"
            ));
        }
    }
    Ok(())
}

fn check_headers(
    era: Era,
    http: bool,
    version: &str,
    index: usize,
    step: &Step,
    message: &Json,
) -> Result<(), String> {
    let name = step.step.as_str();
    if !http {
        return if step.headers.is_empty() {
            Ok(())
        } else {
            Err(format!("step {name:?}: stdio steps carry no headers"))
        };
    }
    for (header, value) in &step.headers {
        if !header_safe(header) || !header_safe(value) {
            return Err(format!(
                "step {name:?}: header {header:?} is not visible ASCII"
            ));
        }
    }
    let expected = match era {
        Era::Legacy if index == 0 => BTreeMap::new(),
        Era::Legacy => legacy_headers(version),
        Era::Modern => {
            let body_version = message
                .pointer(&format!(
                    "/params/_meta/{}",
                    PROTOCOL_VERSION_META.replace('/', "~1")
                ))
                .and_then(Json::as_str);
            if message.get("id").is_some() && body_version != Some(version) {
                return Err(format!(
                    "step {name:?}: the MCP-Protocol-Version header must equal the body's protocol version"
                ));
            }
            modern_headers(version, message)?
        }
    };
    if step.headers != expected {
        return Err(format!(
            "step {name:?}: HTTP headers {:?} differ from the rule's {expected:?}",
            step.headers
        ));
    }
    Ok(())
}

fn check_agent(steps: &[Step]) -> Result<(), String> {
    let mut checkpoints: Vec<&str> = Vec::new();
    let mut shutdowns = 0;
    for step in steps {
        match step.action()? {
            Action::Observe(node) => check_node(&step.step, node)?,
            Action::Checkpoint(name) => {
                if name.is_empty() || checkpoints.contains(&name) {
                    return Err(format!(
                        "step {:?}: checkpoint names are non-empty and unique",
                        step.step
                    ));
                }
                checkpoints.push(name);
            }
            Action::Shutdown => shutdowns += 1,
            Action::Flush | Action::Sleep(_) => {}
            _ => return Err(format!("step {:?} is not an agent step", step.step)),
        }
    }
    if shutdowns != 1 {
        return Err("an agent case shuts the SDK down exactly once".to_owned());
    }
    Ok(())
}

fn check_node(step: &str, node: &Node) -> Result<(), String> {
    if node.name.is_empty() {
        return Err(format!("step {step:?}: a node has an empty name"));
    }
    if !OBSERVATION_TYPES.contains(&node.observation.as_str()) {
        return Err(format!(
            "step {step:?}: node {:?} has type {:?}, which is not an observation type of contract 10.4",
            node.name, node.observation
        ));
    }
    if let Some(context) = &node.context {
        for (key, value) in &context.metadata {
            if !(value.is_string() || value.is_boolean() || value.is_number()) {
                return Err(format!(
                    "step {step:?}: metadata {key:?} is not a string, number or boolean"
                ));
            }
        }
    }
    node.children
        .iter()
        .try_for_each(|child| check_node(step, child))
}

/// One line a runner prints on stdout (`docs/testing/sdk-conformance.md`, "Runner events").
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum RunnerEvent {
    /// A `send` step completed.
    Response(ResponseEvent),
}

/// The response a runner observed for one `send` step.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResponseEvent {
    /// The step id.
    pub step: String,
    /// The HTTP status; `None` on stdio.
    #[serde(default)]
    pub http_status: Option<u16>,
    /// The `MCP-Session-Id` response header.
    #[serde(default)]
    pub session_id: Option<String>,
    /// The JSON-RPC response; `None` for a notification.
    #[serde(default)]
    pub message: Option<Json>,
    /// Server messages that arrived for the request before its response.
    #[serde(default)]
    pub notifications: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_take_the_pointed_value_from_an_earlier_response() {
        let responses = BTreeMap::from([(
            "ask".to_owned(),
            json!({ "result": { "requestState": "opaque", "n": [1, 2] } }),
        )]);
        let mut message = json!({ "params": { "requestState": { "$from": "ask/result/requestState" }, "list": [{ "$from": "ask/result/n/1" }] } });
        fill_templates(&mut message, &responses).unwrap();
        assert_eq!(
            message,
            json!({ "params": { "requestState": "opaque", "list": [2] } })
        );
        let mut missing = json!({ "$from": "nope/result" });
        assert!(fill_templates(&mut missing, &responses).is_err());
        // An object with other keys is not a template.
        assert_eq!(template(&json!({ "$from": "a/b", "x": 1 })), None);
        assert_eq!(template(&json!({ "$from": "a/b/c" })), Some(("a", "/b/c")));
    }
}
