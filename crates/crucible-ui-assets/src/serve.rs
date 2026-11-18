//! The try-files handler (contract 12.1).

use bytes::Bytes;
use crucible_env::{Asset, AssetSource, Encodings};
use http::header::{
    ACCEPT_ENCODING, ALLOW, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH,
    CONTENT_SECURITY_POLICY, CONTENT_TYPE, ETAG, IF_NONE_MATCH, VARY,
};
use http::{HeaderMap, HeaderValue, Method, Response, StatusCode, Uri};

use crate::csp;

/// What [`serve`] decided.
#[derive(Debug)]
pub enum Outcome {
    /// The UI's answer: a file, the 404 page, or 405 for a method other than GET and HEAD.
    Response(Response<Bytes>),
    /// The path belongs to the API, OTLP ingest, the MCP server or a health check and names no file of the
    /// export. These paths never fall back to HTML; the caller answers with a problem 404.
    Reserved,
}

/// Path prefixes that never fall back to HTML, each matched as a whole segment: `/api` covers `/api` and
/// `/api/...` but not `/apiary`. A file of the export below one of them is still served; the UI's MCP pages
/// live under `/mcp/`.
const RESERVED: [&str; 5] = ["/api", "/v1", "/mcp", "/healthz", "/readyz"];
/// The export's 404 page.
const NOT_FOUND_PAGE: &str = "404.html";
/// Content-hashed build output, cacheable for good.
const IMMUTABLE: &str = "_next/static/";
/// The answer for every page when the source is empty.
const NO_UI: &str = "This build of Crucible has no web UI. Build the UI and compile with the embed-ui \
                     feature, or serve a built UI with `crucible serve --ui-dir <dir>`. \
                     docs/development.md has the steps.\n";

/// Answers a request that no other route matched.
///
/// Resolution ignores the query string and tries, in order:
/// 1. the file the path names;
/// 2. the route's page, `R/index.html` for `/R/` and for `/R`;
/// 3. for a `.txt` path, the RSC payload under the name the export gave it: the nested name a Windows build
///    writes for a segment payload the client asks for by its flat name (`R/__next.a.b.__PAGE__.txt` is
///    `R/__next.a/b/__PAGE__.txt`), and `R/index.txt` for `/R.txt`;
/// 4. the 404 page, with status 404, except under the reserved prefixes ([`Outcome::Reserved`]).
pub fn serve(source: &dyn AssetSource, method: &Method, uri: &Uri, headers: &HeaderMap) -> Outcome {
    let path = decode(uri.path());
    let reserved = is_reserved(path.as_deref().unwrap_or(uri.path()));
    let head = method == Method::HEAD;
    if !head && method != Method::GET {
        return if reserved {
            Outcome::Reserved
        } else {
            Outcome::Response(method_not_allowed())
        };
    }
    match path.as_deref().and_then(|path| resolve(source, path)) {
        Some((name, asset)) => Outcome::Response(respond(
            source,
            &name,
            &asset,
            StatusCode::OK,
            headers,
            head,
        )),
        None if reserved => Outcome::Reserved,
        None => Outcome::Response(not_found(source, headers, head)),
    }
}

/// The path percent-decoded, if it can name a file: it starts with `/`, is UTF-8, and has no `.` or `..`
/// segment, no empty segment except after a final `/`, and no `\` or NUL.
fn decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while let Some(&byte) = bytes.get(at) {
        if byte == b'%' {
            let digits = bytes.get(at + 1..at + 3)?;
            let high = char::from(digits[0]).to_digit(16)?;
            let low = char::from(digits[1]).to_digit(16)?;
            decoded.push(u8::try_from(high * 16 + low).ok()?);
            at += 3;
        } else {
            decoded.push(byte);
            at += 1;
        }
    }
    let path = String::from_utf8(decoded).ok()?;
    let segments: Vec<&str> = path.strip_prefix('/')?.split('/').collect();
    let last = segments.len() - 1;
    let valid = segments.iter().enumerate().all(|(n, segment)| {
        (n == last || !segment.is_empty())
            && *segment != "."
            && *segment != ".."
            && !segment.contains(['\\', '\0'])
    });
    valid.then_some(path)
}

fn is_reserved(path: &str) -> bool {
    RESERVED.iter().any(|prefix| {
        path.strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    })
}

/// The try-files order of [`serve`], steps 1 to 3, for a decoded path.
fn resolve(source: &dyn AssetSource, path: &str) -> Option<(String, Asset)> {
    let rel = path.strip_prefix('/').unwrap_or(path);
    let page = if rel.is_empty() || rel.ends_with('/') {
        format!("{rel}index.html")
    } else {
        format!("{rel}/index.html")
    };
    let mut candidates = vec![rel.to_owned(), page];
    if let Some(stem) = rel.strip_suffix(".txt") {
        candidates.extend(nested_segment(rel));
        candidates.push(format!("{stem}/index.txt"));
    }
    candidates
        .into_iter()
        .find_map(|name| source.get(&name).map(|asset| (name, asset)))
}

/// The nested name of a segment payload requested by its flat name: `R/__next.a.b.__PAGE__.txt` becomes
/// `R/__next.a/b/__PAGE__.txt`. Segment names never contain `.`, so the mapping is exact.
fn nested_segment(rel: &str) -> Option<String> {
    let (dir, file) = match rel.rsplit_once('/') {
        Some((dir, file)) => (Some(dir), file),
        None => (None, rel),
    };
    let segments = file.strip_prefix("__next.")?.strip_suffix(".txt")?;
    let nested = format!("__next.{}.txt", segments.replace('.', "/"));
    Some(match dir {
        Some(dir) => format!("{dir}/{nested}"),
        None => nested,
    })
}

/// A content coding the response can use.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Coding {
    Br,
    Gzip,
    Identity,
}

/// The coding for `asset` by `Accept-Encoding` (RFC 9110, 12.5.3): the acceptable variant with the highest
/// quality; brotli, then gzip, then identity on a tie. Without the header, identity.
fn negotiate(asset: &Asset, request: &HeaderMap) -> Coding {
    let (mut br, mut gzip, mut identity, mut any) = (None, None, None, None);
    let items = request
        .get_all(ACCEPT_ENCODING)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','));
    for item in items {
        let mut params = item.split(';');
        let coding = params.next().unwrap_or_default().trim();
        let quality = params.find_map(|param| {
            let (name, value) = param.split_once('=')?;
            name.trim()
                .eq_ignore_ascii_case("q")
                .then(|| qvalue(value.trim()))
        });
        // A malformed quality makes the whole item unusable.
        let Some(quality) = quality.unwrap_or(Some(1000)) else {
            continue;
        };
        match coding.to_ascii_lowercase().as_str() {
            "br" => br = Some(quality),
            "gzip" | "x-gzip" => gzip = Some(quality),
            "identity" => identity = Some(quality),
            "*" => any = Some(quality),
            _ => {}
        }
    }
    // A coding not listed takes the `*` quality; identity stays acceptable, at the lowest preference, unless
    // excluded.
    let weight = |listed: Option<u16>, available: bool| {
        if available {
            listed.or(any).unwrap_or(0)
        } else {
            0
        }
    };
    let br = weight(br, asset.encodings.br.is_some());
    let gzip = weight(gzip, asset.encodings.gzip.is_some());
    let identity = identity.or(any).unwrap_or(1);
    if br > 0 && br >= gzip && br >= identity {
        Coding::Br
    } else if gzip > 0 && gzip >= identity {
        Coding::Gzip
    } else {
        Coding::Identity
    }
}

/// A quality value in thousandths: `0`, `0.5`, `1`, `1.000`.
fn qvalue(text: &str) -> Option<u16> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    if fraction.len() > 3 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let thousandths: u16 = format!("{fraction:0<3}").parse().ok()?;
    match whole {
        "0" => Some(thousandths),
        "1" if thousandths == 0 => Some(1000),
        _ => None,
    }
}

/// The strong ETag of a variant: the file's tag with the coding appended inside the quotes.
fn variant_etag(etag: &str, coding: &str) -> String {
    match etag.strip_suffix('"') {
        Some(open) => format!("{open}-{coding}\""),
        None => format!("{etag}-{coding}"),
    }
}

/// Whether `If-None-Match` matches `etag` under the weak comparison (RFC 9110, 13.1.2).
fn not_modified(request: &HeaderMap, etag: &str) -> bool {
    let tag = etag.strip_prefix("W/").unwrap_or(etag);
    request
        .get_all(IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|candidate| {
            candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == tag
        })
}

fn insert(headers: &mut HeaderMap, name: http::HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

/// The response carrying `asset`, the file named `name`, with `status`. Only a 200 is conditional.
fn respond(
    source: &dyn AssetSource,
    name: &str,
    asset: &Asset,
    status: StatusCode,
    request: &HeaderMap,
    head: bool,
) -> Response<Bytes> {
    let (bytes, etag, coding) = match (negotiate(asset, request), &asset.encodings) {
        (Coding::Br, Encodings { br: Some(br), .. }) => {
            (br.clone(), variant_etag(&asset.etag, "br"), Some("br"))
        }
        (
            Coding::Gzip,
            Encodings {
                gzip: Some(gzip), ..
            },
        ) => (
            gzip.clone(),
            variant_etag(&asset.etag, "gzip"),
            Some("gzip"),
        ),
        _ => (asset.bytes.clone(), asset.etag.clone(), None),
    };
    let mut response = Response::new(Bytes::new());
    let headers = response.headers_mut();
    headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static(if name.starts_with(IMMUTABLE) {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        }),
    );
    if asset.encodings.br.is_some() || asset.encodings.gzip.is_some() {
        headers.insert(VARY, HeaderValue::from_static("accept-encoding"));
    }
    if status == StatusCode::OK {
        insert(headers, ETAG, &etag);
        if not_modified(request, &etag) {
            *response.status_mut() = StatusCode::NOT_MODIFIED;
            return response;
        }
    }
    insert(headers, CONTENT_TYPE, asset.content_type);
    if let Some(coding) = coding {
        headers.insert(CONTENT_ENCODING, HeaderValue::from_static(coding));
    }
    if asset.content_type.starts_with("text/html") {
        let hashes = source.manifest().csp_hashes.get(name);
        let policy = csp::policy(hashes.map_or(&[], Vec::as_slice));
        insert(headers, CONTENT_SECURITY_POLICY, &policy);
    }
    headers.insert(CONTENT_LENGTH, HeaderValue::from(bytes.len()));
    *response.status_mut() = status;
    if !head {
        *response.body_mut() = bytes;
    }
    response
}

/// The 404 page with status 404, or a short text when the source has no 404 page.
fn not_found(source: &dyn AssetSource, request: &HeaderMap, head: bool) -> Response<Bytes> {
    if let Some(page) = source.get(NOT_FOUND_PAGE) {
        return respond(
            source,
            NOT_FOUND_PAGE,
            &page,
            StatusCode::NOT_FOUND,
            request,
            head,
        );
    }
    let text = if source.get("index.html").is_none() {
        NO_UI
    } else {
        "Not found.\n"
    };
    let mut response = Response::new(if head {
        Bytes::new()
    } else {
        Bytes::from_static(text.as_bytes())
    });
    *response.status_mut() = StatusCode::NOT_FOUND;
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(CONTENT_LENGTH, HeaderValue::from(text.len()));
    response
}

fn method_not_allowed() -> Response<Bytes> {
    let mut response = Response::new(Bytes::new());
    *response.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
    response
        .headers_mut()
        .insert(ALLOW, HeaderValue::from_static("GET, HEAD"));
    response
}

#[cfg(test)]
mod tests;
