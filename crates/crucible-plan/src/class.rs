//! Query classes and their limits as data (contract section 8.4).

use crucible_schema::{ProfileCaps, ServiceIdentity};

const GIB: u64 = 1 << 30;

/// The class of a read. It selects the per-query limits and the ClickHouse identity.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum QueryClass {
    /// Trace and span detail, payload, lookups.
    Point,
    /// Trace, span, session, user and operation lists; filter values.
    List,
    /// Full-text search.
    Search,
    /// Rollup reads: metrics, MCP dashboards, data bounds.
    Analytics,
    /// Metrics that the rollups do not cover; aggregate filters; MCP users and sessions.
    AnalyticsRaw,
    /// Alert evaluation (rollups only).
    Alert,
    /// Every read by an API-key principal or through the MCP server.
    Agent,
    /// Export jobs.
    Export,
}

/// The default limits of one class. Deployments may configure other values.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ClassLimits {
    /// `max_execution_time`, in seconds.
    pub max_execution_time_secs: u32,
    /// `max_memory_usage`, in bytes.
    pub max_memory_usage: u64,
    /// `max_rows_to_read`; `None` is unlimited.
    pub max_rows_to_read: Option<u64>,
    /// `max_bytes_to_read`; `None` is unlimited, which is the default of every class.
    pub max_bytes_to_read: Option<u64>,
    /// `max_threads`.
    pub max_threads: u32,
    /// The ClickHouse identity that runs the class.
    pub identity: ServiceIdentity,
}

/// Settings every class sends explicitly, besides its limits (contract sections 8.4 and 8.5).
///
/// `use_statistics = 0` keeps column statistics out of query planning. ClickHouse 26.8 builds statistics on every
/// column and reuses its snapshot of them only while a table's parts are the ones its last refresh saw (every
/// 300 s), so under ingest a read that plans with them, such as trace by id's span read, loads the statistics of
/// every part of the table. The plans' keys and skip indexes do not need them.
pub const CLASS_FIXED_SETTINGS: [(&str, &str); 4] = [
    ("timeout_overflow_mode", "throw"),
    ("read_overflow_mode", "throw"),
    ("cancel_http_readonly_queries_on_client_close", "1"),
    ("use_statistics", "0"),
];

/// The Rust-side deadline is the class time plus this many seconds.
pub const DEADLINE_SLACK_SECS: u32 = 2;

impl QueryClass {
    /// Every class.
    pub const ALL: [Self; 8] = [
        Self::Point,
        Self::List,
        Self::Search,
        Self::Analytics,
        Self::AnalyticsRaw,
        Self::Alert,
        Self::Agent,
        Self::Export,
    ];

    /// The class name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Point => "point",
            Self::List => "list",
            Self::Search => "search",
            Self::Analytics => "analytics",
            Self::AnalyticsRaw => "analytics_raw",
            Self::Alert => "alert",
            Self::Agent => "agent",
            Self::Export => "export",
        }
    }

    /// The default limits.
    pub const fn limits(self) -> ClassLimits {
        const fn ui(secs: u32, gib: u64, rows: Option<u64>, threads: u32) -> ClassLimits {
            ClassLimits {
                max_execution_time_secs: secs,
                max_memory_usage: gib * GIB,
                max_rows_to_read: rows,
                max_bytes_to_read: None,
                max_threads: threads,
                identity: ServiceIdentity::ReadUi,
            }
        }
        match self {
            Self::Point => ui(2, 1, Some(5_000_000), 4),
            Self::List => ui(3, 2, Some(200_000_000), 8),
            Self::Search => ui(10, 4, Some(500_000_000), 8),
            Self::Analytics => ui(10, 4, Some(200_000_000), 8),
            Self::AnalyticsRaw => ui(30, 8, Some(2_000_000_000), 8),
            Self::Alert => ui(10, 2, Some(200_000_000), 2),
            Self::Agent => ClassLimits {
                identity: ServiceIdentity::ReadAgent,
                ..ui(5, 1, Some(100_000_000), 4)
            },
            Self::Export => ui(300, 2, None, 2),
        }
    }

    /// The Rust-side deadline in seconds: the class time plus [`DEADLINE_SLACK_SECS`].
    pub const fn deadline_secs(self) -> u32 {
        self.limits().max_execution_time_secs + DEADLINE_SLACK_SECS
    }
}

/// Who asked: decides between the class an endpoint names and the `agent` class.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Caller {
    /// A session-authenticated request, an alert, an export or a job.
    Session,
    /// An API-key principal or the MCP server: every read runs in class `agent`.
    Agent,
}

impl Caller {
    /// The class a read runs in, given the class its endpoint names for session callers.
    pub const fn class(self, endpoint_class: QueryClass) -> QueryClass {
        match self {
            Self::Session => endpoint_class,
            Self::Agent => QueryClass::Agent,
        }
    }
}

/// The settings-profile ceilings of a read identity: the largest value of each limit over the classes that run
/// as it (contract section 7.1). `None` for identities that run no query class.
pub fn profile_caps(identity: ServiceIdentity) -> Option<ProfileCaps> {
    let mut classes = QueryClass::ALL
        .into_iter()
        .map(QueryClass::limits)
        .filter(|l| l.identity == identity);
    let first = classes.next()?;
    let mut caps = ProfileCaps {
        max_execution_time_secs: first.max_execution_time_secs,
        max_memory_usage: first.max_memory_usage,
        max_rows_to_read: first.max_rows_to_read,
        max_threads: first.max_threads,
    };
    for l in classes {
        caps.max_execution_time_secs = caps.max_execution_time_secs.max(l.max_execution_time_secs);
        caps.max_memory_usage = caps.max_memory_usage.max(l.max_memory_usage);
        caps.max_rows_to_read = match (caps.max_rows_to_read, l.max_rows_to_read) {
            (Some(a), Some(b)) => Some(a.max(b)),
            _ => None,
        };
        caps.max_threads = caps.max_threads.max(l.max_threads);
    }
    Some(caps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_are_the_contract_table() {
        let row = |c: QueryClass| {
            let l = c.limits();
            (
                c.as_str(),
                l.max_execution_time_secs,
                l.max_memory_usage / GIB,
                l.max_rows_to_read,
                l.max_threads,
                l.identity,
            )
        };
        let ui = ServiceIdentity::ReadUi;
        assert_eq!(
            QueryClass::ALL.map(row),
            [
                ("point", 2, 1, Some(5_000_000), 4, ui),
                ("list", 3, 2, Some(200_000_000), 8, ui),
                ("search", 10, 4, Some(500_000_000), 8, ui),
                ("analytics", 10, 4, Some(200_000_000), 8, ui),
                ("analytics_raw", 30, 8, Some(2_000_000_000), 8, ui),
                ("alert", 10, 2, Some(200_000_000), 2, ui),
                (
                    "agent",
                    5,
                    1,
                    Some(100_000_000),
                    4,
                    ServiceIdentity::ReadAgent
                ),
                ("export", 300, 2, None, 2, ui),
            ]
        );
        assert_eq!(QueryClass::Point.deadline_secs(), 4);
        assert_eq!(QueryClass::Export.deadline_secs(), 302);
    }

    #[test]
    fn profile_ceilings_are_the_class_maxima() {
        assert_eq!(
            profile_caps(ServiceIdentity::ReadUi),
            Some(ProfileCaps {
                max_execution_time_secs: 300,
                max_memory_usage: 8 * GIB,
                max_rows_to_read: None,
                max_threads: 8,
            })
        );
        assert_eq!(
            profile_caps(ServiceIdentity::ReadAgent),
            Some(ProfileCaps {
                max_execution_time_secs: 5,
                max_memory_usage: GIB,
                max_rows_to_read: Some(100_000_000),
                max_threads: 4,
            })
        );
        for identity in [
            ServiceIdentity::Admin,
            ServiceIdentity::Ingest,
            ServiceIdentity::Maint,
        ] {
            assert_eq!(profile_caps(identity), None);
        }
    }

    #[test]
    fn agent_callers_always_run_in_the_agent_class() {
        for class in QueryClass::ALL {
            assert_eq!(Caller::Agent.class(class), QueryClass::Agent);
            assert_eq!(Caller::Session.class(class), class);
        }
    }
}
