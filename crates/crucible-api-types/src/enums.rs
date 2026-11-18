//! Enums on the wire: lowercase snake_case strings with the variant names of contract 1.6.

use crucible_records as records;
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use utoipa::ToSchema;

/// Mirrors a frozen enum of `crucible-records`, whose variant names are the wire names.
macro_rules! mirror {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($(#[$vmeta])* $variant,)+
        }

        impl From<records::$name> for $name {
            fn from(value: records::$name) -> Self {
                match value {
                    $(records::$name::$variant => Self::$variant,)+
                }
            }
        }

        impl From<$name> for records::$name {
            fn from(value: $name) -> Self {
                match value {
                    $($name::$variant => Self::$variant,)+
                }
            }
        }
    };
}

mirror! {
    /// The OTel status of a span.
    StatusCode {
        /// No status was set.
        Unset,
        /// The span succeeded.
        Ok,
        /// The span failed.
        Error,
    }
}

mirror! {
    /// The level of a span.
    Level {
        /// Debug detail.
        Debug,
        /// The default level.
        Default,
        /// A warning.
        Warning,
        /// An error.
        Error,
    }
}

mirror! {
    /// What a span observes.
    ObservationType {
        /// A plain span.
        Span,
        /// A model generation.
        Generation,
        /// A point event.
        Event,
        /// An embedding call.
        Embedding,
        /// An agent step.
        Agent,
        /// A tool call.
        Tool,
        /// A chain of steps.
        Chain,
        /// A retrieval.
        Retriever,
        /// An evaluation.
        Evaluator,
        /// A guardrail check.
        Guardrail,
    }
}

mirror! {
    /// Why a trace appears in the list: its root span, or its earliest span while no root has
    /// arrived.
    HeadKind {
        /// The trace's root span.
        Root,
        /// The earliest span of a trace whose root has not arrived.
        Provisional,
    }
}

mirror! {
    /// Who wrote a fact.
    AuthorKind {
        /// A user.
        User,
        /// An API key.
        ApiKey,
        /// The server.
        System,
    }
}

mirror! {
    /// How to read a score's value.
    ScoreDataType {
        /// A number.
        Numeric,
        /// One of a set of names.
        Categorical,
        /// True or false.
        Boolean,
        /// Free text.
        Text,
    }
}

mirror! {
    /// Where a score came from.
    ScoreSource {
        /// End-user feedback.
        Feedback,
        /// Written through the API.
        Api,
        /// A reviewer's annotation.
        Annotation,
        /// An evaluation.
        Eval,
    }
}

mirror! {
    /// A tag edit.
    TagOp {
        /// The tag is added.
        Add,
        /// The tag is removed.
        Remove,
    }
}

/// Whether a response reflects every span this node acknowledged before the request arrived.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum Consistency {
    /// It does.
    Strong,
    /// It may trail: the read barrier expired, a peer did not answer, or a replica lags.
    Lagging,
}

/// Why a response may be missing rows.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum PartialReason {
    /// The telemetry store did not answer, or the read could touch spans that were dropped from the recent
    /// index during a store outage.
    StoreUnavailable,
    /// The request deadline passed before every part answered.
    Deadline,
    /// The read could touch a quarantined batch.
    Quarantined,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire<T: Serialize>(value: T) -> String {
        serde_json::to_value(value)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn mirrored_enums_use_the_frozen_names() {
        for &value in records::StatusCode::ALL {
            assert_eq!(wire(StatusCode::from(value)), value.as_str());
        }
        for &value in records::Level::ALL {
            assert_eq!(wire(Level::from(value)), value.as_str());
        }
        for &value in records::ObservationType::ALL {
            assert_eq!(wire(ObservationType::from(value)), value.as_str());
        }
        for &value in records::HeadKind::ALL {
            assert_eq!(wire(HeadKind::from(value)), value.as_str());
        }
        for &value in records::AuthorKind::ALL {
            assert_eq!(wire(AuthorKind::from(value)), value.as_str());
        }
        for &value in records::ScoreDataType::ALL {
            assert_eq!(wire(ScoreDataType::from(value)), value.as_str());
        }
        for &value in records::ScoreSource::ALL {
            assert_eq!(wire(ScoreSource::from(value)), value.as_str());
        }
        for &value in records::TagOp::ALL {
            assert_eq!(wire(TagOp::from(value)), value.as_str());
        }
    }

    #[test]
    fn meta_enums_use_the_contract_names() {
        assert_eq!(wire(Consistency::Strong), "strong");
        assert_eq!(wire(Consistency::Lagging), "lagging");
        assert_eq!(wire(PartialReason::StoreUnavailable), "store_unavailable");
        assert_eq!(wire(PartialReason::Deadline), "deadline");
        assert_eq!(wire(PartialReason::Quarantined), "quarantined");
    }
}
