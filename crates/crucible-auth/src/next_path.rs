//! Where to go after sign-in (contract 11.4). `next` must be a path on this server: it starts with a single `/`,
//! does not start with `//` or `/\`, holds no scheme, host, backslash or control character, and is not under
//! `/api/`. Anything else becomes `/`, so a crafted link can never send a signed-in browser elsewhere.

/// The longest `next` that is kept.
const MAX_LEN: usize = 2048;

/// `next` if it is a safe path on this server, `/` otherwise.
pub fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(path) if is_safe(path) => path.to_owned(),
        _ => "/".to_owned(),
    }
}

fn is_safe(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    path.len() <= MAX_LEN
        && path.starts_with('/')
        && !path.starts_with("//")
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && !lower.contains("://")
        && lower != "/api"
        && !lower.starts_with("/api/")
        && !lower.starts_with("/api?")
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn only_paths_on_this_server_are_kept() {
        for ok in [
            "/",
            "/traces/",
            "/traces/?project=x&from=1",
            "/trace/#span",
            "/apis/",
        ] {
            assert_eq!(safe_next(Some(ok)), ok, "{ok}");
        }
        for bad in [
            "",
            "traces",
            "//evil.example",
            "/\\evil.example",
            "/a\\b",
            "https://evil.example",
            "/redirect?to=https://evil.example",
            "/api",
            "/api/v1/auth/logout",
            "/API/v1/x",
            "/api?x",
            "/x\ny",
            "/x\u{7f}",
            " /traces/",
        ] {
            assert_eq!(safe_next(Some(bad)), "/", "{bad:?}");
        }
        assert_eq!(safe_next(None), "/");
        assert_eq!(safe_next(Some(&format!("/{}", "a".repeat(MAX_LEN)))), "/");
    }

    proptest! {
        /// Whatever is given, the result is a single-slash path outside `/api/` with no control character.
        #[test]
        fn the_result_is_always_a_local_path(next in ".{0,80}") {
            let safe = safe_next(Some(&next));
            prop_assert!(safe.starts_with('/') && !safe.starts_with("//"));
            prop_assert!(!safe.contains('\\') && !safe.chars().any(char::is_control));
            prop_assert!(!safe.to_ascii_lowercase().starts_with("/api/"));
            prop_assert!(safe == "/" || safe == next);
        }
    }
}
