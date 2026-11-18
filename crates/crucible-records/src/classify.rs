//! The MCP error classifier (contract 9.2): one pure, total function from what was observed about an operation to
//! its `error_class`, `error_subclass`, `status` and `caller_fault`. The normalizer, the simulator and the SDK
//! conformance suite share it.
//!
//! The status always follows the observed channel: the first matching row of the contract table. An explicit
//! class sent by an SDK (`crucible.mcp.error.class`) replaces the table's class only when it is consistent with
//! that channel, which this module defines as: the table pairs that class with the observed status in some row
//! ([`class_fits_status`]). So an SDK can say that a tool error was a validation failure, but it cannot turn a
//! success into an error or a timeout into a tool error.

use crate::enums::{CancelSource, Era, ErrorClass, ErrorSubclass, McpSide, McpStatus};

/// JSON-RPC codes classified as `protocol` (row 6).
const PROTOCOL_CODES: [i32; 6] = [-32700, -32600, -32601, -32020, -32021, -32022];
/// JSON-RPC invalid params.
const INVALID_PARAMS: i32 = -32602;
/// JSON-RPC resource not found.
const RESOURCE_NOT_FOUND: i32 = -32002;
/// JSON-RPC codes for which a server-side row is the caller's fault.
const CALLER_FAULT_CODES: [i32; 5] = [-32700, -32600, -32601, INVALID_PARAMS, RESOURCE_NOT_FOUND];

/// What was observed about one MCP operation.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct McpErrorInput {
    /// Protocol era. No row of the contract table depends on it.
    pub era: Era,
    /// Which side captured the operation; the caller-fault rule needs it.
    pub side: McpSide,
    /// JSON-RPC error code of the response, when the response was an error.
    pub jsonrpc_code: Option<i32>,
    /// The result carried `isError: true`.
    pub is_error: bool,
    /// HTTP response status, when the transport is HTTP.
    pub http_status: Option<u16>,
    /// How the operation was cancelled.
    pub cancel_source: CancelSource,
    /// Server deadline, or no response by the deadline.
    pub timed_out: bool,
    /// Stream break, stdio EOF with the operation pending, or an unparseable message.
    pub transport_failure: bool,
    /// The handler raised an exception that no JSON-RPC error or result reported.
    pub unhandled_exception: bool,
    /// What the SDK added.
    pub annotations: McpErrorAnnotations,
}

/// SDK annotations. The proxy cannot observe any of them.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct McpErrorAnnotations {
    /// `crucible.mcp.error.class`.
    pub explicit_class: Option<ErrorClass>,
    /// The `mark_validation` annotation.
    pub mark_validation: bool,
    /// The `mark_not_found` annotation.
    pub mark_not_found: bool,
    /// The subclass given to `annotate_error`; it refines `tool_execution` only.
    pub subclass: ErrorSubclass,
}

impl McpErrorAnnotations {
    /// No annotations.
    pub const NONE: Self = Self {
        explicit_class: None,
        mark_validation: false,
        mark_not_found: false,
        subclass: ErrorSubclass::None,
    };
}

/// The classifier's result.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct McpClassification {
    /// `error_class`.
    pub error_class: ErrorClass,
    /// `error_subclass`: the annotated subclass when the class is `tool_execution`, else `none`.
    pub error_subclass: ErrorSubclass,
    /// `status`.
    pub status: McpStatus,
    /// `caller_fault`: exactly when `side = server` and the JSON-RPC code is -32700, -32600, -32601, -32602 or
    /// -32002.
    pub caller_fault: bool,
}

/// Classifies one operation (contract 9.2). Total: every input has exactly one result.
pub fn classify_mcp_error(input: &McpErrorInput) -> McpClassification {
    let (table_class, status) = table_row(input);
    let error_class = match input.annotations.explicit_class {
        Some(class) if class_fits_status(class, status) => class,
        _ => table_class,
    };
    let error_subclass = if error_class == ErrorClass::ToolExecution {
        input.annotations.subclass
    } else {
        ErrorSubclass::None
    };
    let caller_fault = input.side == McpSide::Server
        && input
            .jsonrpc_code
            .is_some_and(|code| CALLER_FAULT_CODES.contains(&code));
    McpClassification {
        error_class,
        error_subclass,
        status,
        caller_fault,
    }
}

/// Whether the contract table pairs `class` with `status` in some row.
pub const fn class_fits_status(class: ErrorClass, status: McpStatus) -> bool {
    match status {
        McpStatus::Ok => matches!(class, ErrorClass::None),
        McpStatus::ToolError => matches!(
            class,
            ErrorClass::Validation | ErrorClass::NotFound | ErrorClass::ToolExecution
        ),
        McpStatus::ProtocolError => matches!(
            class,
            ErrorClass::Auth
                | ErrorClass::RateLimit
                | ErrorClass::Protocol
                | ErrorClass::NotFound
                | ErrorClass::Validation
                | ErrorClass::Internal
        ),
        McpStatus::TransportError => matches!(class, ErrorClass::Timeout | ErrorClass::Transport),
        McpStatus::Cancelled => matches!(class, ErrorClass::Cancelled),
    }
}

/// The first matching row of the contract table.
fn table_row(input: &McpErrorInput) -> (ErrorClass, McpStatus) {
    let marks = &input.annotations;
    // Rows 1 to 5: the channel itself failed.
    if input.cancel_source != CancelSource::None {
        return (ErrorClass::Cancelled, McpStatus::Cancelled);
    }
    if input.timed_out {
        return (ErrorClass::Timeout, McpStatus::TransportError);
    }
    if input.transport_failure {
        return (ErrorClass::Transport, McpStatus::TransportError);
    }
    match input.http_status {
        Some(401 | 403) => return (ErrorClass::Auth, McpStatus::ProtocolError),
        Some(429) => return (ErrorClass::RateLimit, McpStatus::ProtocolError),
        _ => {}
    }
    // Rows 6 to 9: a JSON-RPC error, or an exception nothing reported.
    if let Some(code) = input.jsonrpc_code {
        let class = if PROTOCOL_CODES.contains(&code) {
            ErrorClass::Protocol
        } else if code == RESOURCE_NOT_FOUND || (code == INVALID_PARAMS && marks.mark_not_found) {
            ErrorClass::NotFound
        } else if code == INVALID_PARAMS {
            ErrorClass::Validation
        } else {
            ErrorClass::Internal
        };
        return (class, McpStatus::ProtocolError);
    }
    if input.unhandled_exception {
        return (ErrorClass::Internal, McpStatus::ProtocolError);
    }
    // Rows 10 to 12: a tool result with `isError: true`.
    if input.is_error {
        let class = if marks.mark_validation {
            ErrorClass::Validation
        } else if marks.mark_not_found {
            ErrorClass::NotFound
        } else {
            ErrorClass::ToolExecution
        };
        return (class, McpStatus::ToolError);
    }
    // Row 13: an HTTP error with no JSON-RPC body.
    if input.http_status.is_some_and(|s| s >= 400) {
        return (ErrorClass::Internal, McpStatus::ProtocolError);
    }
    // Row 14.
    (ErrorClass::None, McpStatus::Ok)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn ok_input() -> McpErrorInput {
        McpErrorInput {
            era: Era::Modern,
            side: McpSide::Server,
            jsonrpc_code: None,
            is_error: false,
            http_status: None,
            cancel_source: CancelSource::None,
            timed_out: false,
            transport_failure: false,
            unhandled_exception: false,
            annotations: McpErrorAnnotations::NONE,
        }
    }

    fn with(f: impl FnOnce(&mut McpErrorInput)) -> McpErrorInput {
        let mut input = ok_input();
        f(&mut input);
        input
    }

    /// Every row of the contract table, with an input that reaches it while an input of every later row is also
    /// present, so the order is checked too.
    #[test]
    fn every_row_of_the_table() {
        use ErrorClass as C;
        use McpStatus as S;
        let later_rows = |i: &mut McpErrorInput| {
            i.jsonrpc_code = Some(-32603);
            i.is_error = true;
            i.http_status = Some(500);
        };
        let rows: Vec<(u8, McpErrorInput, C, S)> = vec![
            (
                1,
                with(|i| {
                    later_rows(i);
                    i.timed_out = true;
                    i.transport_failure = true;
                    i.http_status = Some(401);
                    i.cancel_source = CancelSource::Notification;
                }),
                C::Cancelled,
                S::Cancelled,
            ),
            (
                1,
                with(|i| i.cancel_source = CancelSource::StreamClose),
                C::Cancelled,
                S::Cancelled,
            ),
            (
                2,
                with(|i| {
                    later_rows(i);
                    i.transport_failure = true;
                    i.http_status = Some(429);
                    i.timed_out = true;
                }),
                C::Timeout,
                S::TransportError,
            ),
            (
                3,
                with(|i| {
                    later_rows(i);
                    i.http_status = Some(403);
                    i.transport_failure = true;
                }),
                C::Transport,
                S::TransportError,
            ),
            (
                4,
                with(|i| {
                    later_rows(i);
                    i.http_status = Some(401);
                }),
                C::Auth,
                S::ProtocolError,
            ),
            (
                4,
                with(|i| {
                    later_rows(i);
                    i.http_status = Some(403);
                }),
                C::Auth,
                S::ProtocolError,
            ),
            (
                5,
                with(|i| {
                    later_rows(i);
                    i.http_status = Some(429);
                }),
                C::RateLimit,
                S::ProtocolError,
            ),
            (
                6,
                with(|i| {
                    later_rows(i);
                    i.jsonrpc_code = Some(-32700);
                }),
                C::Protocol,
                S::ProtocolError,
            ),
            (
                6,
                with(|i| i.jsonrpc_code = Some(-32600)),
                C::Protocol,
                S::ProtocolError,
            ),
            (
                6,
                with(|i| i.jsonrpc_code = Some(-32601)),
                C::Protocol,
                S::ProtocolError,
            ),
            (
                6,
                with(|i| i.jsonrpc_code = Some(-32020)),
                C::Protocol,
                S::ProtocolError,
            ),
            (
                6,
                with(|i| i.jsonrpc_code = Some(-32021)),
                C::Protocol,
                S::ProtocolError,
            ),
            (
                6,
                with(|i| i.jsonrpc_code = Some(-32022)),
                C::Protocol,
                S::ProtocolError,
            ),
            (
                7,
                with(|i| {
                    later_rows(i);
                    i.jsonrpc_code = Some(-32602);
                    i.annotations.mark_not_found = true;
                }),
                C::NotFound,
                S::ProtocolError,
            ),
            (
                7,
                with(|i| {
                    later_rows(i);
                    i.jsonrpc_code = Some(-32002);
                }),
                C::NotFound,
                S::ProtocolError,
            ),
            (
                8,
                with(|i| {
                    later_rows(i);
                    i.jsonrpc_code = Some(-32602);
                    i.annotations.mark_validation = true;
                }),
                C::Validation,
                S::ProtocolError,
            ),
            (
                9,
                with(|i| {
                    later_rows(i);
                    i.jsonrpc_code = Some(-32603);
                }),
                C::Internal,
                S::ProtocolError,
            ),
            (
                9,
                with(|i| {
                    later_rows(i);
                    i.jsonrpc_code = Some(-1);
                }),
                C::Internal,
                S::ProtocolError,
            ),
            (
                9,
                with(|i| {
                    i.is_error = true;
                    i.http_status = Some(500);
                    i.unhandled_exception = true;
                }),
                C::Internal,
                S::ProtocolError,
            ),
            (
                10,
                with(|i| {
                    i.is_error = true;
                    i.http_status = Some(500);
                    i.annotations.mark_validation = true;
                    i.annotations.mark_not_found = true;
                }),
                C::Validation,
                S::ToolError,
            ),
            (
                11,
                with(|i| {
                    i.is_error = true;
                    i.http_status = Some(500);
                    i.annotations.mark_not_found = true;
                }),
                C::NotFound,
                S::ToolError,
            ),
            (
                12,
                with(|i| {
                    i.is_error = true;
                    i.http_status = Some(500);
                }),
                C::ToolExecution,
                S::ToolError,
            ),
            (
                13,
                with(|i| i.http_status = Some(400)),
                C::Internal,
                S::ProtocolError,
            ),
            (
                13,
                with(|i| i.http_status = Some(503)),
                C::Internal,
                S::ProtocolError,
            ),
            (14, with(|i| i.http_status = Some(399)), C::None, S::Ok),
            (14, with(|i| i.http_status = Some(200)), C::None, S::Ok),
            (14, ok_input(), C::None, S::Ok),
        ];
        let mut seen = [false; 15];
        for (row, input, class, status) in rows {
            let out = classify_mcp_error(&input);
            assert_eq!(
                (out.error_class, out.status),
                (class, status),
                "row {row}: {input:?}"
            );
            seen[usize::from(row)] = true;
        }
        assert!(seen[1..].iter().all(|&s| s), "every row is covered");
    }

    #[test]
    fn explicit_class_wins_only_when_consistent_with_the_channel() {
        let tool_error = with(|i| {
            i.is_error = true;
            i.annotations.explicit_class = Some(ErrorClass::Validation);
        });
        let out = classify_mcp_error(&tool_error);
        assert_eq!(
            (out.error_class, out.status),
            (ErrorClass::Validation, McpStatus::ToolError)
        );
        let inconsistent = with(|i| {
            i.is_error = true;
            i.annotations.explicit_class = Some(ErrorClass::Auth);
        });
        assert_eq!(
            classify_mcp_error(&inconsistent).error_class,
            ErrorClass::ToolExecution
        );
        let success = with(|i| i.annotations.explicit_class = Some(ErrorClass::Internal));
        assert_eq!(classify_mcp_error(&success).error_class, ErrorClass::None);
        let timeout = with(|i| {
            i.timed_out = true;
            i.annotations.explicit_class = Some(ErrorClass::Transport);
        });
        assert_eq!(
            classify_mcp_error(&timeout).error_class,
            ErrorClass::Transport
        );
    }

    #[test]
    fn subclass_refines_tool_execution_only() {
        let annotated = |is_error, class| {
            with(|i| {
                i.is_error = is_error;
                i.annotations.subclass = ErrorSubclass::Upstream;
                i.annotations.explicit_class = class;
            })
        };
        let out = classify_mcp_error(&annotated(true, None));
        assert_eq!(out.error_subclass, ErrorSubclass::Upstream);
        let out = classify_mcp_error(&annotated(true, Some(ErrorClass::Validation)));
        assert_eq!(out.error_subclass, ErrorSubclass::None);
        let out = classify_mcp_error(&annotated(false, None));
        assert_eq!(out.error_subclass, ErrorSubclass::None);
    }

    #[test]
    fn caller_fault_rule() {
        for code in [-32700, -32600, -32601, -32602, -32002] {
            let server = with(|i| i.jsonrpc_code = Some(code));
            assert!(classify_mcp_error(&server).caller_fault, "{code}");
            let client = with(|i| {
                i.side = McpSide::Client;
                i.jsonrpc_code = Some(code);
            });
            assert!(!classify_mcp_error(&client).caller_fault, "{code}");
        }
        for code in [-32603, -32020, -32021, -32022, 0, 1] {
            let server = with(|i| i.jsonrpc_code = Some(code));
            assert!(!classify_mcp_error(&server).caller_fault, "{code}");
        }
        assert!(!classify_mcp_error(&ok_input()).caller_fault);
    }

    fn arb_input() -> impl Strategy<Value = McpErrorInput> {
        let enum_of = |n: usize| 0..n;
        (
            (
                enum_of(Era::ALL.len()),
                enum_of(McpSide::ALL.len()),
                enum_of(CancelSource::ALL.len()),
            ),
            proptest::option::of(prop_oneof![-32800i32..-31900, any::<i32>()]),
            proptest::option::of(prop_oneof![
                Just(401u16),
                Just(403),
                Just(429),
                100u16..600,
                any::<u16>()
            ]),
            any::<(bool, bool, bool, bool, bool, bool)>(),
            (
                proptest::option::of(enum_of(ErrorClass::ALL.len())),
                enum_of(ErrorSubclass::ALL.len()),
            ),
        )
            .prop_map(|((era, side, cancel), code, http, flags, (class, sub))| {
                McpErrorInput {
                    era: Era::ALL[era],
                    side: McpSide::ALL[side],
                    jsonrpc_code: code,
                    is_error: flags.0,
                    http_status: http,
                    cancel_source: CancelSource::ALL[cancel],
                    timed_out: flags.1,
                    transport_failure: flags.2,
                    unhandled_exception: flags.3,
                    annotations: McpErrorAnnotations {
                        explicit_class: class.map(|c| ErrorClass::ALL[c]),
                        mark_validation: flags.4,
                        mark_not_found: flags.5,
                        subclass: ErrorSubclass::ALL[sub],
                    },
                }
            })
    }

    proptest! {
        #[test]
        fn results_are_well_formed(input in arb_input()) {
            let out = classify_mcp_error(&input);
            prop_assert!(class_fits_status(out.error_class, out.status));
            prop_assert_eq!(out.status == McpStatus::Ok, out.error_class == ErrorClass::None);
            prop_assert!(out.error_subclass == ErrorSubclass::None || out.error_class == ErrorClass::ToolExecution);
            prop_assert!(!out.caller_fault || input.side == McpSide::Server);
            // The era never changes the result.
            for &era in Era::ALL {
                prop_assert_eq!(classify_mcp_error(&McpErrorInput { era, ..input }), out);
            }
            // The status never depends on the annotations.
            let bare = McpErrorInput { annotations: McpErrorAnnotations::NONE, ..input };
            prop_assert_eq!(classify_mcp_error(&bare).status, out.status);
        }
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    fn any_of<T: Copy>(all: &[T]) -> T {
        let i: usize = kani::any();
        kani::assume(i < all.len());
        all[i]
    }

    /// Totality over every input (contract 4.9): `era`, `side`, the JSON-RPC code, `isError`, the HTTP status, the
    /// cancel source, the timeout, the transport failure, an unhandled exception and the annotations are all
    /// unconstrained. The result is also checked to be a pair the table allows.
    #[kani::proof]
    fn classifier_is_total() {
        let input = McpErrorInput {
            era: any_of(Era::ALL),
            side: any_of(McpSide::ALL),
            jsonrpc_code: kani::any(),
            is_error: kani::any(),
            http_status: kani::any(),
            cancel_source: any_of(CancelSource::ALL),
            timed_out: kani::any(),
            transport_failure: kani::any(),
            unhandled_exception: kani::any(),
            annotations: McpErrorAnnotations {
                explicit_class: if kani::any() {
                    Some(any_of(ErrorClass::ALL))
                } else {
                    None
                },
                mark_validation: kani::any(),
                mark_not_found: kani::any(),
                subclass: any_of(ErrorSubclass::ALL),
            },
        };
        let out = classify_mcp_error(&input);
        assert!(class_fits_status(out.error_class, out.status));
        assert!((out.status == McpStatus::Ok) == (out.error_class == ErrorClass::None));
        assert!(
            out.error_subclass == ErrorSubclass::None
                || out.error_class == ErrorClass::ToolExecution
        );
        assert!(!out.caller_fault || input.side == McpSide::Server);
    }
}
