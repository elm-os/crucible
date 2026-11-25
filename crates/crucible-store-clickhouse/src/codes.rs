//! The server error-code tables (contract 5.6), kept as data, and the mapping from a failed statement to the
//! typed errors of `crucible-env`. Conformance row C12 pins the codes across the version matrix.

use crucible_env::{InsertError, QueryError, RetryReason, StoreError};

/// The class of an insert that the server refused with a known code.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum InsertCodeClass {
    /// Nothing landed; retry with backoff.
    Retry(RetryReason),
    /// The block may have landed; probe.
    UnknownOutcome,
    /// The deployment is misconfigured.
    FatalConfig,
    /// The server refuses the block's data.
    Poison,
}

/// Server codes that classify a failed insert. A code that is not listed is an unknown outcome, because probing
/// is always safe.
pub const INSERT_CODES: [(u32, InsertCodeClass); 20] = [
    (202, InsertCodeClass::Retry(RetryReason::TooManyQueries)),
    (252, InsertCodeClass::Retry(RetryReason::TooManyParts)),
    (241, InsertCodeClass::Retry(RetryReason::Memory)),
    (243, InsertCodeClass::Retry(RetryReason::NoSpace)),
    (242, InsertCodeClass::Retry(RetryReason::ReadOnly)),
    (159, InsertCodeClass::UnknownOutcome),
    (319, InsertCodeClass::UnknownOutcome),
    (999, InsertCodeClass::UnknownOutcome),
    (516, InsertCodeClass::FatalConfig),
    (497, InsertCodeClass::FatalConfig),
    (60, InsertCodeClass::FatalConfig),
    (16, InsertCodeClass::FatalConfig),
    (53, InsertCodeClass::Poison),
    (27, InsertCodeClass::Poison),
    (117, InsertCodeClass::Poison),
    // The similar data errors of contract 5.6, as a Native block provokes them on a real server: a body cut
    // short (33), a frame that fails its checksum (271) or names no codec (432), and a column whose values (376)
    // or type (43) cannot be converted to the table's.
    (33, InsertCodeClass::Poison),
    (271, InsertCodeClass::Poison),
    (432, InsertCodeClass::Poison),
    (376, InsertCodeClass::Poison),
    (43, InsertCodeClass::Poison),
];

/// Code 252 is shared: too many parts (retry), and a block that spans more partitions than
/// `max_partitions_per_insert_block` (a routing bug, so the block is poison). The message tells them apart.
pub const TOO_MANY_PARTITIONS_MESSAGE: &str = "Too many partitions for single INSERT block";

/// The class of a read that the server refused with a known code.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ReadCodeClass {
    /// A row, byte or set-size guardrail.
    TooBroad,
    /// The class time limit.
    Timeout,
    /// The class memory limit.
    MemoryExceeded,
    /// The server refuses queries for now, or the query was killed.
    Unavailable,
}

/// Server codes that classify a failed read. A code that is not listed is `QueryError::Internal`.
pub const READ_CODES: [(u32, ReadCodeClass); 10] = [
    (158, ReadCodeClass::TooBroad),
    (307, ReadCodeClass::TooBroad),
    (396, ReadCodeClass::TooBroad),
    // The semi-join id cap is enforced with `max_rows_in_set`.
    (191, ReadCodeClass::TooBroad),
    (159, ReadCodeClass::Timeout),
    (241, ReadCodeClass::MemoryExceeded),
    (202, ReadCodeClass::Unavailable),
    (209, ReadCodeClass::Unavailable),
    (210, ReadCodeClass::Unavailable),
    (394, ReadCodeClass::Unavailable),
];

/// A failed statement, before it is mapped to the error type of its method.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Failure {
    /// The server answered with an exception.
    Server { code: u32, message: String },
    /// The connection could not be opened, so no byte of the request was sent.
    NotSent(String),
    /// The connection failed after the request started, or the answer was not a ClickHouse answer.
    Network(String),
    /// The Rust-side deadline passed.
    Timeout,
    /// The Rust-side deadline of a read passed before the server sent a byte of its answer: the server did not
    /// even end the statement at its class limit, which falls before the deadline, so it is not answering.
    NoAnswer,
    /// The client refused the statement or could not read the answer.
    Client(String),
}

/// What a read whose server never answered reports.
const NO_ANSWER: &str = "the telemetry store sent nothing before the read's deadline";

/// The exception code in a server answer: the digits after the first `Code: `.
pub fn parse_code(text: &str) -> Option<u32> {
    let rest = &text[text.find("Code: ")? + "Code: ".len()..];
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    rest[..digits].parse().ok()
}

/// `NOTHING_TO_KILL`: from ClickHouse 26.11 a `KILL QUERY` that matches no running query fails with this code.
pub const NOTHING_TO_KILL: u32 = 785;

/// The answer of a `KILL QUERY`. A query that has ended counts as killed (contract 2.6), so the refusal of a kill
/// that matched no running query is success.
pub(crate) fn killed(answer: Result<(), Failure>) -> Result<(), Failure> {
    match answer {
        Err(Failure::Server {
            code: NOTHING_TO_KILL,
            ..
        }) => Ok(()),
        other => other,
    }
}

/// Whether the server refused a statement because its user runs as many statements as its settings profile allows
/// (`TOO_MANY_SIMULTANEOUS_QUERIES`): the statement never ran.
pub(crate) fn too_many_queries(error: &clickhouse::error::Error) -> bool {
    matches!(error, clickhouse::error::Error::BadResponse(text) if parse_code(text) == Some(202))
}

/// Whether a statement failed before any byte of its request was sent: the connection could not be opened.
pub(crate) fn not_sent(error: &clickhouse::error::Error) -> bool {
    match error {
        clickhouse::error::Error::Network(source) => source
            .downcast_ref::<hyper_util::client::legacy::Error>()
            .is_some_and(hyper_util::client::legacy::Error::is_connect),
        _ => false,
    }
}

impl Failure {
    pub(crate) fn from_client(error: clickhouse::error::Error) -> Self {
        use clickhouse::error::Error;
        let unsent = not_sent(&error);
        match error {
            Error::BadResponse(text) => match parse_code(&text) {
                Some(code) => Self::Server {
                    code,
                    message: text,
                },
                None => Self::Network(text),
            },
            Error::Network(source) if unsent => Self::NotSent(source.to_string()),
            Error::Network(source) => Self::Network(source.to_string()),
            Error::TimedOut => Self::Timeout,
            other => Self::Client(other.to_string()),
        }
    }

    pub(crate) fn into_insert(self) -> InsertError {
        match self {
            Self::Server { code, message } => classify_insert(code, message),
            Self::NotSent(message) => InsertError::Retry {
                reason: RetryReason::NotSent,
                message,
            },
            Self::Network(message) | Self::Client(message) => InsertError::UnknownOutcome {
                code: None,
                message,
            },
            Self::Timeout | Self::NoAnswer => InsertError::UnknownOutcome {
                code: None,
                message: "the insert deadline passed".to_owned(),
            },
        }
    }

    pub(crate) fn into_query(self) -> QueryError {
        match self {
            Self::Server { code, message } => classify_read(code, message),
            Self::NotSent(message) | Self::Network(message) => QueryError::Unavailable(message),
            Self::Timeout => QueryError::Timeout,
            Self::NoAnswer => QueryError::Unavailable(NO_ANSWER.to_owned()),
            Self::Client(message) => QueryError::Internal(message),
        }
    }

    pub(crate) fn into_store(self) -> StoreError {
        match self {
            Self::Server { code, message } => StoreError::Rejected { code, message },
            Self::NotSent(message) | Self::Network(message) => StoreError::Unavailable(message),
            Self::Timeout => StoreError::Unavailable("the statement deadline passed".to_owned()),
            Self::NoAnswer => StoreError::Unavailable(NO_ANSWER.to_owned()),
            Self::Client(message) => StoreError::Internal(message),
        }
    }
}

/// The class of a failed insert by its server code and message (contract 5.6).
pub fn classify_insert(code: u32, message: String) -> InsertError {
    let class = if code == 252 && message.contains(TOO_MANY_PARTITIONS_MESSAGE) {
        Some(InsertCodeClass::Poison)
    } else {
        INSERT_CODES
            .iter()
            .find(|(known, _)| *known == code)
            .map(|(_, class)| *class)
    };
    match class {
        Some(InsertCodeClass::Retry(reason)) => InsertError::Retry { reason, message },
        Some(InsertCodeClass::FatalConfig) => InsertError::FatalConfig { code, message },
        Some(InsertCodeClass::Poison) => InsertError::Poison { code, message },
        Some(InsertCodeClass::UnknownOutcome) | None => InsertError::UnknownOutcome {
            code: Some(code),
            message,
        },
    }
}

/// The class of a failed read by its server code (contract 5.6, read guardrails).
pub fn classify_read(code: u32, message: String) -> QueryError {
    match READ_CODES.iter().find(|(known, _)| *known == code) {
        Some((_, ReadCodeClass::TooBroad)) => QueryError::TooBroad,
        Some((_, ReadCodeClass::Timeout)) => QueryError::Timeout,
        Some((_, ReadCodeClass::MemoryExceeded)) => QueryError::MemoryExceeded,
        Some((_, ReadCodeClass::Unavailable)) => QueryError::Unavailable(message),
        None => QueryError::Internal(message),
    }
}

#[cfg(test)]
mod tests {
    use crucible_env::InsertClass;

    use super::*;

    /// A kill that matched no running query killed a query that has ended (contract 2.6); 26.11 refuses it with
    /// `NOTHING_TO_KILL`. Every other failure stays a failure.
    #[test]
    fn a_kill_of_a_query_that_has_ended_succeeds() {
        let server = |code| Failure::Server {
            code,
            message: format!("Code: {code}. DB::Exception: x"),
        };
        assert_eq!(killed(Ok(())), Ok(()));
        assert_eq!(killed(Err(server(NOTHING_TO_KILL))), Ok(()));
        assert_eq!(killed(Err(server(497))), Err(server(497)));
        assert_eq!(killed(Err(Failure::Timeout)), Err(Failure::Timeout));
    }

    #[test]
    fn codes_are_parsed_from_bodies_and_from_the_header_form() {
        assert_eq!(
            parse_code("Code: 252. DB::Exception: Too many parts"),
            Some(252)
        );
        assert_eq!(parse_code("Code: 60"), Some(60));
        assert_eq!(parse_code("x Code: 159. y Code: 3."), Some(159));
        assert_eq!(parse_code("502 Bad Gateway"), None);
        assert_eq!(parse_code("Code: x"), None);
    }

    #[test]
    fn the_insert_table_is_the_contract_table() {
        let class = |code: u32| classify_insert(code, String::new()).class();
        for code in [202, 252, 241, 243, 242] {
            assert_eq!(class(code), InsertClass::Retry, "{code}");
        }
        for code in [159, 319, 999, 1, 1000] {
            assert_eq!(class(code), InsertClass::UnknownOutcome, "{code}");
        }
        for code in [516, 497, 60, 16] {
            assert_eq!(class(code), InsertClass::FatalConfig, "{code}");
        }
        for code in [53, 27, 117, 33, 271, 432, 376, 43] {
            assert_eq!(class(code), InsertClass::Poison, "{code}");
        }
        let mut codes: Vec<u32> = INSERT_CODES.iter().map(|(code, _)| *code).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), INSERT_CODES.len(), "no code is listed twice");
    }

    #[test]
    fn a_block_that_spans_partitions_is_poison_not_a_retry() {
        let parts = classify_insert(252, "Code: 252. DB::Exception: Too many parts (300)".into());
        assert_eq!(parts.class(), InsertClass::Retry);
        let partitions = classify_insert(
            252,
            format!("Code: 252. DB::Exception: {TOO_MANY_PARTITIONS_MESSAGE} (more than 1)"),
        );
        assert_eq!(partitions.class(), InsertClass::Poison);
    }

    #[test]
    fn the_read_table_maps_to_the_problem_codes() {
        let code = |code: u32| classify_read(code, String::new()).problem_code();
        for too_broad in [158, 307, 396, 191] {
            assert_eq!(code(too_broad), "query_too_broad");
        }
        assert_eq!(code(159), "query_timeout");
        assert_eq!(code(241), "query_memory_exceeded");
        assert_eq!(code(202), "telemetry_store_unavailable");
        assert_eq!(code(62), "internal_error");
    }

    #[test]
    fn failures_map_to_each_error_type() {
        let not_sent = Failure::NotSent("refused".into());
        assert!(matches!(
            not_sent.clone().into_insert(),
            InsertError::Retry {
                reason: RetryReason::NotSent,
                ..
            }
        ));
        assert!(matches!(
            not_sent.clone().into_query(),
            QueryError::Unavailable(_)
        ));
        assert!(matches!(not_sent.into_store(), StoreError::Unavailable(_)));
        let reset = Failure::Network("reset".into());
        assert_eq!(
            reset.clone().into_insert().class(),
            InsertClass::UnknownOutcome
        );
        assert!(matches!(reset.into_store(), StoreError::Unavailable(_)));
        assert_eq!(
            Failure::Timeout.into_insert().class(),
            InsertClass::UnknownOutcome
        );
        assert_eq!(Failure::Timeout.into_query(), QueryError::Timeout);
        // A read the server never answered is the store being unavailable, not a slow query.
        assert_eq!(
            Failure::NoAnswer.into_query(),
            QueryError::Unavailable(NO_ANSWER.to_owned())
        );
        let refused = Failure::Server {
            code: 497,
            message: "denied".into(),
        };
        assert_eq!(
            refused.clone().into_store(),
            StoreError::Rejected {
                code: 497,
                message: "denied".into()
            }
        );
        assert_eq!(refused.into_insert().class(), InsertClass::FatalConfig);
    }
}
