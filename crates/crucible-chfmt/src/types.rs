//! Parsing of `TableDef` type strings into wire layouts and block-header type strings.

use std::ops::Range;

/// The layout a column's values take in a Native block.
///
/// `Date`, `DateTime`, `DateTime64` and `Enum8` collapse into their storage integers, and the `LowCardinality` and
/// `SimpleAggregateFunction` wrappers into the type they wrap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireType {
    UInt8,
    UInt16,
    UInt32,
    UInt64,
    Int8,
    Int32,
    Int64,
    Float32,
    Float64,
    Uuid,
    String,
    FixedString(usize),
    Nullable(Box<WireType>),
    Array(Box<WireType>),
    Tuple(Vec<WireType>),
}

/// A parsed type string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedType {
    /// Layout of the values on the wire.
    pub wire: WireType,
    /// Type string for the block header: the input with every `LowCardinality(String)` replaced by `String`.
    pub header: String,
}

/// Parses a type string. Returns `None` for malformed strings and for types that section 6.4 does not use.
pub fn parse(ty: &str) -> Option<ParsedType> {
    let mut parser = Parser {
        src: ty,
        pos: 0,
        cuts: Vec::new(),
    };
    let wire = parser.ty()?;
    parser.skip_ws();
    if parser.pos != ty.len() {
        return None;
    }
    parser.cuts.sort_by_key(|cut| cut.start);
    let mut header = String::with_capacity(ty.len());
    let mut at = 0;
    for cut in &parser.cuts {
        header.push_str(ty.get(at..cut.start)?);
        at = cut.end;
    }
    header.push_str(ty.get(at..)?);
    Some(ParsedType {
        wire,
        header: header.trim().to_owned(),
    })
}

struct Parser<'a> {
    src: &'a str,
    pos: usize,
    /// Byte ranges of `LowCardinality(` and its closing parenthesis, removed from the header.
    cuts: Vec<Range<usize>>,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.src.as_bytes().get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(|b| b.is_ascii_whitespace()) {
            self.pos += 1;
        }
    }

    /// Consumes `byte` after optional whitespace.
    fn eat(&mut self, byte: u8) -> Option<()> {
        self.skip_ws();
        (self.peek() == Some(byte)).then(|| self.pos += 1)
    }

    fn ident(&mut self) -> Option<&'a str> {
        self.skip_ws();
        let start = self.pos;
        if !self
            .peek()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        {
            return None;
        }
        while self
            .peek()
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            self.pos += 1;
        }
        self.src.get(start..self.pos)
    }

    fn integer(&mut self) -> Option<i64> {
        self.skip_ws();
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.pos += 1;
        }
        self.src.get(start..self.pos)?.parse().ok()
    }

    /// A single-quoted string literal with backslash escapes; returns its raw contents.
    fn quoted(&mut self) -> Option<&'a str> {
        self.eat(b'\'')?;
        let start = self.pos;
        loop {
            match self.peek()? {
                b'\\' => self.pos += 2,
                b'\'' => break,
                _ => self.pos += 1,
            }
        }
        let contents = self.src.get(start..self.pos)?;
        self.pos += 1;
        Some(contents)
    }

    fn utc(&mut self) -> Option<()> {
        (self.quoted()? == "UTC").then_some(())
    }

    fn ty(&mut self) -> Option<WireType> {
        let name = self.ident()?;
        let start = self.pos - name.len();
        Some(match name {
            "UInt8" => WireType::UInt8,
            "UInt16" | "Date" => WireType::UInt16,
            "UInt32" => WireType::UInt32,
            "UInt64" => WireType::UInt64,
            "Int32" => WireType::Int32,
            "Int64" => WireType::Int64,
            "Float32" => WireType::Float32,
            "Float64" => WireType::Float64,
            "UUID" => WireType::Uuid,
            "String" => WireType::String,
            "FixedString" => {
                self.eat(b'(')?;
                let n = usize::try_from(self.integer()?).ok().filter(|&n| n > 0)?;
                self.eat(b')')?;
                WireType::FixedString(n)
            }
            "DateTime" => {
                if self.eat(b'(').is_some() {
                    self.utc()?;
                    self.eat(b')')?;
                }
                WireType::UInt32
            }
            "DateTime64" => {
                self.eat(b'(')?;
                (self.integer()? == 6).then_some(())?;
                self.eat(b',')?;
                self.utc()?;
                self.eat(b')')?;
                WireType::Int64
            }
            "Enum8" => {
                self.eat(b'(')?;
                loop {
                    self.quoted()?;
                    self.eat(b'=')?;
                    i8::try_from(self.integer()?).ok()?;
                    if self.eat(b',').is_none() {
                        break;
                    }
                }
                self.eat(b')')?;
                WireType::Int8
            }
            "Nullable" => {
                self.eat(b'(')?;
                let inner = self.ty()?;
                self.eat(b')')?;
                WireType::Nullable(Box::new(inner))
            }
            "Array" => {
                self.eat(b'(')?;
                let inner = self.ty()?;
                self.eat(b')')?;
                WireType::Array(Box::new(inner))
            }
            "Tuple" => {
                self.eat(b'(')?;
                let mut elements = vec![self.ty()?];
                while self.eat(b',').is_some() {
                    elements.push(self.ty()?);
                }
                self.eat(b')')?;
                WireType::Tuple(elements)
            }
            "LowCardinality" => {
                self.eat(b'(')?;
                self.skip_ws();
                let inner_start = self.pos;
                let inner = self.ty()?;
                let inner_end = self.pos;
                self.eat(b')')?;
                if inner != WireType::String {
                    return None;
                }
                self.cuts.push(start..inner_start);
                self.cuts.push(inner_end..self.pos);
                inner
            }
            "SimpleAggregateFunction" => {
                self.eat(b'(')?;
                self.ident()?;
                if self.eat(b'(').is_some() {
                    loop {
                        self.integer()?;
                        if self.eat(b',').is_none() {
                            break;
                        }
                    }
                    self.eat(b')')?;
                }
                self.eat(b',')?;
                let inner = self.ty()?;
                self.eat(b')')?;
                inner
            }
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{WireType as W, parse};

    fn wire(ty: &str) -> W {
        parse(ty)
            .unwrap_or_else(|| panic!("`{ty}` should parse"))
            .wire
    }

    fn header(ty: &str) -> String {
        parse(ty)
            .unwrap_or_else(|| panic!("`{ty}` should parse"))
            .header
    }

    fn arr(inner: W) -> W {
        W::Array(Box::new(inner))
    }

    #[test]
    fn every_section_6_4_type_parses_to_its_wire_layout() {
        let cases = [
            ("UInt8", W::UInt8),
            ("UInt16", W::UInt16),
            ("UInt32", W::UInt32),
            ("UInt64", W::UInt64),
            ("Int64", W::Int64),
            ("Float32", W::Float32),
            ("Float64", W::Float64),
            ("UUID", W::Uuid),
            ("String", W::String),
            ("FixedString(8)", W::FixedString(8)),
            ("FixedString(16)", W::FixedString(16)),
            ("FixedString(32)", W::FixedString(32)),
            ("Date", W::UInt16),
            ("DateTime('UTC')", W::UInt32),
            ("DateTime", W::UInt32),
            ("DateTime64(6, 'UTC')", W::Int64),
            ("Enum8('unset' = 0, 'ok' = 1, 'error' = 2)", W::Int8),
            ("LowCardinality(String)", W::String),
            (
                "Nullable(DateTime64(6, 'UTC'))",
                W::Nullable(Box::new(W::Int64)),
            ),
            ("Nullable(UInt32)", W::Nullable(Box::new(W::UInt32))),
            ("Nullable(Int32)", W::Nullable(Box::new(W::Int32))),
            ("Nullable(UInt16)", W::Nullable(Box::new(W::UInt16))),
            ("Array(String)", arr(W::String)),
            ("Array(LowCardinality(String))", arr(W::String)),
            ("Array(UInt8)", arr(W::UInt8)),
            ("Array(UInt32)", arr(W::UInt32)),
            ("Array(UInt64)", arr(W::UInt64)),
            ("Array(Int64)", arr(W::Int64)),
            (
                "Tuple(Array(UInt16), Array(UInt64))",
                W::Tuple(vec![arr(W::UInt16), arr(W::UInt64)]),
            ),
            (
                "Tuple(Array(UInt8), Array(UInt64))",
                W::Tuple(vec![arr(W::UInt8), arr(W::UInt64)]),
            ),
            ("SimpleAggregateFunction(max, DateTime('UTC'))", W::UInt32),
            (
                "SimpleAggregateFunction(min, DateTime64(6, 'UTC'))",
                W::Int64,
            ),
            (
                "SimpleAggregateFunction(max, DateTime64(6, 'UTC'))",
                W::Int64,
            ),
            ("SimpleAggregateFunction(max, UInt8)", W::UInt8),
            ("SimpleAggregateFunction(sum, UInt64)", W::UInt64),
            ("SimpleAggregateFunction(sum, Int64)", W::Int64),
            ("SimpleAggregateFunction(max, String)", W::String),
            (
                "SimpleAggregateFunction(groupUniqArrayArray(16), Array(String))",
                arr(W::String),
            ),
            (
                "SimpleAggregateFunction(groupUniqArrayArray(8), Array(String))",
                arr(W::String),
            ),
            (
                "SimpleAggregateFunction(groupUniqArrayArray(4), Array(UInt8))",
                arr(W::UInt8),
            ),
            (
                "SimpleAggregateFunction(sumMap, Tuple(Array(UInt16), Array(UInt64)))",
                W::Tuple(vec![arr(W::UInt16), arr(W::UInt64)]),
            ),
            (
                "SimpleAggregateFunction(sumMap, Tuple(Array(UInt8), Array(UInt64)))",
                W::Tuple(vec![arr(W::UInt8), arr(W::UInt64)]),
            ),
        ];
        for (ty, expected) in cases {
            assert_eq!(wire(ty), expected, "{ty}");
        }
    }

    #[test]
    fn header_is_the_type_string_with_low_cardinality_unwrapped() {
        assert_eq!(header("LowCardinality(String)"), "String");
        assert_eq!(header("Array(LowCardinality(String))"), "Array(String)");
        assert_eq!(
            header("Array( LowCardinality( String ) )"),
            "Array( String )"
        );
        assert_eq!(
            header("SimpleAggregateFunction(max, LowCardinality(String))"),
            "SimpleAggregateFunction(max, String)"
        );
        for verbatim in [
            "DateTime('UTC')",
            "DateTime64(6, 'UTC')",
            "Enum8('a' = -128, 'b' = 0, 'c' = 127)",
            "SimpleAggregateFunction(groupUniqArrayArray(16), Array(String))",
            "Tuple(Array(UInt16),Array(UInt64))",
        ] {
            assert_eq!(header(verbatim), verbatim);
        }
        assert_eq!(header("  UInt64\n"), "UInt64");
    }

    #[test]
    fn whitespace_and_quoted_punctuation_are_tolerated() {
        assert_eq!(
            wire(" Nullable ( DateTime64 ( 6 ,'UTC' ) ) "),
            W::Nullable(Box::new(W::Int64))
        );
        assert_eq!(wire("Enum8('a, (b)' = 1, 'it\\'s' = -2)"), W::Int8);
        assert_eq!(
            wire("SimpleAggregateFunction( groupUniqArrayArray ( 16 ) , Array ( String ) )"),
            arr(W::String)
        );
    }

    #[test]
    fn types_outside_section_6_4_and_malformed_strings_are_rejected() {
        for ty in [
            "",
            "Bool",
            "Map(String, UInt64)",
            "Int8",
            "Int16",
            "UInt128",
            "Decimal(18, 4)",
            "DateTime64(3, 'UTC')",
            "DateTime64(6)",
            "DateTime('Europe/Berlin')",
            "FixedString(0)",
            "FixedString(-1)",
            "Enum8('a' = 128)",
            "Enum8()",
            "LowCardinality(UInt64)",
            "LowCardinality(Nullable(String))",
            "Tuple(a UInt8)",
            "uint64",
            "UInt64 CODEC(ZSTD(1))",
            "Array(UInt64",
            "Array(UInt64))",
            "Enum8('a = 1)",
            "SimpleAggregateFunction(sum)",
        ] {
            assert_eq!(parse(ty), None, "`{ty}` should be rejected");
        }
    }
}
