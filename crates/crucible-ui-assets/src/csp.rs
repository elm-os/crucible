//! The per-page Content Security Policy.
//!
//! Each page of the export carries inline bootstrap scripts whose text differs per page, so each page may run
//! exactly the inline scripts it shipped with: their SHA-256 hashes, measured when the catalog is built.

use std::borrow::Cow;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use sha2::{Digest, Sha256};

/// The hash sources (`sha256-<base64>`) of the inline scripts in `html`, sorted and deduplicated.
pub(crate) fn script_hashes(html: &[u8]) -> Vec<String> {
    let mut hashes: Vec<String> = inline_scripts(html)
        .into_iter()
        .map(|script| {
            let digest = Sha256::digest(&*normalize_newlines(script));
            format!("sha256-{}", STANDARD.encode(digest))
        })
        .collect();
    hashes.sort();
    hashes.dedup();
    hashes
}

/// The policy for a page whose inline scripts hash to `hashes`:
///
/// - `script-src`: same-origin files plus the page's own inline scripts;
/// - `style-src-attr 'unsafe-inline'`: the UI SDK writes `style` attributes (custom properties such as
///   `--tree-level`); its style sheets are constructed in script, which CSP does not govern;
/// - `img-src data:`: the UI SDK style sheet draws its icons from `data:image/svg+xml` URLs;
/// - everything else same-origin, with no plugins, no `<base>` and no framing.
pub(crate) fn policy(hashes: &[String]) -> String {
    let mut script_src = String::from("'self'");
    for hash in hashes {
        script_src.push_str(" '");
        script_src.push_str(hash);
        script_src.push('\'');
    }
    format!(
        "default-src 'self'; script-src {script_src}; style-src-attr 'unsafe-inline'; \
         img-src 'self' data:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'"
    )
}

/// The text of each `<script>` element without a `src` attribute, in document order, delimited as the HTML
/// tokenizer delimits it: from the end of the start tag to the first `</script` followed by whitespace, `/`
/// or `>`. Comments are skipped. The export escapes `<` inside script text, so the tokenizer's escaped
/// script states never arise.
fn inline_scripts(html: &[u8]) -> Vec<&[u8]> {
    let mut scripts = Vec::new();
    let mut at = 0;
    while let Some(lt) = find(html, at, b"<") {
        let rest = &html[lt + 1..];
        if rest.starts_with(b"!--") {
            // Searching from the comment's own dashes also ends `<!-->` and `<!--->`, as the tokenizer does.
            at = find(html, lt + 2, b"-->").map_or(html.len(), |end| end + 3);
        } else if is_tag(rest, b"script") {
            let (has_src, start) = start_tag(html, lt + 1 + b"script".len());
            let end = end_tag(html, start);
            if !has_src {
                scripts.push(&html[start..end]);
            }
            at = end;
        } else {
            at = lt + 1;
        }
    }
    scripts
}

/// Whether `rest` (the text after `<` or `</`) starts with the tag `name`, in any case, followed by a byte
/// that ends a tag name.
fn is_tag(rest: &[u8], name: &[u8]) -> bool {
    rest.len() > name.len()
        && rest[..name.len()].eq_ignore_ascii_case(name)
        && (is_space(rest[name.len()]) || matches!(rest[name.len()], b'/' | b'>'))
}

/// Parses a start tag's attributes from `at`. Returns whether one is named `src`, and where the element's
/// content starts.
fn start_tag(html: &[u8], mut at: usize) -> (bool, usize) {
    let mut has_src = false;
    loop {
        while at < html.len() && (is_space(html[at]) || html[at] == b'/') {
            at += 1;
        }
        match html.get(at) {
            None => return (has_src, html.len()),
            Some(b'>') => return (has_src, at + 1),
            Some(_) => {}
        }
        // A name runs to whitespace, `/`, `>` or `=`; a `=` in first place belongs to the name.
        let name = at;
        at += 1;
        while at < html.len() && !(is_space(html[at]) || matches!(html[at], b'/' | b'>' | b'=')) {
            at += 1;
        }
        has_src |= html[name..at].eq_ignore_ascii_case(b"src");
        while at < html.len() && is_space(html[at]) {
            at += 1;
        }
        if html.get(at) != Some(&b'=') {
            continue;
        }
        at += 1;
        while at < html.len() && is_space(html[at]) {
            at += 1;
        }
        match html.get(at) {
            Some(&quote @ (b'"' | b'\'')) => {
                at = find(html, at + 1, &[quote]).map_or(html.len(), |end| end + 1);
            }
            _ => {
                while at < html.len() && !is_space(html[at]) && html[at] != b'>' {
                    at += 1;
                }
            }
        }
    }
}

/// Where the script element whose content starts at `from` ends: its `</script` end tag, or the end of input.
fn end_tag(html: &[u8], from: usize) -> usize {
    let mut at = from;
    while let Some(lt) = find(html, at, b"</") {
        if is_tag(&html[lt + 2..], b"script") {
            return lt;
        }
        at = lt + 2;
    }
    html.len()
}

fn find(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|position| from + position)
}

/// HTML whitespace: tab, line feed, form feed, carriage return and space.
fn is_space(byte: u8) -> bool {
    matches!(byte, b'\t' | b'\n' | 0x0c | b'\r' | b' ')
}

/// The text after the HTML input stream's newline normalization (CR LF and lone CR become LF), which happens
/// before a browser hashes a script.
fn normalize_newlines(text: &[u8]) -> Cow<'_, [u8]> {
    if !text.contains(&b'\r') {
        return Cow::Borrowed(text);
    }
    let mut out = Vec::with_capacity(text.len());
    let mut bytes = text.iter().copied().peekable();
    while let Some(byte) = bytes.next() {
        if byte == b'\r' {
            out.push(b'\n');
            bytes.next_if_eq(&b'\n');
        } else {
            out.push(byte);
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use rust_embed::RustEmbed;

    use super::*;

    #[derive(RustEmbed)]
    #[folder = "tests/fixtures/out/"]
    struct Fixture;

    fn sha(text: &[u8]) -> String {
        format!("sha256-{}", STANDARD.encode(Sha256::digest(text)))
    }

    /// The hashes were measured from the same files with a separate extractor (a regular expression over the
    /// decoded text, in Node.js), so this checks the tokenizer against an independent reading.
    #[test]
    fn fixture_pages_hash_as_an_independent_extractor_measured() {
        const BOOTSTRAP: [&str; 2] = [
            "sha256-ArxuZJNLynxg7sfbHnX7DrH+2qzQvkOO4u+/4ig+6eA=",
            "sha256-OBTN3RiyCV4Bq7dFqZ5a2pAXjnCcCYeTJMO2I/LYKeo=",
        ];
        for (page, own) in [
            (
                "index.html",
                "sha256-XpMBkZvnQvyRS9CMErcJePkZoqFhfBfZn7YLFeN+668=",
            ),
            (
                "home/index.html",
                "sha256-7UQjpfpJkM5PbtRgw6cuIa2ZuPJubCIw4j7fwbYrjAc=",
            ),
            (
                "traces/index.html",
                "sha256-XOivoHbUCoVCbfI+O8w3dah7bUTNOHI1X/EPOkt+qqY=",
            ),
            (
                "mcp/servers/index.html",
                "sha256-W9LIyPMUr9uf2lUMrSIZ3A637JDz2Zvc9vlogleciBI=",
            ),
            (
                "404.html",
                "sha256-tflqNlZ+nNGLf2x50GJ5lZKDxqyEutpicpx0JsUfFWs=",
            ),
        ] {
            let html = Fixture::get(page).unwrap().data;
            let mut expected = vec![BOOTSTRAP[0], BOOTSTRAP[1], own];
            expected.sort_unstable();
            assert_eq!(script_hashes(&html), expected, "{page}");
            // Every page has 13 external scripts, which must not count.
            assert_eq!(inline_scripts(&html).len(), 3, "{page}");
        }
    }

    #[test]
    fn the_tokenizer_cases_that_matter() {
        let html = concat!(
            "<!DOCTYPE html><SCRIPT type=\"module\">one</SCRIPT >",
            "<script src=\"/a.js\" async=\"\"></script>",
            "<script SRC=/b.js>ignored</script>",
            "<script data-src=\"x\" title='a > b'>two</script/>",
            "<!-- <script>commented</script> --><!--><script>three</script>",
            "<scripts>not a script</scripts><scriptx>nor this</scriptx>",
            "<script\tasync>four</scriptx></script\n>",
            "<script defer src>no value is still src</script>",
            "<script>five",
        );
        let scripts: Vec<&[u8]> = inline_scripts(html.as_bytes());
        assert_eq!(
            scripts,
            [&b"one"[..], b"two", b"three", b"four</scriptx>", b"five"]
        );
    }

    #[test]
    fn newlines_are_normalized_and_hashes_deduplicated() {
        let html = b"<script>a\r\nb\rc</script><script>a\nb\nc</script><script>z</script>";
        let mut expected = vec![sha(b"a\nb\nc"), sha(b"z")];
        expected.sort();
        assert_eq!(script_hashes(html), expected);
    }

    #[test]
    fn the_policy_lists_the_page_hashes() {
        let hashes = vec![sha(b"a"), sha(b"b")];
        assert_eq!(
            policy(&hashes),
            format!(
                "default-src 'self'; script-src 'self' '{}' '{}'; style-src-attr 'unsafe-inline'; \
                 img-src 'self' data:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'",
                hashes[0], hashes[1]
            )
        );
        assert!(policy(&[]).contains("script-src 'self';"));
    }

    #[derive(Clone, Debug)]
    enum Part {
        Inline { id: bool, body: String },
        External(String),
        Comment(String),
        Text(String),
    }

    fn part() -> impl Strategy<Value = Part> {
        // Script bodies have no `<`, so they cannot end early; comments have no `>`, so they cannot either.
        let body = "[a-zA-Z0-9 (){};=.\"'\r\n/>-]{0,40}";
        prop_oneof![
            (any::<bool>(), body).prop_map(|(id, body)| Part::Inline { id, body }),
            body.prop_map(Part::External),
            "[a-z !-]{0,20}".prop_map(Part::Comment),
            "[a-z >/=\"]{0,20}".prop_map(Part::Text),
        ]
    }

    proptest! {
        #[test]
        fn exactly_the_inline_script_bodies_are_found(parts in prop::collection::vec(part(), 0..12)) {
            let mut html = String::new();
            let mut bodies = Vec::new();
            for part in &parts {
                match part {
                    Part::Inline { id, body } => {
                        html.push_str(if *id { "<script id=\"_R_\">" } else { "<script>" });
                        html.push_str(body);
                        html.push_str("</script>");
                        bodies.push(body.as_bytes());
                    }
                    Part::External(body) => {
                        html.push_str(&format!("<script src=\"/c.js\">{body}</script>"));
                    }
                    Part::Comment(text) => {
                        html.push_str(&format!("<!--{text}<script>x</script>-->"));
                    }
                    Part::Text(text) => html.push_str(text),
                }
            }
            prop_assert_eq!(inline_scripts(html.as_bytes()), bodies.clone());
            let mut expected: Vec<String> =
                bodies.iter().map(|body| sha(&normalize_newlines(body))).collect();
            expected.sort();
            expected.dedup();
            prop_assert_eq!(script_hashes(html.as_bytes()), expected);
        }
    }
}
