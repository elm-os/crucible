//! The `meta` object of every telemetry list and detail response (contract 8.6).

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use utoipa::ToSchema;

use crate::enums::{Consistency, PartialReason};

/// How complete and how fresh a telemetry response is.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct Meta {
    /// The instant the response describes; for a list, the `as_of` its first page fixed.
    #[schema(format = DateTime)]
    pub as_of: String,
    /// Whether rows may be missing, for the reason in `partial_reason`.
    pub partial: bool,
    /// Why rows may be missing; present only when `partial` is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub partial_reason: Option<PartialReason>,
    /// `strong` when the response reflects every span this node acknowledged before the request arrived.
    pub consistency: Consistency,
}
