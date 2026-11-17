//! Frozen enums (contract section 1.6). The numeric values are the stored `Enum8` values and the internal codec
//! values: they never change, and new variants only append. The strings are the wire names.

use core::fmt;

macro_rules! frozen_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident = $value:literal => $text:literal,)+ }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
        #[repr(u8)]
        pub enum $name {
            $($(#[$vmeta])* $variant = $value,)+
        }

        impl $name {
            /// Every variant in value order.
            pub const ALL: &'static [Self] = &[$(Self::$variant,)+];

            /// The stored numeric value.
            pub const fn value(self) -> u8 {
                self as u8
            }

            /// The variant with this stored value.
            pub const fn from_value(value: u8) -> Option<Self> {
                match value {
                    $($value => Some(Self::$variant),)+
                    _ => None,
                }
            }

            /// The wire name.
            pub const fn as_str(&self) -> &'static str {
                match self {
                    $(Self::$variant => $text,)+
                }
            }

            /// The variant with this wire name.
            pub fn parse(text: &str) -> Option<Self> {
                match text {
                    $($text => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

frozen_enum! {
    /// OTel span kind.
    SpanKind {
        Unspecified = 0 => "unspecified",
        Internal = 1 => "internal",
        Server = 2 => "server",
        Client = 3 => "client",
        Producer = 4 => "producer",
        Consumer = 5 => "consumer",
    }
}

frozen_enum! {
    /// OTel status code.
    StatusCode {
        Unset = 0 => "unset",
        Ok = 1 => "ok",
        Error = 2 => "error",
    }
}

frozen_enum! {
    /// What a span represents in an agent run.
    ObservationType {
        Span = 0 => "span",
        Generation = 1 => "generation",
        Event = 2 => "event",
        Embedding = 3 => "embedding",
        Agent = 4 => "agent",
        Tool = 5 => "tool",
        Chain = 6 => "chain",
        Retriever = 7 => "retriever",
        Evaluator = 8 => "evaluator",
        Guardrail = 9 => "guardrail",
    }
}

frozen_enum! {
    /// Span level.
    Level {
        Debug = 0 => "debug",
        Default = 1 => "default",
        Warning = 2 => "warning",
        Error = 3 => "error",
    }
}

frozen_enum! {
    /// Shape of the stored input and output bodies.
    IoShape {
        Raw = 0 => "raw",
        GenaiMessages = 1 => "genai_messages",
        IndexedMessages = 2 => "indexed_messages",
    }
}

frozen_enum! {
    /// Whether a payload field was truncated at its cap.
    Truncation {
        None = 0 => "none",
        CapExceeded = 1 => "cap_exceeded",
    }
}

frozen_enum! {
    /// Where a cost came from.
    CostSource {
        None = 0 => "none",
        Provided = 1 => "provided",
        Calculated = 2 => "calculated",
    }
}

frozen_enum! {
    /// Trace visibility flag.
    TracePublic {
        Unset = 0 => "unset",
        Private = 1 => "private",
        Public = 2 => "public",
    }
}

frozen_enum! {
    /// Which side of an MCP exchange a span describes.
    McpSide {
        None = 0 => "none",
        Client = 1 => "client",
        Server = 2 => "server",
    }
}

frozen_enum! {
    /// How a span arrived. A span mapped by a compat dialect has `compat`, with the dialect id alongside.
    SourceFormat {
        Otel = 0 => "otel",
        CrucibleSdk = 1 => "crucible_sdk",
        CrucibleProxy = 2 => "crucible_proxy",
        Compat = 3 => "compat",
    }
}

frozen_enum! {
    /// Which instrumentation captured an MCP operation.
    CaptureSource {
        Sdk = 1 => "sdk",
        ProxyStdio = 2 => "proxy_stdio",
        ProxyHttp = 3 => "proxy_http",
        Otlp = 4 => "otlp",
    }
}

frozen_enum! {
    /// MCP protocol era: session-based or stateless.
    Era {
        Legacy = 1 => "legacy",
        Modern = 2 => "modern",
    }
}

frozen_enum! {
    /// MCP transport.
    Transport {
        Unknown = 0 => "unknown",
        Stdio = 1 => "stdio",
        StreamableHttp = 2 => "streamable_http",
        Sse = 3 => "sse",
    }
}

frozen_enum! {
    /// What an MCP operation targets.
    McpTargetKind {
        None = 0 => "none",
        Tool = 1 => "tool",
        Prompt = 2 => "prompt",
        Resource = 3 => "resource",
    }
}

frozen_enum! {
    /// Direction of an MCP message.
    Direction {
        ClientToServer = 1 => "client_to_server",
        ServerToClient = 2 => "server_to_client",
    }
}

frozen_enum! {
    /// Kind of MCP result.
    ResultType {
        None = 0 => "none",
        Complete = 1 => "complete",
        InputRequired = 2 => "input_required",
        Task = 3 => "task",
    }
}

frozen_enum! {
    /// How an MCP operation was cancelled.
    CancelSource {
        None = 0 => "none",
        Notification = 1 => "notification",
        StreamClose = 2 => "stream_close",
    }
}

frozen_enum! {
    /// Outcome of an MCP operation.
    McpStatus {
        Ok = 0 => "ok",
        ToolError = 1 => "tool_error",
        ProtocolError = 2 => "protocol_error",
        TransportError = 3 => "transport_error",
        Cancelled = 4 => "cancelled",
    }
}

frozen_enum! {
    /// MCP error class (contract section 9.2).
    ErrorClass {
        None = 0 => "none",
        Protocol = 1 => "protocol",
        Validation = 2 => "validation",
        NotFound = 3 => "not_found",
        ToolExecution = 4 => "tool_execution",
        Auth = 5 => "auth",
        RateLimit = 6 => "rate_limit",
        Timeout = 7 => "timeout",
        Transport = 8 => "transport",
        Cancelled = 9 => "cancelled",
        Internal = 10 => "internal",
    }
}

frozen_enum! {
    /// Subclass of a tool execution error, set by an annotation.
    ErrorSubclass {
        None = 0 => "none",
        Auth = 1 => "auth",
        RateLimit = 2 => "rate_limit",
        Upstream = 3 => "upstream",
        Business = 4 => "business",
    }
}

frozen_enum! {
    /// Source of an MCP operation's group key (contract section 9.4).
    GroupKeyKind {
        LegacySession = 1 => "legacy_session",
        Handle = 2 => "handle",
        Trace = 3 => "trace",
        Process = 4 => "process",
    }
}

frozen_enum! {
    /// Source of an MCP user identity.
    IdentitySource {
        None = 0 => "none",
        IdentifyHook = 1 => "identify_hook",
        TokenClaims = 2 => "token_claims",
        TokenVerified = 3 => "token_verified",
    }
}

frozen_enum! {
    /// What a fact is attached to.
    FactTargetKind {
        Trace = 1 => "trace",
        Span = 2 => "span",
        Session = 3 => "session",
    }
}

frozen_enum! {
    /// Who wrote a fact.
    AuthorKind {
        User = 1 => "user",
        ApiKey = 2 => "api_key",
        System = 3 => "system",
    }
}

frozen_enum! {
    /// Whether a trace head comes from the root span or is provisional.
    HeadKind {
        Root = 1 => "root",
        Provisional = 2 => "provisional",
    }
}

frozen_enum! {
    /// Data type of a score.
    ScoreDataType {
        Numeric = 1 => "numeric",
        Categorical = 2 => "categorical",
        Boolean = 3 => "boolean",
        Text = 4 => "text",
    }
}

frozen_enum! {
    /// Where a score came from.
    ScoreSource {
        Feedback = 1 => "feedback",
        Api = 2 => "api",
        Annotation = 3 => "annotation",
        Eval = 4 => "eval",
    }
}

frozen_enum! {
    /// Tag edit operation.
    TagOp {
        Add = 1 => "add",
        Remove = 2 => "remove",
    }
}

frozen_enum! {
    /// What a deletion request removes.
    DeletionKind {
        Trace = 1 => "trace",
        User = 2 => "user",
        Session = 3 => "session",
        Project = 4 => "project",
    }
}

/// State of a deletion request. Stored as text in Postgres, so it has no numeric value.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum DeletionState {
    /// Accepted, not yet logged in the WAL.
    Pending,
    /// The tombstone is durable.
    Logged,
    /// Physical deletes have been issued.
    Applied,
    /// Zero rows remain and no masked parts are left.
    Verified,
    /// Every reach-list place is done.
    Completed,
    /// Gave up; needs an operator.
    Failed,
}

impl DeletionState {
    /// Every state in lifecycle order.
    pub const ALL: &'static [Self] = &[
        Self::Pending,
        Self::Logged,
        Self::Applied,
        Self::Verified,
        Self::Completed,
        Self::Failed,
    ];

    /// The stored and wire name.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Logged => "logged",
            Self::Applied => "applied",
            Self::Verified => "verified",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    /// The state with this name.
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|s| s.as_str() == text)
    }
}

impl fmt::Display for DeletionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check<T: Copy + PartialEq + fmt::Debug>(
        all: &[T],
        value: fn(T) -> u8,
        from_value: fn(u8) -> Option<T>,
        name: fn(&T) -> &'static str,
        parse: fn(&str) -> Option<T>,
    ) {
        for v in all {
            assert_eq!(from_value(value(*v)), Some(*v));
            assert_eq!(parse(name(v)), Some(*v));
        }
        let max = all.iter().map(|v| value(*v)).max().unwrap();
        assert_eq!(from_value(max + 1), None);
    }

    #[test]
    fn frozen_values_round_trip() {
        macro_rules! each {
            ($($t:ident),+) => {$(
                check($t::ALL, $t::value, $t::from_value, $t::as_str, $t::parse);
            )+};
        }
        each!(
            SpanKind,
            StatusCode,
            ObservationType,
            Level,
            IoShape,
            Truncation,
            CostSource,
            TracePublic,
            McpSide,
            SourceFormat,
            CaptureSource,
            Era,
            Transport,
            McpTargetKind,
            Direction,
            ResultType,
            CancelSource,
            McpStatus,
            ErrorClass,
            ErrorSubclass,
            GroupKeyKind,
            IdentitySource,
            FactTargetKind,
            AuthorKind,
            HeadKind,
            ScoreDataType,
            ScoreSource,
            TagOp,
            DeletionKind
        );
        for s in DeletionState::ALL {
            assert_eq!(DeletionState::parse(s.as_str()), Some(*s));
        }
    }

    #[test]
    fn spot_check_frozen_numbers() {
        assert_eq!(ObservationType::Guardrail.value(), 9);
        assert_eq!(ErrorClass::Internal.value(), 10);
        assert_eq!(CaptureSource::Otlp.value(), 4);
        assert_eq!(SourceFormat::Compat.as_str(), "compat");
        assert_eq!(Level::parse("default"), Some(Level::Default));
        assert_eq!(McpStatus::from_value(0), Some(McpStatus::Ok));
    }
}
