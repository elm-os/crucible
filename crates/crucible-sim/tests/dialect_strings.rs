//! Dialect-specific strings live only in `crates/crucible-compat` (contracts 1.5 and 10).
//!
//! The strings are taken from the tables: every key, key prefix, scope name, event name, media token prefix,
//! route path and header name of dialects 1 to 7. The native ones are left out, because the contract names them
//! and any crate may: the keys of the `crucible.*` layer and of dialect 0, and every `gen_ai.*` and `crucible.*`
//! key. Bare keys that a table reads only on spans of its own dialect (`Scope::Own`) are generic words and are
//! left out too.
//!
//! The test reads every source file of the workspace outside `crates/crucible-compat` and fails when a string
//! literal in it is one of those strings, or starts with one of the prefixes. It lives here because only this
//! crate's tests may read the file system.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crucible_compat::{
    CRUCIBLE_KEYS, DIALECT_0, DetectOn, DialectTable, KeyMatch, Location, ROUTES, Scope, TABLES,
};

/// The namespaces the contract names: any crate may use their keys.
const NATIVE_NAMESPACES: [&str; 2] = ["gen_ai.", "crucible."];

/// Shorter strings are too generic to attribute to a dialect.
const MIN_LEN: usize = 6;

/// Source files that are scanned, by extension.
const EXTENSIONS: [&str; 7] = ["rs", "ts", "tsx", "js", "mjs", "py", "go"];

/// Directories that hold no source of this repository.
const SKIPPED_DIRS: [&str; 7] = [
    "target",
    "node_modules",
    ".git",
    ".next",
    "dist",
    "out",
    ".venv",
];

#[derive(Default)]
struct Strings {
    exact: BTreeSet<&'static str>,
    prefixes: BTreeSet<&'static str>,
}

impl Strings {
    fn add(&mut self, key: KeyMatch) {
        let text = key.text();
        if text.len() < MIN_LEN || NATIVE_NAMESPACES.iter().any(|n| text.starts_with(n)) {
            return;
        }
        match key {
            KeyMatch::Exact(_) => self.exact.insert(text),
            KeyMatch::Prefix(_) => self.prefixes.insert(text),
        };
    }

    fn add_table(&mut self, table: &DialectTable) {
        for rule in table.detect {
            match rule.on {
                DetectOn::ScopeName(name) | DetectOn::SpanKey(name) => self.add(name),
            }
        }
        for rule in table.observation_types {
            self.add(KeyMatch::Exact(rule.key));
        }
        for source in table.fields.iter().flat_map(|field| field.sources) {
            match source.location {
                Location::Event(name) | Location::EventTime(name) => {
                    self.add(KeyMatch::Exact(name));
                }
                Location::Span | Location::Resource => {}
            }
            let event_time = matches!(source.location, Location::EventTime(_));
            if source.scope == Scope::Any && !event_time {
                self.add(source.key);
            }
        }
        for token in table.media {
            self.add(KeyMatch::Prefix(token.prefix));
        }
    }

    /// The dialect string a literal holds, if any.
    fn find(&self, literal: &str) -> Option<&'static str> {
        self.exact.get(literal).copied().or_else(|| {
            self.prefixes
                .iter()
                .copied()
                .find(|prefix| literal.starts_with(prefix))
        })
    }
}

fn dialect_strings() -> Strings {
    let mut strings = Strings::default();
    for table in TABLES.iter().filter(|t| t.dialect != 0) {
        strings.add_table(table);
    }
    for route in &ROUTES {
        strings.add(KeyMatch::Exact(route.path));
        for header in [
            route.hint_header,
            route.sdk_name_header,
            route.sdk_version_header,
        ]
        .into_iter()
        .flatten()
        {
            strings.add(KeyMatch::Exact(header));
        }
    }
    // A key that a native table also reads is native.
    let mut native = Strings::default();
    for table in [&CRUCIBLE_KEYS, &DIALECT_0] {
        native.add_table(table);
    }
    strings.exact.retain(|key| !native.exact.contains(key));
    strings
        .prefixes
        .retain(|prefix| !native.prefixes.contains(prefix));
    strings
}

fn source_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if !SKIPPED_DIRS.contains(&&*name) {
                source_files(&path, out);
            }
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| EXTENSIONS.contains(&e))
        {
            out.push(path);
        }
    }
}

/// The string literals of a source text, roughly: the text between each pair of quote characters. A false
/// literal (the text between the end of one literal and the start of the next) can only add findings, never
/// hide one.
fn literals(text: &str) -> impl Iterator<Item = &str> {
    text.split(['"', '\'', '`'])
}

#[test]
fn the_tables_yield_dialect_strings_of_every_dialect() {
    let strings = dialect_strings();
    assert!(strings.exact.len() > 150, "{} keys", strings.exact.len());
    assert!(
        strings.prefixes.len() > 15,
        "{} prefixes",
        strings.prefixes.len()
    );
    // Native keys are not dialect strings, even where a dialect table repeats them.
    for key in ["session.id", "user.id", "service.version"] {
        assert!(strings.find(key).is_none(), "{key}");
    }
    for route in &ROUTES {
        assert_eq!(strings.find(route.path), Some(route.path));
    }
}

#[test]
fn dialect_strings_appear_only_in_the_compat_crate() {
    let strings = dialect_strings();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let compat = Path::new("crates").join("crucible-compat");
    let mut files = Vec::new();
    for dir in ["crates", "sdk", "xtask", "ui/src", "tools"] {
        source_files(&root.join(dir), &mut files);
    }
    assert!(files.len() > 200, "{} source files found", files.len());
    let mut findings = Vec::new();
    let mut scanned = 0;
    for path in files {
        let relative = path.strip_prefix(&root).unwrap_or(&path);
        if relative.starts_with(&compat) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        scanned += 1;
        for literal in literals(&text) {
            if let Some(found) = strings.find(literal) {
                findings.push(format!("{}: {found}", relative.display()));
            }
        }
    }
    assert!(scanned > 200, "{scanned} source files read");
    assert!(
        findings.is_empty(),
        "dialect strings outside crates/crucible-compat:\n{}",
        findings.join("\n")
    );
}
