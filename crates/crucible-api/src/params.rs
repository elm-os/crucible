//! Query and path parameters, checked one by one so that every problem names its parameter with a JSON pointer
//! (contract 12.3). Unknown and repeated parameters are refused (contract 1.3).

use std::collections::{BTreeMap, BTreeSet};

use axum::extract::Query;
use axum::http::Uri;
use crucible_api_types::{FieldError, ProblemCode};
use crucible_records::{RequestId, SpanId, TimeWindow, Timestamp, TraceId};

use crate::problem::{ApiError, field_error};

const MICROS_PER_DAY: i64 = 86_400_000_000;
/// The longest session or user id a span carries (contract 3.1).
const MAX_ID_BYTES: usize = 512;

/// The parameters of one request and the problems found so far.
#[derive(Debug)]
pub(crate) struct Params {
    request_id: RequestId,
    query: Vec<(String, String)>,
    errors: Vec<FieldError>,
}

impl Params {
    /// The query string of `uri`, decoded into name and value pairs.
    pub(crate) fn new(request_id: RequestId, uri: &Uri) -> Result<Self, ApiError> {
        let Query(query) = Query::<Vec<(String, String)>>::try_from_uri(uri).map_err(|_| {
            ApiError::validation(
                request_id,
                vec![field_error(
                    "",
                    "invalid_query",
                    "the query string is not decodable",
                )],
            )
        })?;
        Ok(Self {
            request_id,
            query,
            errors: Vec::new(),
        })
    }

    pub(crate) fn error(&mut self, name: &str, code: &str, message: &str) {
        self.errors.push(field_error(name, code, message));
    }

    /// Takes every value of a parameter that may be repeated, in the order given.
    pub(crate) fn all(&mut self, name: &str) -> Vec<String> {
        let mut values = Vec::new();
        self.query.retain(|(key, value)| {
            let taken = key == name;
            if taken {
                values.push(value.clone());
            }
            !taken
        });
        values
    }

    /// Takes the value of an optional parameter.
    pub(crate) fn optional(&mut self, name: &str) -> Option<String> {
        let mut values = Vec::new();
        self.query.retain(|(key, value)| {
            let taken = key == name;
            if taken {
                values.push(value.clone());
            }
            !taken
        });
        if values.len() > 1 {
            self.error(name, "repeated", "the parameter is given more than once");
        }
        values.into_iter().next()
    }

    /// Takes the value of a required parameter.
    pub(crate) fn required(&mut self, name: &str) -> Option<String> {
        let value = self.optional(name);
        if value.is_none() {
            self.error(name, "required", "the parameter is required");
        }
        value
    }

    /// A required RFC 3339 time inside the valid range of contract 1.2.
    pub(crate) fn time(&mut self, name: &str) -> Option<Timestamp> {
        let text = self.required(name)?;
        self.parse_time(name, &text)
    }

    /// An optional RFC 3339 time, checked like a required one.
    pub(crate) fn optional_time(&mut self, name: &str) -> Option<Timestamp> {
        let text = self.optional(name)?;
        self.parse_time(name, &text)
    }

    fn parse_time(&mut self, name: &str, text: &str) -> Option<Timestamp> {
        let Ok(time) = Timestamp::parse_rfc3339(text) else {
            self.error(name, "invalid_timestamp", "expected an RFC 3339 date-time");
            return None;
        };
        if time < Timestamp::EPOCH || time > Timestamp::MAX_EXCLUSIVE {
            self.error(name, "out_of_range", "expected a time from 1970 to 2100");
            return None;
        }
        Some(time)
    }

    /// The window of `from` and `to`, which must be ordered.
    pub(crate) fn window(&mut self) -> Option<TimeWindow> {
        let (from, to) = (self.time("from"), self.time("to"));
        let window = TimeWindow::new(from?, to?).ok();
        if window.is_none() {
            self.error("to", "invalid_window", "`to` must be after `from`");
        }
        window
    }

    /// An optional page size from 1 to `max`.
    pub(crate) fn limit(&mut self, max: u32) -> Option<u32> {
        let text = self.optional("limit")?;
        match text.parse::<u32>() {
            Ok(limit) if (1..=max).contains(&limit) => Some(limit),
            _ => {
                let message = format!("expected an integer from 1 to {max}");
                self.error("limit", "out_of_range", &message);
                None
            }
        }
    }

    /// A required span id parameter.
    pub(crate) fn span_id(&mut self, name: &str) -> Option<SpanId> {
        let text = self.required(name)?;
        let id = SpanId::parse(&text).ok();
        if id.is_none() {
            self.error(name, "invalid_id", "expected 16 hex characters");
        }
        id
    }

    /// A text id path parameter (`session_id`, `user_id`): any non-empty text of at most 512 bytes, the cap of
    /// the ids spans carry (contract 3.1).
    pub(crate) fn text_id(
        &mut self,
        path: &BTreeMap<String, String>,
        name: &str,
    ) -> Option<String> {
        let id = path
            .get(name)
            .filter(|id| !id.is_empty() && id.len() <= MAX_ID_BYTES)
            .cloned();
        if id.is_none() {
            self.error(name, "invalid_id", "expected 1 to 512 bytes of text");
        }
        id
    }

    /// The `trace_id` path parameter.
    pub(crate) fn trace_id(&mut self, path: &BTreeMap<String, String>) -> Option<TraceId> {
        let id = path
            .get("trace_id")
            .and_then(|text| TraceId::parse(text).ok());
        if id.is_none() {
            self.error("trace_id", "invalid_id", "expected 32 hex characters");
        }
        id
    }

    /// Ends the checks: an unknown parameter, or any problem found before, refuses the request.
    pub(crate) fn finish(mut self) -> Result<(), ApiError> {
        let unknown: BTreeSet<String> = std::mem::take(&mut self.query)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        for name in unknown {
            self.error(
                &name,
                "unknown_parameter",
                "the endpoint has no such parameter",
            );
        }
        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(ApiError::validation(self.request_id, self.errors))
        }
    }
}

/// Refuses a window longer than the project allows (contract 12.4); a window of exactly the maximum passes.
pub(crate) fn check_window(
    request_id: RequestId,
    window: TimeWindow,
    max_window_days: u32,
) -> Result<(), ApiError> {
    let max = i64::from(max_window_days).saturating_mul(MICROS_PER_DAY);
    if window.len_micros() > max {
        let detail =
            format!("the window is longer than the project's maximum of {max_window_days} days");
        return Err(
            ApiError::new(request_id, ProblemCode::QueryTooBroad, detail)
                .with_limit("max_window_days", max_window_days),
        );
    }
    Ok(())
}
