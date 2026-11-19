//! Instrumentation for MCP servers built on rmcp, in both protocol eras.
//!
//! [`CrucibleService`] wraps a [`ServerHandler`] and records one SERVER span per request and notification with
//! the MCP wire keys of contract 9.1. It implements `ServerHandler` itself, so rmcp serves it like any handler:
//! over stdio with `serve`, and over streamable HTTP inside the `StreamableHttpService` factory, which builds
//! one handler per session or per stateless request. Build the [`Instrumentation`] once, outside the factory:
//!
//! ```no_run
//! # use rmcp::ServerHandler;
//! # #[derive(Clone)] struct Orders;
//! # impl ServerHandler for Orders {}
//! # fn main() -> Result<(), crucible_sdk::mcp::OptionsError> {
//! let instrumentation = crucible_sdk::mcp::Instrumentation::new(crucible_sdk::mcp::Options::default())?;
//! let factory = move || Ok::<_, std::io::Error>(instrumentation.wrap(Orders));
//! # let _ = factory;
//! # Ok(())
//! # }
//! ```
//!
//! Call [`crate::init`] first so the spans are exported. A tool handler reaches its own operation through its
//! `RequestContext`: [`annotate_error`], [`mark_validation`], [`mark_not_found`] and [`notify_progress`].

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use rmcp::model::{Extensions, ProgressNotificationParam};
use rmcp::service::{RequestContext, RoleServer, ServiceError};

pub use crate::mcp_common::Transport;

mod op;
mod service;

/// The rmcp trait [`CrucibleService`] implements and wraps.
pub use rmcp::ServerHandler;
pub use service::CrucibleService;

/// The default cap on each captured field.
const DEFAULT_MAX_BYTES: usize = 16 * 1024;

/// Hooks for the SDK's own tests (feature `test-support`).
#[cfg(feature = "test-support")]
pub mod test_support {
    use std::sync::{Arc, PoisonError, RwLock};

    pub use crate::scrub::{MatcherHook, set_matcher_hook};

    /// Called once per captured field with the bytes of JSON the capture path serialized for it before any cut,
    /// and the bytes it handed to the scrubber.
    pub type CaptureWorkHook = Arc<dyn Fn(usize, usize) + Send + Sync>;

    static HOOK: RwLock<Option<CaptureWorkHook>> = RwLock::new(None);

    /// Installs, or with `None` removes, the hook the capture-overhead scenario reads its work bound from.
    pub fn set_capture_work_hook(hook: Option<CaptureWorkHook>) {
        *HOOK.write().unwrap_or_else(PoisonError::into_inner) = hook;
    }

    pub(super) fn note(serialized: usize, scrubbed: usize) {
        let hook = HOOK.read().unwrap_or_else(PoisonError::into_inner).clone();
        if let Some(hook) = hook {
            hook(serialized, scrubbed);
        }
    }
}

/// Options of [`Instrumentation::new`]. Every field is optional.
#[derive(Clone, Default)]
pub struct Options {
    /// The server's name in Crucible (`crucible.mcp.server.name`). When empty, the name the handler declares in
    /// `get_info` is used.
    pub server_name: String,
    /// The server's version (`crucible.mcp.server.version`). When empty, the version the handler declares in
    /// `get_info` is used.
    pub server_version: String,
    /// Reported for requests that did not arrive over streamable HTTP, which is detected. Default `Stdio`.
    pub transport: Transport,
    /// Opts tools into argument and result capture; the first rule whose glob matches applies. Content is not
    /// captured by default.
    pub capture: Vec<CaptureRule>,
    /// Maps a tool name to one of its arguments whose string value becomes the handle id (contract 9.8),
    /// scrubbed and cut at 256 bytes. A handle id from `identify` takes precedence.
    pub handle_params: BTreeMap<String, String>,
    /// Called for every request and notification; returns what the server knows about the caller. On
    /// streamable HTTP the extensions hold the request's `http::request::Parts`.
    pub identify: Option<IdentifyFn>,
    /// Suppresses every identity key: `identify` is not called.
    pub no_identity: bool,
    /// Runs after the built-in scrubbing rules on every captured argument and result, error message and handle
    /// id from `handle_params`; returns the value to send, or `None` to send nothing.
    pub scrub: Option<ScrubFn>,
}

/// The `identify` hook of [`Options`].
pub type IdentifyFn = Arc<dyn Fn(&Extensions) -> Identity + Send + Sync>;

/// The `scrub` hook of [`Options`].
pub type ScrubFn = Arc<dyn Fn(Field, &str) -> Option<String> + Send + Sync>;

impl fmt::Debug for Options {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Options")
            .field("server_name", &self.server_name)
            .field("server_version", &self.server_version)
            .field("transport", &self.transport)
            .field("capture", &self.capture)
            .field("handle_params", &self.handle_params)
            .field("identify", &self.identify.is_some())
            .field("no_identity", &self.no_identity)
            .field("scrub", &self.scrub.is_some())
            .finish()
    }
}

/// Opts the arguments and results of matching tools into capture (contract 9.10).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CaptureRule {
    /// A glob over tool names: `*` matches any run of characters and `?` any one character.
    pub tool: String,
    /// Capture the arguments (`gen_ai.tool.call.arguments`).
    pub args: bool,
    /// Capture the result (`gen_ai.tool.call.result`).
    pub result: bool,
    /// The share of matching calls captured, decided before anything is copied. 0 means 1.
    pub sample_rate: f64,
    /// The cap on each captured field, cut on a character boundary. 0 means 16 KiB.
    pub max_bytes: usize,
}

/// What an `identify` hook knows about the caller of one operation. Empty fields are not sent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Identity {
    /// Your application's user id (`user.id`).
    pub user_id: String,
    /// The issuer of the caller's token (`crucible.mcp.user.issuer`).
    pub issuer: String,
    /// The subject of the caller's token. Crucible is designed to hash it with a per-project salt and keep only
    /// the hash and the issuer (contract 9.3).
    pub subject: String,
    /// The caller's tenant (`crucible.mcp.tenant`).
    pub tenant: String,
    /// Groups the operations of one logical session (`crucible.mcp.handle.id`, contract 9.8).
    pub handle_id: String,
}

/// The captured value a `scrub` hook is called for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Field {
    /// A tool call's arguments.
    Arguments,
    /// A tool call's result.
    Result,
    /// An error message.
    ErrorMessage,
    /// A handle id taken from a tool argument.
    HandleId,
}

/// A refinement of a tool execution error (`crucible.mcp.error.subclass`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Subclass {
    /// The tool's own credentials were refused.
    Auth,
    /// The tool was rate limited.
    RateLimit,
    /// A service the tool depends on failed.
    Upstream,
    /// The tool refused for a business reason.
    Business,
}

impl Subclass {
    fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::RateLimit => "rate_limit",
            Self::Upstream => "upstream",
            Self::Business => "business",
        }
    }
}

/// Why [`Instrumentation::new`] refused its options.
#[derive(Debug, Clone, PartialEq)]
pub struct OptionsError(String);

impl fmt::Display for OptionsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "crucible: {}", self.0)
    }
}

impl std::error::Error for OptionsError {}

/// Validated options, shared by every wrapped handler.
#[derive(Debug)]
struct Config {
    options: Options,
}

/// The instrumentation of one server: build it once, then [`Instrumentation::wrap`] a handler per
/// connection, session or stateless request. Cloning is cheap.
#[derive(Clone, Debug)]
pub struct Instrumentation {
    config: Arc<Config>,
}

impl Instrumentation {
    /// Validates `options`. The error says which option to change.
    pub fn new(mut options: Options) -> Result<Self, OptionsError> {
        for (i, rule) in options.capture.iter_mut().enumerate() {
            if !(0.0..=1.0).contains(&rule.sample_rate) {
                return Err(OptionsError(format!(
                    "Options::capture[{i}].sample_rate {} is outside 0 to 1; use 0 or 1 to capture every matching call",
                    rule.sample_rate
                )));
            }
            if rule.sample_rate == 0.0 {
                rule.sample_rate = 1.0;
            }
            if rule.max_bytes == 0 {
                rule.max_bytes = DEFAULT_MAX_BYTES;
            }
        }
        Ok(Self {
            config: Arc::new(Config { options }),
        })
    }

    /// Wraps a handler. Nothing is recorded until the handler serves an operation.
    pub fn wrap<S: ServerHandler>(&self, server: S) -> CrucibleService<S> {
        CrucibleService::new(server, self.config.clone())
    }
}

/// The state of one operation that its handler can annotate, carried in the request context's extensions.
#[derive(Debug, Default)]
struct OpState {
    subclass: Mutex<Option<Subclass>>,
    validation: AtomicBool,
    not_found: AtomicBool,
    progress: AtomicU32,
    progress_sent: AtomicBool,
}

#[derive(Clone, Debug)]
struct OpHandle(Arc<OpState>);

fn op_state(context: &RequestContext<RoleServer>) -> Option<&OpState> {
    context.extensions.get::<OpHandle>().map(|h| h.0.as_ref())
}

/// Records why a tool failed: refines a tool execution error, the class of a result with `isError: true`.
/// Call it from the tool handler with the handler's context; without the wrapper it does nothing.
pub fn annotate_error(context: &RequestContext<RoleServer>, subclass: Subclass) {
    if let Some(op) = op_state(context) {
        *op.subclass.lock().unwrap_or_else(PoisonError::into_inner) = Some(subclass);
    }
}

/// Records that the tool rejected its arguments: an `isError` result counts as a validation error instead of a
/// tool execution error.
pub fn mark_validation(context: &RequestContext<RoleServer>) {
    if let Some(op) = op_state(context) {
        op.validation.store(true, Ordering::Relaxed);
    }
}

/// Records that what the request named does not exist: an `isError` result, or a JSON-RPC invalid-params
/// error (-32602), counts as not found.
pub fn mark_not_found(context: &RequestContext<RoleServer>) {
    if let Some(op) = op_state(context) {
        op.not_found.store(true, Ordering::Relaxed);
    }
}

/// Sends a progress notification for the request and counts it on the operation's span
/// (`crucible.mcp.progress.count`). Progress sent through the peer directly is not counted, and an operation
/// whose handler never calls this carries no count: Crucible reads that as unobserved.
pub async fn notify_progress(
    context: &RequestContext<RoleServer>,
    params: ProgressNotificationParam,
) -> Result<(), ServiceError> {
    context.peer.notify_progress(params).await?;
    if let Some(op) = op_state(context) {
        op.progress.fetch_add(1, Ordering::Relaxed);
        op.progress_sent.store(true, Ordering::Relaxed);
    }
    Ok(())
}
