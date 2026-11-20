//! The tools: one module each, and the registry that lists them.
//!
//! A tool module defines a type that implements [`Tool`]: its name, the description an agent reads, its
//! arguments and its output (both deriving `JsonSchema`, which become the tool's input and output schemas), and
//! the call. It reads through the query services with the call's [`Call::ctx`], checks its arguments with
//! [`Check`], keeps its result under the cap with `shape::fit`, and continues cut results with `cursor`. It is
//! registered here with its `mod` line and one entry in [`TOOLS`]. Every tool is read-only except
//! `create_export`, which starts an export job.

mod compare_mcp_deploys;
mod create_export;
mod get_data_bounds;
mod get_export;
mod get_mcp_tool;
mod get_span_children;
mod get_span_payload;
mod get_trace;
mod list_mcp_errors;
mod list_mcp_servers;
mod list_mcp_tools;
mod list_sessions;
mod list_spans;
mod list_traces;
mod list_user_sessions;
mod list_users;
mod search_spans;

use std::future::Future;

use crucible_api::ProjectSettings;
use crucible_api_types::{FieldError, ProblemCode};
use crucible_export::Exports;
use crucible_query::{QueryService, ReadContext};
use crucible_records::{ApiKeyId, RequestId, SpanId, TimeWindow, Timestamp, TraceId};
use futures::future::BoxFuture;
use rmcp::handler::server::tool::{schema_for_input, schema_for_output};
use rmcp::model::{JsonObject, ToolAnnotations};
use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::cursor::Cursors;
use crate::error::{ToolError, field_error};

/// Every tool, sorted by name. A tool registers with one entry at its sorted place.
pub(crate) const TOOLS: &[Entry] = &[
    entry::<compare_mcp_deploys::CompareMcpDeploys>(),
    entry::<create_export::CreateExport>(),
    entry::<get_data_bounds::GetDataBounds>(),
    entry::<get_export::GetExport>(),
    entry::<get_mcp_tool::GetMcpTool>(),
    entry::<get_span_children::GetSpanChildren>(),
    entry::<get_span_payload::GetSpanPayload>(),
    entry::<get_trace::GetTrace>(),
    entry::<list_mcp_errors::ListMcpErrors>(),
    entry::<list_mcp_servers::ListMcpServers>(),
    entry::<list_mcp_tools::ListMcpTools>(),
    entry::<list_sessions::ListSessions>(),
    entry::<list_spans::ListSpans>(),
    entry::<list_traces::ListTraces>(),
    entry::<list_user_sessions::ListUserSessions>(),
    entry::<list_users::ListUsers>(),
    entry::<search_spans::SearchSpans>(),
];

/// One tool.
pub(crate) trait Tool {
    /// The tool's name.
    const NAME: &'static str;
    /// Whether the tool only reads. A tool that starts work says so in its annotations.
    const READ_ONLY: bool = true;
    /// What the tool does, for the agent: what it returns, its caps, and how to continue a cut result.
    const DESCRIPTION: &'static str;
    /// The arguments. Unknown arguments are refused, so the type denies unknown fields.
    type Args: DeserializeOwned + JsonSchema + 'static;
    /// The structured result.
    type Output: Serialize + JsonSchema + 'static;

    /// Runs the tool for the call's project.
    fn call(
        call: &Call<'_>,
        args: Self::Args,
    ) -> impl Future<Output = Result<Self::Output, ToolError>> + Send;
}

/// What a tool call reads with: the key's project as a read of class `agent`, the project's settings, the query
/// service, and the cursors of the call. The request id goes into the server log with a failed read.
#[derive(Debug)]
pub(crate) struct Call<'a> {
    pub(crate) request_id: RequestId,
    pub(crate) ctx: ReadContext,
    pub(crate) settings: ProjectSettings,
    pub(crate) service: &'a QueryService,
    pub(crate) cursors: Cursors<'a>,
    /// Exports, with a control plane.
    pub(crate) exports: Option<&'a Exports>,
    /// The key the call comes from.
    pub(crate) key: ApiKeyId,
}

/// A registered tool, its types erased.
#[derive(Debug)]
pub(crate) struct Entry {
    pub(crate) name: &'static str,
    definition: fn() -> rmcp::model::Tool,
    call: for<'a> fn(&'a Call<'a>, JsonObject) -> BoxFuture<'a, Result<Value, ToolError>>,
}

impl Entry {
    /// The tool as `tools/list` describes it.
    pub(crate) fn definition(&self) -> rmcp::model::Tool {
        (self.definition)()
    }

    /// Calls the tool with the arguments of a `tools/call`.
    pub(crate) fn call<'a>(
        &self,
        call: &'a Call<'a>,
        arguments: JsonObject,
    ) -> BoxFuture<'a, Result<Value, ToolError>> {
        (self.call)(call, arguments)
    }
}

/// The registered tool named `name`.
pub(crate) fn find(name: &str) -> Option<&'static Entry> {
    TOOLS.iter().find(|entry| entry.name == name)
}

const fn entry<T: Tool>() -> Entry {
    Entry {
        name: T::NAME,
        definition: definition::<T>,
        call: call::<T>,
    }
}

fn definition<T: Tool>() -> rmcp::model::Tool {
    // Every argument type is a struct, whose schema is an object; the empty object stands in for none.
    let input = schema_for_input::<T::Args>()
        .unwrap_or_else(|_| rmcp::handler::server::common::schema_for_empty_input());
    rmcp::model::Tool::new(T::NAME, T::DESCRIPTION, input)
        .with_raw_output_schema(schema_for_output::<T::Output>())
        .with_annotations(
            ToolAnnotations::new()
                .read_only(T::READ_ONLY)
                .destructive(false)
                .idempotent(T::READ_ONLY)
                .open_world(false),
        )
}

fn call<'a, T: Tool>(
    call: &'a Call<'a>,
    arguments: JsonObject,
) -> BoxFuture<'a, Result<Value, ToolError>> {
    Box::pin(async move {
        let args = serde_json::from_value(Value::Object(arguments))
            .map_err(|error| ToolError::arguments(&error))?;
        let output = T::call(call, args).await?;
        serde_json::to_value(&output).map_err(|error| {
            ToolError::new(
                ProblemCode::InternalError,
                format!("the result did not serialize: {error}"),
            )
        })
    })
}

const MICROS_PER_DAY: i64 = 86_400_000_000;

/// The checks of a call's arguments, collected so that one refusal names every invalid argument by its JSON
/// pointer and says what it expects.
#[derive(Debug, Default)]
pub(crate) struct Check {
    errors: Vec<FieldError>,
}

impl Check {
    /// Refuses the argument `name` with a stable code and a message that says what it expects.
    pub(crate) fn error(&mut self, name: &str, code: &str, message: &str) {
        self.errors.push(field_error(name, code, message));
    }

    /// An RFC 3339 time inside the valid range of contract 1.2.
    pub(crate) fn time(&mut self, name: &str, text: &str) -> Option<Timestamp> {
        let Ok(time) = Timestamp::parse_rfc3339(text) else {
            self.error(
                name,
                "invalid_timestamp",
                "expected an RFC 3339 date-time such as 2026-11-17T09:30:00Z",
            );
            return None;
        };
        if time < Timestamp::EPOCH || time > Timestamp::MAX_EXCLUSIVE {
            self.error(name, "out_of_range", "expected a time from 1970 to 2100");
            return None;
        }
        Some(time)
    }

    /// The window `[from, to)`, which must be ordered.
    pub(crate) fn window(&mut self, from: &str, to: &str) -> Option<TimeWindow> {
        let (from, to) = (self.time("from", from), self.time("to", to));
        let window = TimeWindow::new(from?, to?).ok();
        if window.is_none() {
            self.error("to", "invalid_window", "`to` must be after `from`");
        }
        window
    }

    /// A page size from 1 to `max`, `default` when absent.
    pub(crate) fn limit(&mut self, limit: Option<u32>, default: u32, max: u32) -> u32 {
        match limit {
            None => default,
            Some(limit) if (1..=max).contains(&limit) => limit,
            Some(_) => {
                self.error(
                    "limit",
                    "out_of_range",
                    &format!("expected an integer from 1 to {max}"),
                );
                default
            }
        }
    }

    /// A trace id: 32 hex characters.
    pub(crate) fn trace_id(&mut self, name: &str, text: &str) -> Option<TraceId> {
        let id = TraceId::parse(text).ok();
        if id.is_none() {
            self.error(
                name,
                "invalid_id",
                "expected a trace id of 32 hex characters",
            );
        }
        id
    }

    /// A span id: 16 hex characters.
    pub(crate) fn span_id(&mut self, name: &str, text: &str) -> Option<SpanId> {
        let id = SpanId::parse(text).ok();
        if id.is_none() {
            self.error(
                name,
                "invalid_id",
                "expected a span id of 16 hex characters",
            );
        }
        id
    }

    /// Ends the checks: any problem found refuses the call.
    pub(crate) fn finish(self) -> Result<(), ToolError> {
        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(ToolError::validation(self.errors))
        }
    }
}

/// Refuses a window longer than the project allows (contract 12.4), saying which setting sets the maximum.
pub(crate) fn check_window(window: TimeWindow, max_window_days: u32) -> Result<(), ToolError> {
    let max = i64::from(max_window_days).saturating_mul(MICROS_PER_DAY);
    if window.len_micros() > max {
        let detail = format!(
            "the window is longer than the project's maximum of {max_window_days} days (the project's max \
             query window setting); narrow `from` and `to`"
        );
        return Err(ToolError::new(ProblemCode::QueryTooBroad, detail)
            .with_limit("max_window_days", max_window_days));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_is_sorted_and_every_tool_has_object_schemas() {
        let names: Vec<&str> = TOOLS.iter().map(|entry| entry.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(names, sorted);
        for entry in TOOLS {
            let tool = entry.definition();
            assert_eq!(tool.name, entry.name);
            assert_eq!(tool.input_schema["type"], "object", "{}", entry.name);
            let output = tool.output_schema.unwrap();
            assert_eq!(output["type"], "object", "{}", entry.name);
            let annotations = tool.annotations.unwrap();
            // Every tool reads; create_export alone starts work (an export job).
            assert_eq!(
                annotations.read_only_hint,
                Some(entry.name != "create_export")
            );
            assert!(find(entry.name).is_some());
        }
        assert!(find("drop_project").is_none());
    }
}
