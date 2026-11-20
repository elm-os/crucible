use std::sync::LazyLock;

use http::HeaderName;
use proptest::prelude::*;
use rust_embed::RustEmbed;

use super::*;
use crate::Catalog;

/// A minimal subset of a real export from a Linux build of the UI, which names segment payloads flat
/// (`home/__next.!KHNoZWxsKQ.home.__PAGE__.txt`). The `/traces/` payload sits under the nested name a Windows
/// build writes instead. Variants come from `cargo xtask ui compress`.
#[derive(RustEmbed)]
#[folder = "tests/fixtures/out/"]
struct Fixture;

static UI: LazyLock<Catalog> = LazyLock::new(Catalog::from_embed::<Fixture>);

fn file(path: &str) -> Bytes {
    Bytes::from(Fixture::get(path).unwrap().data.into_owned())
}

fn request(
    source: &dyn AssetSource,
    method: &Method,
    uri: &str,
    headers: &[(&str, &str)],
) -> Outcome {
    let uri = Uri::builder().path_and_query(uri).build().unwrap();
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        map.append(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    serve(source, method, &uri, &map, None)
}

fn response(
    source: &dyn AssetSource,
    method: &Method,
    uri: &str,
    headers: &[(&str, &str)],
) -> Response<Bytes> {
    match request(source, method, uri, headers) {
        Outcome::Response(response) => response,
        Outcome::Reserved => panic!("{method} {uri} was reserved"),
    }
}

fn get(source: &dyn AssetSource, uri: &str) -> Response<Bytes> {
    response(source, &Method::GET, uri, &[])
}

fn header(response: &Response<Bytes>, name: HeaderName) -> Option<&str> {
    response
        .headers()
        .get(name)
        .map(|value| value.to_str().unwrap())
}

#[test]
fn try_files_resolves_in_order() {
    for (uri, served) in [
        ("/", "index.html"),
        ("/index.html", "index.html"),
        ("/home/", "home/index.html"),
        ("/home", "home/index.html"),
        ("/home/?project=p1&range=7d", "home/index.html"),
        ("/mcp/servers/", "mcp/servers/index.html"),
        // RSC payloads; the query never takes part in resolution.
        (
            "/traces/index.txt?project=p1&range=24h&_rsc=1ld0r",
            "traces/index.txt",
        ),
        ("/traces.txt?_rsc=1ld0r", "traces/index.txt"),
        ("/index.txt?_rsc=x", "index.txt"),
        ("/__next.__PAGE__.txt", "__next.__PAGE__.txt"),
        ("/home/__next._tree.txt?_rsc=x", "home/__next._tree.txt"),
        // Linux layout: the flat name exists as asked.
        (
            "/home/__next.!KHNoZWxsKQ.home.__PAGE__.txt?_rsc=x",
            "home/__next.!KHNoZWxsKQ.home.__PAGE__.txt",
        ),
        // Windows layout: the flat name maps to the nested file, with `!` raw or escaped.
        (
            "/traces/__next.!KHNoZWxsKQ.traces.__PAGE__.txt?_rsc=x",
            "traces/__next.!KHNoZWxsKQ/traces/__PAGE__.txt",
        ),
        (
            "/traces/__next.%21KHNoZWxsKQ.traces.__PAGE__.txt",
            "traces/__next.!KHNoZWxsKQ/traces/__PAGE__.txt",
        ),
        (
            "/%5Fnext/static/chunks/0pnnrkjq50nr7.js",
            "_next/static/chunks/0pnnrkjq50nr7.js",
        ),
        (
            "/fonts/space-grotesk-OFL.txt",
            "fonts/space-grotesk-OFL.txt",
        ),
    ] {
        let response = get(&*UI, uri);
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        assert_eq!(response.body(), &file(served), "{uri}");
    }
}

#[test]
fn every_file_is_served_at_its_own_path() {
    let names: Vec<String> = Fixture::iter().map(|name| name.into_owned()).collect();
    for name in names
        .iter()
        .filter(|name| !name.ends_with(".br") && !name.ends_with(".gz"))
    {
        let response = get(&*UI, &format!("/{name}"));
        assert_eq!(response.status(), StatusCode::OK, "{name}");
        assert_eq!(response.body(), &file(name), "{name}");
    }
}

#[test]
fn misses_get_the_404_page_with_status_404() {
    for uri in [
        "/nope",
        "/nope/",
        "/traces/nope.txt",
        "/home/__next.!KHNoZWxsKQ.nope.__PAGE__.txt",
        // Variants are not addressable.
        "/index.html.br",
        "/index.html.gz",
        // Paths that cannot name a file.
        "/traces/../index.html",
        "/home%2F..%2Findex.html",
        "/traces/./",
        "/traces//",
        "//home/",
        "/a%5Cb",
        "/a%00b",
        "/%zz",
        "/%C3",
        // Not reserved: prefixes match whole segments.
        "/apiary",
        "/v10/traces",
        "/mcpx/",
        "/healthzz",
    ] {
        let response = get(&*UI, uri);
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(
            header(&response, CONTENT_TYPE),
            Some("text/html; charset=utf-8"),
            "{uri}"
        );
        assert_eq!(response.body(), &file("404.html"), "{uri}");
        assert_eq!(header(&response, ETAG), None, "{uri}");
        assert_eq!(header(&response, CACHE_CONTROL), Some("no-cache"), "{uri}");
    }
}

#[test]
fn reserved_paths_never_fall_back_to_html() {
    let methods = [
        Method::GET,
        Method::HEAD,
        Method::POST,
        Method::DELETE,
        Method::OPTIONS,
    ];
    for uri in [
        "/api",
        "/api/",
        "/api/v1/projects",
        "/api/v1/nope?x=1",
        "/api/%zz",
        "/%61pi/v1/x",
        "/v1",
        "/v1/traces",
        "/mcp",
        "/mcp/",
        "/mcp/nope",
        "/mcp/servers/nope/",
        "/healthz",
        "/healthz/",
        "/readyz",
        "/readyz/x",
    ] {
        for method in &methods {
            assert!(
                matches!(request(&*UI, method, uri, &[]), Outcome::Reserved),
                "{method} {uri}"
            );
            assert!(
                matches!(
                    request(&Catalog::default(), method, uri, &[]),
                    Outcome::Reserved
                ),
                "{method} {uri} without a UI"
            );
        }
    }
    // A file of the export below a reserved prefix is still served: the UI's MCP pages.
    let page = get(&*UI, "/mcp/servers/");
    assert_eq!(page.status(), StatusCode::OK);
    assert_eq!(page.body(), &file("mcp/servers/index.html"));
}

proptest! {
    #[test]
    fn nothing_below_a_reserved_prefix_falls_back(
        prefix in prop::sample::select(&RESERVED[..]),
        rest in "(/[a-z0-9._%!-]{0,10}){0,4}/?",
        query in "(\\?[a-z=&_]{0,10})?",
        method in prop::sample::select(vec![Method::GET, Method::HEAD, Method::POST]),
    ) {
        let uri = format!("{prefix}{rest}{query}");
        match request(&*UI, &method, &uri, &[]) {
            Outcome::Reserved => {}
            // Only a real file of the export answers here (the MCP pages), never the 404 page.
            Outcome::Response(response) => {
                prop_assert!(response.status().is_success(), "{} {}", method, uri);
                prop_assert!(uri.starts_with("/mcp/"), "{} {}", method, uri);
            }
        }
    }

    #[test]
    fn decoding_reverses_percent_encoding(segments in prop::collection::vec("[a-zA-Z0-9_!~()$-]{1,8}", 0..5)) {
        let path = format!("/{}", segments.join("/"));
        prop_assert_eq!(decode(&path), Some(path.clone()));
        let encoded: String = path
            .bytes()
            .map(|byte| if byte == b'/' { "/".to_owned() } else { format!("%{byte:02X}") })
            .collect();
        prop_assert_eq!(decode(&encoded), Some(path));
    }

    #[test]
    fn decoded_paths_never_climb(raw in "(/(\\.|\\.\\.|%2[eE]|%2[fF]|%5[cC]|[a-z])*){1,5}/?") {
        if let Some(path) = decode(&raw) {
            prop_assert!(path.starts_with('/'));
            prop_assert!(!path.contains('\\'));
            prop_assert!(path[1..].split('/').all(|segment| segment != "." && segment != ".."));
        }
    }
}

#[test]
fn head_sends_the_headers_of_get_without_a_body() {
    for (uri, headers) in [
        ("/traces/", &[][..]),
        ("/traces/", &[("accept-encoding", "br")][..]),
        ("/traces/", &[("if-none-match", "*")][..]),
        ("/nope/", &[][..]),
        (
            "/_next/static/chunks/0pnnrkjq50nr7.js",
            &[("accept-encoding", "gzip")][..],
        ),
    ] {
        let get = response(&*UI, &Method::GET, uri, headers);
        let head = response(&*UI, &Method::HEAD, uri, headers);
        assert_eq!(head.status(), get.status(), "{uri} {headers:?}");
        assert_eq!(head.headers(), get.headers(), "{uri} {headers:?}");
        assert!(head.body().is_empty(), "{uri} {headers:?}");
        if get.status() != StatusCode::NOT_MODIFIED {
            assert_eq!(
                header(&head, CONTENT_LENGTH),
                Some(get.body().len().to_string().as_str())
            );
        }
    }
    let head = response(&Catalog::default(), &Method::HEAD, "/", &[]);
    assert_eq!(head.status(), StatusCode::NOT_FOUND);
    assert!(head.body().is_empty());
    assert_eq!(
        header(&head, CONTENT_LENGTH),
        Some(NO_UI.len().to_string().as_str())
    );
}

#[test]
fn content_types() {
    for (uri, expected) in [
        ("/", "text/html; charset=utf-8"),
        ("/nope/", "text/html; charset=utf-8"),
        ("/traces/index.txt?_rsc=x", "text/plain; charset=utf-8"),
        (
            "/traces/__next.!KHNoZWxsKQ.traces.__PAGE__.txt",
            "text/plain; charset=utf-8",
        ),
        ("/home/__next._tree.txt", "text/plain; charset=utf-8"),
        ("/fonts/space-grotesk-OFL.txt", "text/plain; charset=utf-8"),
        (
            "/_next/static/chunks/0pnnrkjq50nr7.js",
            "text/javascript; charset=utf-8",
        ),
        (
            "/fonts/space-grotesk-latin-1.2ffba32413.woff2",
            "font/woff2",
        ),
    ] {
        assert_eq!(
            header(&get(&*UI, uri), CONTENT_TYPE),
            Some(expected),
            "{uri}"
        );
    }
}

#[test]
fn hashed_build_output_is_immutable_and_the_rest_revalidates() {
    for uri in [
        "/_next/static/chunks/0pnnrkjq50nr7.js",
        "/_next/static/07_El3sNkZ2AOTCUxcqke/_ssgManifest.js",
    ] {
        assert_eq!(
            header(&get(&*UI, uri), CACHE_CONTROL),
            Some("public, max-age=31536000, immutable"),
            "{uri}"
        );
    }
    for uri in [
        "/",
        "/traces/",
        "/traces/index.txt",
        "/home/__next._tree.txt",
        "/fonts/space-grotesk-latin-1.2ffba32413.woff2",
    ] {
        assert_eq!(
            header(&get(&*UI, uri), CACHE_CONTROL),
            Some("no-cache"),
            "{uri}"
        );
    }
}

#[test]
fn etags_and_conditional_requests() {
    let etag = header(&get(&*UI, "/traces/"), ETAG).unwrap().to_owned();
    assert_eq!(etag, UI.get("traces/index.html").unwrap().etag);
    for if_none_match in [
        etag.clone(),
        format!("W/{etag}"),
        format!("\"0123\", {etag}"),
        "*".to_owned(),
    ] {
        for method in [Method::GET, Method::HEAD] {
            let response = response(
                &*UI,
                &method,
                "/traces/",
                &[("if-none-match", &if_none_match)],
            );
            assert_eq!(
                response.status(),
                StatusCode::NOT_MODIFIED,
                "{if_none_match}"
            );
            assert!(response.body().is_empty());
            assert_eq!(header(&response, ETAG), Some(etag.as_str()));
            assert_eq!(header(&response, CACHE_CONTROL), Some("no-cache"));
            assert_eq!(header(&response, VARY), Some("accept-encoding"));
            assert_eq!(header(&response, CONTENT_LENGTH), None);
        }
    }
    let changed = response(
        &*UI,
        &Method::GET,
        "/traces/",
        &[("if-none-match", "\"0123\"")],
    );
    assert_eq!(changed.status(), StatusCode::OK);
    assert_eq!(changed.body(), &file("traces/index.html"));

    // Each coding is its own representation with its own strong tag.
    let br = response(&*UI, &Method::GET, "/traces/", &[("accept-encoding", "br")]);
    let gzip = response(
        &*UI,
        &Method::GET,
        "/traces/",
        &[("accept-encoding", "gzip")],
    );
    let br_etag = header(&br, ETAG).unwrap();
    assert_eq!(br_etag, format!("{}-br\"", &etag[..etag.len() - 1]));
    assert_eq!(
        header(&gzip, ETAG).unwrap(),
        format!("{}-gzip\"", &etag[..etag.len() - 1])
    );
    let revalidated = response(
        &*UI,
        &Method::GET,
        "/traces/",
        &[("accept-encoding", "br"), ("if-none-match", br_etag)],
    );
    assert_eq!(revalidated.status(), StatusCode::NOT_MODIFIED);
    let other_coding = response(
        &*UI,
        &Method::GET,
        "/traces/",
        &[("if-none-match", br_etag)],
    );
    assert_eq!(other_coding.status(), StatusCode::OK);

    // The 404 page is never conditional.
    let miss = response(&*UI, &Method::GET, "/nope/", &[("if-none-match", "*")]);
    assert_eq!(miss.status(), StatusCode::NOT_FOUND);
    assert_eq!(miss.body(), &file("404.html"));
}

#[test]
fn encoding_negotiation_picks_the_best_acceptable_variant() {
    for (accept, coding) in [
        ("gzip, deflate, br, zstd", Some("br")),
        ("br", Some("br")),
        ("BR;q=0.5", Some("br")),
        ("gzip", Some("gzip")),
        ("x-gzip", Some("gzip")),
        ("gzip;q=1.0, br;q=0.5", Some("gzip")),
        ("br;q=0, gzip", Some("gzip")),
        ("*", Some("br")),
        ("*;q=0.5, br;q=0", Some("gzip")),
        ("gzip;q=0.5, identity;q=0.5", Some("gzip")),
        ("identity;q=1, br;q=0.5", None),
        ("identity", None),
        ("deflate", None),
        ("", None),
        ("br;q=0, gzip;q=0", None),
        // A malformed quality voids its item.
        ("br;q=2", None),
        ("br;q=0.0001", None),
    ] {
        let response = response(
            &*UI,
            &Method::GET,
            "/traces/",
            &[("accept-encoding", accept)],
        );
        assert_eq!(header(&response, CONTENT_ENCODING), coding, "{accept:?}");
        let served = match coding {
            Some("br") => "traces/index.html.br",
            Some(_) => "traces/index.html.gz",
            None => "traces/index.html",
        };
        assert_eq!(response.body(), &file(served), "{accept:?}");
        assert_eq!(
            header(&response, VARY),
            Some("accept-encoding"),
            "{accept:?}"
        );
    }
    let plain = get(&*UI, "/traces/");
    assert_eq!(header(&plain, CONTENT_ENCODING), None);
    assert_eq!(header(&plain, VARY), Some("accept-encoding"));

    // Only a brotli variant exists for this file: gzip was not smaller.
    let tree = |accept| {
        response(
            &*UI,
            &Method::GET,
            "/__next._tree.txt",
            &[("accept-encoding", accept)],
        )
    };
    assert_eq!(header(&tree("gzip"), CONTENT_ENCODING), None);
    assert_eq!(header(&tree("gzip, br"), CONTENT_ENCODING), Some("br"));
    assert_eq!(tree("gzip, br").body(), &file("__next._tree.txt.br"));

    // A font has no variants, so nothing varies.
    let font = response(
        &*UI,
        &Method::GET,
        "/fonts/space-grotesk-latin-1.2ffba32413.woff2",
        &[("accept-encoding", "br, gzip")],
    );
    assert_eq!(header(&font, CONTENT_ENCODING), None);
    assert_eq!(header(&font, VARY), None);
}

proptest! {
    #[test]
    fn negotiation_never_picks_an_absent_or_unrequested_variant(
        items in prop::collection::vec(
            (prop::sample::select(vec!["br", "gzip", "identity", "*", "deflate", "zstd"]),
             prop::option::of(prop::sample::select(vec!["0", "0.1", "0.5", "1", "1.0", "2", "x"]))),
            0..5,
        ),
        has_br in any::<bool>(),
        has_gzip in any::<bool>(),
    ) {
        let header: Vec<String> = items
            .iter()
            .map(|(coding, q)| q.map_or_else(|| (*coding).to_owned(), |q| format!("{coding};q={q}")))
            .collect();
        let header = header.join(", ");
        let mut asset = UI.get("traces/index.html").unwrap();
        if !has_br {
            asset.encodings.br = None;
        }
        if !has_gzip {
            asset.encodings.gzip = None;
        }
        let mut request = HeaderMap::new();
        request.insert(ACCEPT_ENCODING, HeaderValue::from_str(&header).unwrap());
        let coding = negotiate(&asset, &request);
        let named = |name: &str| items.iter().any(|(coding, _)| *coding == name || *coding == "*");
        match coding {
            Coding::Br => prop_assert!(has_br && named("br"), "{}", header),
            Coding::Gzip => prop_assert!(has_gzip && named("gzip"), "{}", header),
            Coding::Identity => {}
        }
        prop_assert_eq!(negotiate(&asset, &HeaderMap::new()), Coding::Identity);
    }
}

#[test]
fn each_page_carries_its_own_script_hashes() {
    let manifest = UI.manifest();
    for (uri, page) in [
        ("/", "index.html"),
        ("/home/", "home/index.html"),
        ("/traces", "traces/index.html"),
        ("/mcp/servers/", "mcp/servers/index.html"),
        ("/nope/", "404.html"),
    ] {
        for accept in ["", "br"] {
            let response = response(&*UI, &Method::GET, uri, &[("accept-encoding", accept)]);
            let policy = header(&response, CONTENT_SECURITY_POLICY).unwrap();
            let hashes = &manifest.csp_hashes[page];
            assert_eq!(policy, csp::policy(hashes, None), "{uri}");
            for hash in hashes {
                assert!(policy.contains(&format!("'{hash}'")), "{uri}: {hash}");
            }
        }
    }
    // Two bootstrap scripts are the same on every page; the third is the page's own.
    let home = &manifest.csp_hashes["home/index.html"];
    let traces = &manifest.csp_hashes["traces/index.html"];
    assert_eq!(home.iter().filter(|hash| traces.contains(hash)).count(), 2);
    for uri in [
        "/traces/index.txt",
        "/_next/static/chunks/0pnnrkjq50nr7.js",
        "/fonts/space-grotesk-latin-1.2ffba32413.woff2",
    ] {
        assert_eq!(
            header(&get(&*UI, uri), CONTENT_SECURITY_POLICY),
            None,
            "{uri}"
        );
    }
}

#[test]
fn without_a_ui_every_page_explains_how_to_build_one() {
    let empty = Catalog::default();
    for uri in ["/", "/traces/", "/_next/static/chunks/a.js"] {
        let response = get(&empty, uri);
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(
            header(&response, CONTENT_TYPE),
            Some("text/plain; charset=utf-8")
        );
        assert_eq!(response.body(), NO_UI.as_bytes());
    }
    assert!(NO_UI.contains("embed-ui") && NO_UI.contains("--ui-dir"));
    // An export without a 404 page answers a miss with a short text.
    let partial =
        Catalog::from_files([("index.html".to_owned(), Bytes::from_static(b"<p>hi</p>"))]);
    let miss = get(&partial, "/nope/");
    assert_eq!(miss.status(), StatusCode::NOT_FOUND);
    assert_eq!(miss.body(), &b"Not found.\n"[..]);
}

#[test]
fn methods_other_than_get_and_head_are_refused() {
    for method in [
        Method::POST,
        Method::PUT,
        Method::DELETE,
        Method::PATCH,
        Method::OPTIONS,
    ] {
        let response = response(&*UI, &method, "/traces/", &[]);
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method}"
        );
        assert_eq!(header(&response, ALLOW), Some("GET, HEAD"));
        assert!(response.body().is_empty());
    }
}
