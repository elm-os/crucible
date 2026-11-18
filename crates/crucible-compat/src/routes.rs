//! Compat ingest routes (contract 12.1): the paths that other SDKs export OTLP traces to, the dialect hint each
//! sets, the headers that carry the SDK's identity there, the routing headers that mark a dialect's traffic on
//! `/v1/traces`, and the signal paths that v0 does not ingest.
//!
//! Both HTTP ingest listeners serve every route with the bodies, limits, admission, ack rule and responses of
//! `/v1/traces`, and every route accepts Crucible keys as Basic or Bearer (contract 12.2). The project always
//! comes from the key: routing headers set a hint and never select a project (contract 10.1).

/// One compat ingest route.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IngestRoute {
    /// The request path.
    pub path: &'static str,
    /// The envelope's `compat_dialect` hint for every request on the path.
    pub dialect: u8,
    /// The request headers that carry the SDK's name and version on this route. When either is present,
    /// `sdk_name` and `sdk_version` come from them instead of `User-Agent`.
    pub sdk_headers: Option<SdkHeaders>,
}

/// The lowercase names of the request headers that carry an SDK's name and version.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SdkHeaders {
    /// The SDK's name.
    pub name: &'static str,
    /// The SDK's version.
    pub version: &'static str,
}

/// A request header whose presence on `/v1/traces` sets a dialect hint.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RoutingHeader {
    /// The lowercase header name.
    pub name: &'static str,
    /// The hint it sets.
    pub dialect: u8,
}

/// The compat ingest routes.
pub const INGEST_ROUTES: [IngestRoute; 2] = [
    // The SDKs of dialect 1 export to their platform's OTLP compatibility path with Basic auth, and name
    // themselves in two headers.
    IngestRoute {
        path: "/api/public/otel/v1/traces",
        dialect: 1,
        sdk_headers: Some(SdkHeaders {
            name: "x-langfuse-sdk-name",
            version: "x-langfuse-sdk-version",
        }),
    },
    // The exporters of dialect 7 take a base URL that ends in `/otel` and append `/v1/traces`.
    IngestRoute {
        path: "/otel/v1/traces",
        dialect: 7,
        sdk_headers: None,
    },
];

/// The routing headers. Dialect 7's exporters name the destination object of their spans in it.
pub const ROUTING_HEADERS: [RoutingHeader; 1] = [RoutingHeader {
    name: "x-bt-parent",
    dialect: 7,
}];

/// Signal paths that compat SDKs send to and v0 does not ingest (logs). Each lies under a route's first
/// segment, so it answers problem 404 ([`under_route_prefix`]).
pub const NOT_INGESTED: [&str; 1] = ["/otel/v1/logs"];

/// The first segment of `path`, with its leading `/`: `/otel` for `/otel/v1/traces`.
fn first_segment(path: &str) -> &str {
    let end = path
        .get(1..)
        .and_then(|rest| rest.find('/'))
        .map_or(path.len(), |at| at + 1);
    path.get(..end).unwrap_or(path)
}

/// Whether `path` lies under the first segment of a compat route, matched as a whole segment: `/otel` covers
/// `/otel` and `/otel/...` but not `/otelx`. Paths there that name no route never fall back to HTML; they
/// answer problem 404 (contract 12.1).
pub fn under_route_prefix(path: &str) -> bool {
    INGEST_ROUTES
        .iter()
        .any(|route| first_segment(path) == first_segment(route.path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DIALECTS;

    /// A lowercase HTTP header name: a token of letters, digits and `-`.
    fn header_name(name: &str) -> bool {
        !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    }

    #[test]
    fn routes_are_distinct_static_paths_of_registered_dialects() {
        for (i, route) in INGEST_ROUTES.iter().enumerate() {
            let path = route.path;
            // A static path, so that the router takes it as is: no parameter, wildcard or empty segment.
            assert!(path.starts_with('/') && !path.ends_with('/'), "{path}");
            assert!(!path.contains("//"), "{path}");
            assert!(
                path.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"/-_.".contains(&b)),
                "{path}"
            );
            assert!(
                path != "/v1/traces" && !path.starts_with("/v1/") && !path.starts_with("/api/v1/"),
                "{path} is a native path"
            );
            assert!(
                INGEST_ROUTES[..i]
                    .iter()
                    .all(|earlier| earlier.path != path),
                "{path} is listed twice"
            );
            assert!(route.dialect != 0, "{path} hints the native dialect");
            assert!(
                DIALECTS.iter().any(|d| d.id == route.dialect),
                "{path}: dialect {} is not registered",
                route.dialect
            );
            if let Some(sdk) = route.sdk_headers {
                assert!(header_name(sdk.name) && header_name(sdk.version), "{sdk:?}");
                assert_ne!(sdk.name, sdk.version);
            }
        }
    }

    #[test]
    fn routing_headers_name_registered_dialects() {
        for header in ROUTING_HEADERS {
            assert!(header_name(header.name), "{header:?}");
            assert!(
                ![
                    "authorization",
                    "content-type",
                    "content-encoding",
                    "user-agent"
                ]
                .contains(&header.name),
                "{header:?}"
            );
            assert!(header.dialect != 0 && DIALECTS.iter().any(|d| d.id == header.dialect));
        }
    }

    #[test]
    fn route_prefixes_cover_their_unknown_paths_and_nothing_native() {
        for route in INGEST_ROUTES {
            let prefix = first_segment(route.path);
            assert!(
                prefix.len() > 1 && prefix.len() < route.path.len(),
                "{prefix}"
            );
            for path in [
                route.path,
                prefix,
                &format!("{prefix}/"),
                &format!("{prefix}/x/y"),
            ] {
                assert!(under_route_prefix(path), "{path}");
            }
            assert!(!under_route_prefix(&format!("{prefix}x")));
        }
        for path in NOT_INGESTED {
            assert!(under_route_prefix(path), "{path}");
            assert!(INGEST_ROUTES.iter().all(|route| route.path != path));
        }
        for path in [
            "/",
            "",
            "/v1/traces",
            "/v1/logs",
            "/healthz",
            "/readyz",
            "/mcp",
            "/traces/",
        ] {
            assert!(!under_route_prefix(path), "{path}");
        }
    }

    #[test]
    fn first_segments() {
        for (path, segment) in [
            ("/otel/v1/traces", "/otel"),
            ("/otel", "/otel"),
            ("/otel/", "/otel"),
            ("/", "/"),
            ("", ""),
            ("otel", "otel"),
        ] {
            assert_eq!(first_segment(path), segment, "{path}");
        }
    }
}
