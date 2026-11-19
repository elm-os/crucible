//! `get_span_payload`: one payload field of one span, read in pieces of at most 32 KiB (contract 8.7). It is
//! the one tool that returns more than a preview, so it reads one field of one span at a time.

use crucible_plan::PayloadField;
use crucible_query::PayloadQuery;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ToolError;
use crate::shape::{MAX_RESULT_BYTES, Meta, meta, result_bytes};
use crate::tools::{Call, Check, Tool};

/// The most bytes of a field one call reads.
const PIECE_BYTES: u64 = 32 * 1024;

pub(crate) struct GetSpanPayload;

/// A payload field.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Field {
    /// What the span received.
    Input,
    /// What the span produced.
    Output,
    /// The system instructions of a generation.
    SystemInstructions,
    /// The span's attributes, as JSON.
    Attributes,
    /// The attributes of the resource that sent the span, as JSON.
    ResourceAttributes,
    /// The span's events, as JSON.
    Events,
    /// The span's links, as JSON.
    Links,
    /// The tools offered to a generation, as JSON.
    ToolDefinitions,
    /// The tool calls of a generation, as JSON.
    ToolCalls,
}

impl Field {
    const fn plan(self) -> PayloadField {
        match self {
            Self::Input => PayloadField::Input,
            Self::Output => PayloadField::Output,
            Self::SystemInstructions => PayloadField::SystemInstructions,
            Self::Attributes => PayloadField::Attributes,
            Self::ResourceAttributes => PayloadField::ResourceAttributes,
            Self::Events => PayloadField::Events,
            Self::Links => PayloadField::Links,
            Self::ToolDefinitions => PayloadField::ToolDefinitions,
            Self::ToolCalls => PayloadField::ToolCalls,
        }
    }
}

/// The arguments.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Args {
    /// The trace id, 32 hex characters.
    trace_id: String,
    /// The span id, 16 hex characters.
    span_id: String,
    /// The span's start, RFC 3339, as get_trace returns it: it locates the payload.
    start_time: String,
    /// The field to read.
    field: Field,
    /// The byte of the field to read from; 0 when absent. Pass `next_offset` to read on.
    #[serde(default)]
    offset: Option<u64>,
}

/// A piece of one payload field.
#[derive(Serialize, JsonSchema)]
pub(crate) struct Output {
    /// The field.
    field: Field,
    /// The byte of the field that `text` starts at.
    offset: u64,
    /// The length of the whole field in bytes.
    total_bytes: u64,
    /// The field from `offset`: at most 32 KiB, less when its text escapes heavily, and cut at a character
    /// boundary.
    text: String,
    /// Pass it as `offset`, with the same span and field, to read on; null at the end of the field.
    next_offset: Option<u64>,
    /// True when the field goes on after `text`.
    truncated: bool,
    /// How complete and how fresh the answer is.
    meta: Meta,
}

impl Tool for GetSpanPayload {
    const NAME: &'static str = "get_span_payload";
    const DESCRIPTION: &'static str = "Read one payload field of one span: its input, output, system \
        instructions, attributes, resource attributes, events, links, tool definitions or tool calls. Give the \
        span's trace_id, span_id and start_time as get_trace returns them. At most 32 KiB of text per call, cut at \
        a character boundary; when `next_offset` is not null, call again with it as `offset` to read on.";
    type Args = Args;
    type Output = Output;

    async fn call(call: &Call<'_>, args: Args) -> Result<Output, ToolError> {
        let mut check = Check::default();
        let trace_id = check.trace_id("trace_id", &args.trace_id);
        let span_id = check.span_id("span_id", &args.span_id);
        let start_time = check.time("start_time", &args.start_time);
        check.finish()?;
        let (Some(trace_id), Some(span_id), Some(start_time)) = (trace_id, span_id, start_time)
        else {
            return Err(ToolError::validation(Vec::new()));
        };
        let offset = args.offset.unwrap_or(0);
        let query = PayloadQuery {
            trace_id,
            span_id,
            start_time,
            field: args.field.plan(),
            range: Some((offset, PIECE_BYTES)),
        };
        let piece = call.service.payload(&call.ctx, query).await;
        let piece = piece.map_err(|e| ToolError::query(&e, call.request_id))?;
        let bytes = piece.bytes.as_slice();
        let render = |len: usize| {
            let end = offset.saturating_add(len as u64);
            let more = end < piece.total;
            Output {
                field: args.field,
                offset,
                total_bytes: piece.total,
                text: String::from_utf8_lossy(&bytes[..len]).into_owned(),
                next_offset: more.then_some(end),
                truncated: more,
                meta: meta(&piece.meta),
            }
        };
        let ends_field = offset.saturating_add(bytes.len() as u64) >= piece.total;
        let whole = if ends_field {
            bytes.len()
        } else {
            whole_characters(bytes)
        };
        Ok(render(longest_fitting(bytes, whole, |len| {
            result_bytes(&render(len)) <= MAX_RESULT_BYTES
        })))
    }
}

/// The bytes of `piece` up to its last whole character: a character the piece cuts at its end is read by the
/// next call.
fn whole_characters(piece: &[u8]) -> usize {
    match std::str::from_utf8(piece) {
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        _ => piece.len(),
    }
}

/// The longest prefix of the first `whole` bytes of `piece` that ends at a character start and `fits`. The
/// empty prefix always fits; a character takes at most 13 bytes of a result, so some text always does.
fn longest_fitting(piece: &[u8], whole: usize, fits: impl Fn(usize) -> bool) -> usize {
    if fits(whole) {
        return whole;
    }
    let starts: Vec<usize> = (0..whole)
        .filter(|&at| at == 0 || piece[at] & 0xc0 != 0x80)
        .collect();
    let (mut fitting, mut too_long) = (0, starts.len());
    while too_long - fitting > 1 {
        let middle = fitting + (too_long - fitting) / 2;
        if fits(starts[middle]) {
            fitting = middle;
        } else {
            too_long = middle;
        }
    }
    starts[fitting]
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn a_piece_ends_before_a_character_it_cuts() {
        let text = "aé€😀";
        let bytes = text.as_bytes();
        assert_eq!(whole_characters(bytes), bytes.len());
        // Every cut inside a character ends the piece before that character.
        for (cut, whole) in [(2, 1), (4, 3), (5, 3), (7, 6), (8, 6), (9, 6)] {
            assert_eq!(whole_characters(&bytes[..cut]), whole, "{cut}");
        }
    }

    proptest! {
        /// The prefix is the longest that fits and ends at a character start.
        #[test]
        fn the_longest_fitting_prefix_ends_at_a_character_start(
            text in "\\PC{0,300}",
            budget in 0usize..1_200,
        ) {
            let bytes = text.as_bytes();
            let len = longest_fitting(bytes, bytes.len(), |len| len <= budget);
            prop_assert!(text.is_char_boundary(len));
            prop_assert!(len <= budget || len == 0);
            let next = (len + 1..=bytes.len()).find(|at| text.is_char_boundary(*at));
            prop_assert!(next.is_none_or(|next| next > budget));
        }
    }
}
