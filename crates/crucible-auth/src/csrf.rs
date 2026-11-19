//! CSRF and origin checks for cookie-authenticated requests (contract 11.4).
//!
//! A cookie-authenticated request other than GET or HEAD must carry `X-Crucible-CSRF` equal to the session's
//! token, an `Origin` that is the server's own, and no `Sec-Fetch-Site` other than `same-origin`. The server's
//! origin is `server.public_url` when it is set; otherwise it is the request's own `Host` (same-origin), so a
//! server without a public URL works on any address it is reached at. `server.allowed_origins` adds origins.
//! Pre-session POSTs (login, password reset, invitation acceptance, first-owner setup) have no session token, so
//! they send the fixed header value `prelogin`: a custom header forces a CORS preflight, which is never granted.

use subtle::ConstantTimeEq;

/// The CSRF header.
pub const CSRF_HEADER: &str = "x-crucible-csrf";
/// The CSRF header's value on pre-session POSTs.
pub const PRELOGIN: &str = "prelogin";

/// Why a request fails the checks. Each message says how to fix the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CsrfRefusal {
    /// `X-Crucible-CSRF` is missing or wrong (`csrf_invalid`).
    #[error(
        "send the session's CSRF token (the `csrf_token` of GET /api/v1/auth/session) as `X-Crucible-CSRF`"
    )]
    Token,
    /// `X-Crucible-CSRF: prelogin` is missing on a pre-session POST (`csrf_invalid`).
    #[error("send `X-Crucible-CSRF: prelogin` with this request")]
    Prelogin,
    /// `Origin` is missing or not the server's (`origin_invalid`).
    #[error(
        "the request's Origin is not this server's; open the UI at the server's own address, or add the origin \
         to `server.allowed_origins`"
    )]
    Origin,
    /// `Sec-Fetch-Site` says the request came from another site (`origin_invalid`).
    #[error("the request came from another site (Sec-Fetch-Site); send it from the Crucible UI")]
    CrossSite,
}

/// The origins a cookie-authenticated request may come from.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Origins {
    /// The origin of `server.public_url`.
    public: Option<String>,
    /// `server.allowed_origins`.
    allowed: Vec<String>,
}

/// An origin as compared: lowercase `scheme://host[:port]` without a default port or a path. `None` when `url`
/// has no `http` or `https` scheme or no host.
pub fn origin_of(url: &str) -> Option<String> {
    let url = url.trim().to_ascii_lowercase();
    let (scheme, rest) = url.split_once("://")?;
    let default_port = match scheme {
        "http" => ":80",
        "https" => ":443",
        _ => return None,
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority.strip_suffix(default_port).unwrap_or(authority);
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    Some(format!("{scheme}://{authority}"))
}

impl Origins {
    /// The origins of a public URL and a list of extra origins. An entry that is not an `http` or `https` URL
    /// is refused, naming it.
    pub fn new(public_url: Option<&str>, allowed: &[String]) -> Result<Self, String> {
        let parse = |url: &str| {
            origin_of(url).ok_or_else(|| format!("{url:?} is not an http or https origin"))
        };
        Ok(Self {
            public: public_url.map(parse).transpose()?,
            allowed: allowed
                .iter()
                .map(|url| parse(url))
                .collect::<Result<_, _>>()?,
        })
    }

    /// Whether the public origin is HTTPS, so cookies are `__Host-` and `Secure`.
    pub fn public_is_https(&self) -> bool {
        self.public
            .as_deref()
            .is_some_and(|origin| origin.starts_with("https://"))
    }

    /// Checks `Origin` and `Sec-Fetch-Site`. `host` is the request's `Host` header, which stands in for the
    /// server's origin when no public URL is set.
    pub fn check(
        &self,
        origin: Option<&str>,
        host: Option<&str>,
        sec_fetch_site: Option<&str>,
    ) -> Result<(), CsrfRefusal> {
        if sec_fetch_site.is_some_and(|site| !site.trim().eq_ignore_ascii_case("same-origin")) {
            return Err(CsrfRefusal::CrossSite);
        }
        let origin = origin.and_then(origin_of).ok_or(CsrfRefusal::Origin)?;
        if self.allowed.contains(&origin) {
            return Ok(());
        }
        let own = match &self.public {
            Some(public) => *public == origin,
            // Same-origin: the authority the browser used is the one in `Host`. The scheme is not compared, as
            // a proxy in front may terminate TLS.
            None => host.is_some_and(|host| {
                let host = host.trim().to_ascii_lowercase();
                origin.split_once("://").is_some_and(|(scheme, authority)| {
                    let port = if scheme == "https" { ":443" } else { ":80" };
                    authority == host.strip_suffix(port).unwrap_or(&host)
                })
            }),
        };
        if own {
            Ok(())
        } else {
            Err(CsrfRefusal::Origin)
        }
    }
}

/// Checks the CSRF header of a cookie-authenticated request against the session's token, in constant time.
pub fn check_token(expected: &str, presented: Option<&str>) -> Result<(), CsrfRefusal> {
    let presented = presented.map(str::trim).unwrap_or_default();
    if bool::from(presented.as_bytes().ct_eq(expected.as_bytes())) && !expected.is_empty() {
        Ok(())
    } else {
        Err(CsrfRefusal::Token)
    }
}

/// Checks the CSRF header of a pre-session POST.
pub fn check_prelogin(presented: Option<&str>) -> Result<(), CsrfRefusal> {
    match presented {
        Some(value) if value.trim().eq_ignore_ascii_case(PRELOGIN) => Ok(()),
        _ => Err(CsrfRefusal::Prelogin),
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn origins_are_normalized() {
        assert_eq!(
            origin_of("HTTPS://Crucible.Example.com:443/ui/?x").as_deref(),
            Some("https://crucible.example.com")
        );
        assert_eq!(
            origin_of("http://10.0.0.5:8080").as_deref(),
            Some("http://10.0.0.5:8080")
        );
        assert_eq!(origin_of("http://host:80").as_deref(), Some("http://host"));
        for bad in [
            "null",
            "",
            "ftp://x",
            "http://",
            "https://user@host",
            "host",
        ] {
            assert_eq!(origin_of(bad), None, "{bad}");
        }
        assert!(Origins::new(Some("crucible"), &[]).is_err());
        assert!(
            Origins::new(Some("https://c.example.com/"), &[])
                .unwrap()
                .public_is_https()
        );
        assert!(!Origins::default().public_is_https());
    }

    #[test]
    fn without_a_public_url_the_request_host_is_the_origin() {
        let origins = Origins::default();
        assert_eq!(
            origins.check(Some("http://10.0.0.5:8080"), Some("10.0.0.5:8080"), None),
            Ok(())
        );
        // Behind a proxy that terminates TLS, the browser's origin is https while the server sees http.
        assert_eq!(
            origins.check(Some("https://c.example.com"), Some("c.example.com"), None),
            Ok(())
        );
        assert_eq!(
            origins.check(Some("http://evil.example"), Some("10.0.0.5:8080"), None),
            Err(CsrfRefusal::Origin)
        );
        assert_eq!(
            origins.check(None, Some("10.0.0.5:8080"), None),
            Err(CsrfRefusal::Origin)
        );
        assert_eq!(
            origins.check(Some("http://h"), None, None),
            Err(CsrfRefusal::Origin)
        );
    }

    #[test]
    fn a_public_url_and_allowed_origins_decide() {
        let origins = Origins::new(
            Some("https://crucible.example.com"),
            &["http://localhost:3000".to_owned()],
        )
        .unwrap();
        assert_eq!(
            origins.check(
                Some("https://crucible.example.com"),
                Some("internal:8080"),
                None
            ),
            Ok(())
        );
        assert_eq!(
            origins.check(
                Some("http://localhost:3000"),
                Some("x"),
                Some("same-origin")
            ),
            Ok(())
        );
        // With a public URL the Host header no longer counts.
        assert_eq!(
            origins.check(Some("http://internal:8080"), Some("internal:8080"), None),
            Err(CsrfRefusal::Origin)
        );
        for site in ["cross-site", "same-site", "none"] {
            assert_eq!(
                origins.check(Some("https://crucible.example.com"), None, Some(site)),
                Err(CsrfRefusal::CrossSite)
            );
        }
    }

    #[test]
    fn prelogin_is_a_fixed_value() {
        assert_eq!(check_prelogin(Some("prelogin")), Ok(()));
        assert_eq!(check_prelogin(Some("x")), Err(CsrfRefusal::Prelogin));
        assert_eq!(check_prelogin(None), Err(CsrfRefusal::Prelogin));
    }

    proptest! {
        /// Only the exact token passes.
        #[test]
        fn only_the_session_token_passes(expected in "[A-Za-z0-9_-]{43}", other in "[A-Za-z0-9_-]{0,50}") {
            prop_assert_eq!(check_token(&expected, Some(&expected)), Ok(()));
            if other != expected {
                prop_assert_eq!(check_token(&expected, Some(&other)), Err(CsrfRefusal::Token));
            }
            prop_assert_eq!(check_token(&expected, None), Err(CsrfRefusal::Token));
            prop_assert_eq!(check_token("", Some("")), Err(CsrfRefusal::Token));
        }
    }
}
