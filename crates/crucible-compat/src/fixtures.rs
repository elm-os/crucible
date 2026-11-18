//! The compat fixtures (contract 10): one OTLP/JSON export request per file, with the fields the mapping must
//! give. Every value is invented; the shapes (keys, encodings, nesting and wire order) are the ones each SDK
//! sends, and each file names where its shape is documented under `provenance`.
//!
//! One file is `{ "provenance", "request": { "path", "headers" }, "otlp": <OTLP JSON export request>,
//! "expect": { "<span id hex>": { ...fields } } }`.

/// One fixture file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fixture {
    /// The dialect the fixture belongs to (its directory).
    pub dialect: u8,
    /// The file name without its extension.
    pub id: &'static str,
    /// The JSON text of the file.
    pub json: &'static str,
}

/// Every fixture, by dialect and then by name.
pub static FIXTURES: &[Fixture] = &[];
