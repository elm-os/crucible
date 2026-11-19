//! The session and flow cookies (contract 11.4). Over HTTPS they are `__Host-` cookies with `Secure`; over plain
//! HTTP, common on internal networks, they keep working without `Secure` under names without the prefix. Both are
//! `HttpOnly`, `SameSite=Lax` and `Path=/`.

use std::time::Duration;

/// The session cookie over HTTPS.
pub const SESSION_SECURE: &str = "__Host-crucible_session";
/// The session cookie over plain HTTP.
pub const SESSION_PLAIN: &str = "crucible_session";
/// The flow cookie over HTTPS: pre-login state, never a session.
pub const FLOW_SECURE: &str = "__Host-crucible_flow";
/// The flow cookie over plain HTTP.
pub const FLOW_PLAIN: &str = "crucible_flow";

/// Whether the cookies of a response are for an HTTPS origin.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cookies {
    /// The public origin is HTTPS.
    pub secure: bool,
}

impl Cookies {
    /// The session cookie's name.
    pub const fn session(self) -> &'static str {
        if self.secure {
            SESSION_SECURE
        } else {
            SESSION_PLAIN
        }
    }

    /// The flow cookie's name.
    pub const fn flow(self) -> &'static str {
        if self.secure { FLOW_SECURE } else { FLOW_PLAIN }
    }

    /// A `Set-Cookie` value that sets `name` to `value` for `max_age`.
    pub fn set(self, name: &str, value: &str, max_age: Duration) -> String {
        let secure = if self.secure { "; Secure" } else { "" };
        format!(
            "{name}={value}; Max-Age={}; Path=/; HttpOnly; SameSite=Lax{secure}",
            max_age.as_secs()
        )
    }

    /// A `Set-Cookie` value that removes `name`.
    pub fn clear(self, name: &str) -> String {
        self.set(name, "", Duration::ZERO)
    }
}

/// The value of cookie `name` in the `Cookie` header values `headers`; the first one wins.
pub fn find<'a>(headers: impl IntoIterator<Item = &'a str>, name: &str) -> Option<&'a str> {
    headers
        .into_iter()
        .flat_map(|header| header.split(';'))
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| key.trim() == name)
        .map(|(_, value)| value.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_gives_host_prefixed_secure_cookies_and_plain_http_works() {
        let https = Cookies { secure: true };
        assert_eq!(
            https.set(https.session(), "abc", Duration::from_secs(60)),
            "__Host-crucible_session=abc; Max-Age=60; Path=/; HttpOnly; SameSite=Lax; Secure"
        );
        assert_eq!(https.flow(), "__Host-crucible_flow");
        let http = Cookies { secure: false };
        assert_eq!(
            http.set(http.session(), "abc", Duration::from_secs(60)),
            "crucible_session=abc; Max-Age=60; Path=/; HttpOnly; SameSite=Lax"
        );
        assert_eq!(
            http.clear(http.flow()),
            "crucible_flow=; Max-Age=0; Path=/; HttpOnly; SameSite=Lax"
        );
    }

    #[test]
    fn cookies_are_found_by_exact_name() {
        let headers = [
            "a=1; crucible_session_x=2",
            " crucible_session = 3 ; crucible_session=4",
        ];
        assert_eq!(find(headers, "crucible_session"), Some("3"));
        assert_eq!(find(headers, "a"), Some("1"));
        assert_eq!(find(headers, "b"), None);
        assert_eq!(find(["novalue; =x"], "novalue"), None);
    }
}
