//! The lexical check of a query text (contract 7.4). It exists for user experience only: it turns the common
//! mistakes into errors that say how to fix them before the server is asked. It is not a security boundary.
//! The server's own parser and access control decide what runs, and a text that slips past this check is
//! still confined to its project by ClickHouse.

use std::fmt;

/// The longest text the workspace sends: the profile's `max_query_size`.
pub const MAX_SQL_BYTES: usize = 128 * 1024;

/// One query parameter the text uses: `{name:Type}`.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ParamUse {
    /// The name.
    pub name: String,
    /// The declared ClickHouse type, as written.
    pub ty: String,
}

/// A text that passed the check.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Checked {
    /// The distinct parameters the text uses, in order of first use.
    pub params: Vec<ParamUse>,
}

/// Why a text was refused, with the place when there is one.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LexError {
    /// What is wrong.
    pub kind: LexErrorKind,
    /// The line, one-based.
    pub line: u32,
    /// The column, one-based, in characters.
    pub column: u32,
}

/// What is wrong with a text.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum LexErrorKind {
    /// Nothing but whitespace and comments.
    Empty,
    /// Longer than [`MAX_SQL_BYTES`].
    TooLong,
    /// Something follows a `;`.
    MultipleStatements,
    /// The first word is not `SELECT` or `WITH`.
    NotSelect,
    /// A `SETTINGS` clause.
    Settings,
    /// A `FORMAT` clause.
    Format,
    /// `INTO OUTFILE`.
    IntoOutfile,
    /// A string, quoted name or comment that never ends.
    Unterminated,
    /// The text does not use `{from:...}`.
    WindowFromMissing,
    /// The text does not use `{to:...}`.
    WindowToMissing,
    /// A window parameter is not declared as a `DateTime` or `DateTime64`.
    WindowType,
    /// One parameter is declared with two different types.
    ParamTypeConflict,
}

impl LexErrorKind {
    /// Whether the problem is the missing time window (`sql_time_window_required`) rather than an invalid text
    /// (`sql_invalid`).
    pub const fn is_window(&self) -> bool {
        matches!(
            self,
            Self::WindowFromMissing | Self::WindowToMissing | Self::WindowType
        )
    }

    /// What to change, in one sentence.
    pub const fn fix(&self) -> &'static str {
        match self {
            Self::Empty => "Write a query that starts with SELECT or WITH.",
            Self::TooLong => {
                "The query is longer than 128 KiB. Shorten it, or move long value lists into parameters."
            }
            Self::MultipleStatements => {
                "Run one statement at a time: remove everything after the semicolon."
            }
            Self::NotSelect => "The workspace is read-only: start the query with SELECT or WITH.",
            Self::Settings => {
                "Remove the SETTINGS clause: the workspace fixes its settings and limits for every query."
            }
            Self::Format => {
                "Remove the FORMAT clause: results come back as a grid, and Export offers CSV, JSON lines and Parquet."
            }
            Self::IntoOutfile => "Remove INTO OUTFILE: use Export to download the result.",
            Self::Unterminated => "Close the string, quoted name or comment that starts here.",
            Self::WindowFromMissing => {
                "Filter on the time window: add a condition such as start_time >= {from:DateTime64(6)}."
            }
            Self::WindowToMissing => {
                "Filter on the time window: add a condition such as start_time < {to:DateTime64(6)}."
            }
            Self::WindowType => {
                "Declare the window parameters as {from:DateTime64(6)} and {to:DateTime64(6)}."
            }
            Self::ParamTypeConflict => {
                "Declare each parameter with one type: this name is used with two different types."
            }
        }
    }
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "line {}, column {}: {}",
            self.line,
            self.column,
            self.kind.fix()
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Token<'a> {
    Word(&'a str),
    Param { name: &'a str, ty: &'a str },
    Semicolon,
    Equals,
    Other,
}

struct Lexer<'a> {
    text: &'a str,
    at: usize,
}

impl<'a> Lexer<'a> {
    fn rest(&self) -> &'a str {
        &self.text[self.at..]
    }

    /// Skips to just after `end`, or reports the token that starts at `start` as unterminated.
    fn skip_past(&mut self, start: usize, end: &str) -> Result<(), usize> {
        match self.rest().find(end) {
            Some(found) => {
                self.at += found + end.len();
                Ok(())
            }
            None => Err(start),
        }
    }

    /// Skips a quoted run that ends at an unescaped `quote`; a backslash or a doubled quote escapes.
    fn skip_quoted(&mut self, start: usize, quote: char) -> Result<(), usize> {
        let mut chars = self.rest().char_indices();
        while let Some((offset, c)) = chars.next() {
            if c == '\\' {
                chars.next();
            } else if c == quote {
                let after = self.at + offset + c.len_utf8();
                if self.text[after..].starts_with(quote) {
                    chars.next();
                } else {
                    self.at = after;
                    return Ok(());
                }
            }
        }
        Err(start)
    }

    /// `{name:Type}` at the cursor, which is on the `{`.
    fn param(&mut self) -> Option<Token<'a>> {
        let rest = self.rest();
        let close = rest.find('}')?;
        let (name, ty) = rest[1..close].split_once(':')?;
        let (name, ty) = (name.trim(), ty.trim());
        let ident = !name.is_empty()
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            && !name.as_bytes()[0].is_ascii_digit();
        if !ident || ty.is_empty() {
            return None;
        }
        self.at += close + 1;
        Some(Token::Param { name, ty })
    }

    /// The next token and where it starts; `None` at the end. `Err` carries the start of an unterminated run.
    fn next(&mut self) -> Result<Option<(usize, Token<'a>)>, usize> {
        loop {
            let rest = self.rest();
            let Some(c) = rest.chars().next() else {
                return Ok(None);
            };
            let start = self.at;
            if c.is_whitespace() {
                self.at += c.len_utf8();
            } else if rest.starts_with("--") || rest.starts_with('#') {
                self.at += rest.find('\n').unwrap_or(rest.len());
            } else if rest.starts_with("/*") {
                self.at += 2;
                self.skip_past(start, "*/")?;
            } else if c == '\'' || c == '"' || c == '`' {
                self.at += 1;
                self.skip_quoted(start, c)?;
                return Ok(Some((start, Token::Other)));
            } else if c == '$' {
                // A heredoc `$tag$ ... $tag$`; a lone `$` is an ordinary character.
                let tag_end = rest[1..].find('$').filter(|end| {
                    rest[1..1 + end]
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_')
                });
                match tag_end {
                    Some(end) => {
                        let tag = &rest[..end + 2];
                        self.at += tag.len();
                        self.skip_past(start, tag)?;
                    }
                    None => self.at += 1,
                }
                return Ok(Some((start, Token::Other)));
            } else if c == '{' {
                let token = self.param().unwrap_or_else(|| {
                    self.at += 1;
                    Token::Other
                });
                return Ok(Some((start, token)));
            } else if c.is_alphabetic() || c == '_' {
                let len = rest
                    .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .unwrap_or(rest.len());
                self.at += len;
                return Ok(Some((start, Token::Word(&rest[..len]))));
            } else {
                self.at += c.len_utf8();
                let token = match c {
                    ';' => Token::Semicolon,
                    '=' => Token::Equals,
                    _ => Token::Other,
                };
                return Ok(Some((start, token)));
            }
        }
    }
}

fn error(text: &str, at: usize, kind: LexErrorKind) -> LexError {
    let before = &text[..at.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let column = before
        .rsplit('\n')
        .next()
        .unwrap_or_default()
        .chars()
        .count()
        + 1;
    LexError {
        kind,
        line: u32::try_from(line).unwrap_or(u32::MAX),
        column: u32::try_from(column).unwrap_or(u32::MAX),
    }
}

fn is_time_type(ty: &str) -> bool {
    let ty = ty.trim();
    ty == "DateTime" || ty.starts_with("DateTime(") || ty.starts_with("DateTime64(")
}

/// Checks a query text: its size, that it is one statement that starts with `SELECT` or `WITH`, that it has no
/// `SETTINGS`, `FORMAT` or `INTO OUTFILE` clause, and that it uses the window parameters `{from}` and `{to}`.
pub fn check(text: &str) -> Result<Checked, LexError> {
    if text.len() > MAX_SQL_BYTES {
        return Err(error(text, 0, LexErrorKind::TooLong));
    }
    let mut lexer = Lexer { text, at: 0 };
    let mut params: Vec<(ParamUse, usize)> = Vec::new();
    let mut first = true;
    let mut ended: Option<usize> = None;
    // The two tokens before the current one.
    let mut history: [Option<(usize, Token<'_>)>; 2] = [None, None];
    let is_word = |token: Option<(usize, Token<'_>)>, word: &str| matches!(token, Some((_, Token::Word(w))) if w.eq_ignore_ascii_case(word));
    loop {
        let token = lexer
            .next()
            .map_err(|at| error(text, at, LexErrorKind::Unterminated))?;
        // A clause keyword is judged by what follows it, because a column may be named `settings` or `format`
        // and `format(...)` is a function: `SETTINGS name =` and `FORMAT Name` at the end of the statement
        // are the clauses.
        let [before, last] = history;
        if let (Some((keyword_at, _)), Some((_, Token::Word(_)))) = (before, last) {
            let ends =
                matches!(token, None | Some((_, Token::Semicolon))) || is_word(token, "settings");
            if is_word(before, "settings") && matches!(token, Some((_, Token::Equals))) {
                return Err(error(text, keyword_at, LexErrorKind::Settings));
            }
            if is_word(before, "format") && ends {
                return Err(error(text, keyword_at, LexErrorKind::Format));
            }
        }
        if let Some((keyword_at, _)) = last
            && is_word(last, "into")
            && is_word(token, "outfile")
        {
            return Err(error(text, keyword_at, LexErrorKind::IntoOutfile));
        }
        let Some((at, token)) = token else { break };
        if ended.is_some() {
            return Err(error(text, at, LexErrorKind::MultipleStatements));
        }
        if first {
            let select = matches!(token, Token::Word(word)
                if word.eq_ignore_ascii_case("select") || word.eq_ignore_ascii_case("with"));
            if !select {
                return Err(error(text, at, LexErrorKind::NotSelect));
            }
            first = false;
        }
        match token {
            Token::Semicolon => ended = Some(at),
            Token::Param { name, ty } => match params.iter().find(|(p, _)| p.name == name) {
                Some((known, _)) if known.ty != ty => {
                    return Err(error(text, at, LexErrorKind::ParamTypeConflict));
                }
                Some(_) => {}
                None => params.push((
                    ParamUse {
                        name: name.to_owned(),
                        ty: ty.to_owned(),
                    },
                    at,
                )),
            },
            Token::Word(_) | Token::Equals | Token::Other => {}
        }
        history = [last, Some((at, token))];
    }
    if first {
        return Err(error(text, 0, LexErrorKind::Empty));
    }
    for (name, missing) in [
        ("from", LexErrorKind::WindowFromMissing),
        ("to", LexErrorKind::WindowToMissing),
    ] {
        match params.iter().find(|(p, _)| p.name == name) {
            None => return Err(error(text, text.len(), missing)),
            Some((param, at)) if !is_time_type(&param.ty) => {
                return Err(error(text, *at, LexErrorKind::WindowType));
            }
            Some(_) => {}
        }
    }
    Ok(Checked {
        params: params.into_iter().map(|(param, _)| param).collect(),
    })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const WINDOW: &str = "start_time >= {from:DateTime64(6)} AND start_time < {to:DateTime64(6)}";

    fn kind(text: &str) -> LexErrorKind {
        check(text).unwrap_err().kind
    }

    #[test]
    fn a_windowed_select_passes_and_lists_its_parameters() {
        let text = format!(
            "-- spans by name\nSELECT name, count() FROM spans WHERE {WINDOW} AND user_id = {{user:String}} GROUP BY \
             name;"
        );
        let checked = check(&text).unwrap();
        let names: Vec<&str> = checked.params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["from", "to", "user"]);
        assert_eq!(checked.params[0].ty, "DateTime64(6)");
        assert!(
            check(&format!(
                "WITH x AS (SELECT 1) SELECT * FROM spans WHERE {WINDOW}"
            ))
            .is_ok()
        );
        assert!(
            check(&format!(
                "  select 1 from spans where {WINDOW}  ;  -- done\n"
            ))
            .is_ok()
        );
    }

    #[test]
    fn each_mistake_has_its_own_error_and_place() {
        assert_eq!(kind(""), LexErrorKind::Empty);
        assert_eq!(kind("  -- only a comment\n/* x */"), LexErrorKind::Empty);
        assert_eq!(kind("INSERT INTO t VALUES (1)"), LexErrorKind::NotSelect);
        assert_eq!(kind("(SELECT 1)"), LexErrorKind::NotSelect);
        assert_eq!(kind("SET max_threads = 1"), LexErrorKind::NotSelect);
        assert_eq!(
            kind(&format!("SELECT 1 FROM spans WHERE {WINDOW}; SELECT 2")),
            LexErrorKind::MultipleStatements
        );
        assert_eq!(
            kind(&format!(
                "SELECT 1 FROM spans WHERE {WINDOW} SETTINGS max_threads = 8"
            )),
            LexErrorKind::Settings
        );
        assert_eq!(
            kind(&format!("SELECT 1 FROM spans WHERE {WINDOW} FORMAT JSON")),
            LexErrorKind::Format
        );
        assert_eq!(
            kind(&format!(
                "SELECT 1 FROM spans WHERE {WINDOW} INTO OUTFILE 'x'"
            )),
            LexErrorKind::IntoOutfile
        );
        assert_eq!(kind("SELECT 'abc"), LexErrorKind::Unterminated);
        assert_eq!(kind("SELECT 1 /* never closed"), LexErrorKind::Unterminated);
        assert_eq!(kind("SELECT 1"), LexErrorKind::WindowFromMissing);
        assert_eq!(
            kind("SELECT 1 FROM spans WHERE start_time >= {from:DateTime64(6)}"),
            LexErrorKind::WindowToMissing
        );
        assert_eq!(
            kind(
                "SELECT 1 FROM spans WHERE start_time >= {from:String} AND start_time < {to:DateTime}"
            ),
            LexErrorKind::WindowType
        );
        assert_eq!(
            kind(&format!(
                "SELECT {{x:String}}, {{x:Int64}} FROM spans WHERE {WINDOW}"
            )),
            LexErrorKind::ParamTypeConflict
        );
        assert_eq!(kind(&"x".repeat(MAX_SQL_BYTES + 1)), LexErrorKind::TooLong);
        let error = check("SELECT 1\nFROM spans\n  SETTINGS max_threads = 1").unwrap_err();
        assert_eq!((error.line, error.column), (3, 3));
        assert!(error.to_string().contains("Remove the SETTINGS clause"));
    }

    #[test]
    fn keywords_inside_strings_names_and_comments_are_not_clauses() {
        for text in [
            format!("SELECT 'a; SETTINGS x = 1 FORMAT JSON' FROM spans WHERE {WINDOW}"),
            format!("SELECT `format`, \"settings\" FROM spans WHERE {WINDOW}"),
            format!("SELECT format('{{}}', name), settings FROM spans WHERE {WINDOW}"),
            format!(
                "SELECT settings, format FROM spans WHERE {WINDOW} AND format = 'x' ORDER BY format"
            ),
            format!(
                "SELECT 1 /* FORMAT JSON; SELECT 2 */ FROM spans WHERE {WINDOW} -- SETTINGS a = 1"
            ),
            format!("SELECT $tag$; INTO OUTFILE$tag$ FROM spans WHERE {WINDOW}"),
            format!("SELECT 'it''s; fine', 'a\\'; b' FROM spans WHERE {WINDOW}"),
            format!("SELECT {{'a': 1}}, [1, 2] FROM spans WHERE {WINDOW}"),
        ] {
            assert!(check(&text).is_ok(), "{text}");
        }
        // A window parameter inside a string or a comment is not a use.
        assert_eq!(
            kind("SELECT '{from:DateTime64(6)} {to:DateTime64(6)}'"),
            LexErrorKind::WindowFromMissing
        );
    }

    #[test]
    fn every_error_says_how_to_fix_it() {
        for kind in [
            LexErrorKind::Empty,
            LexErrorKind::TooLong,
            LexErrorKind::MultipleStatements,
            LexErrorKind::NotSelect,
            LexErrorKind::Settings,
            LexErrorKind::Format,
            LexErrorKind::IntoOutfile,
            LexErrorKind::Unterminated,
            LexErrorKind::WindowFromMissing,
            LexErrorKind::WindowToMissing,
            LexErrorKind::WindowType,
            LexErrorKind::ParamTypeConflict,
        ] {
            assert!(kind.fix().len() > 20 && kind.fix().ends_with('.'));
        }
        assert!(LexErrorKind::WindowToMissing.is_window());
        assert!(!LexErrorKind::Settings.is_window());
    }

    proptest! {
        /// The check never panics, whatever the text, and an accepted text starts with SELECT or WITH and uses
        /// both window parameters.
        #[test]
        fn the_check_is_total(text in "\\PC{0,200}") {
            if let Ok(checked) = check(&text) {
                let first = text.trim_start().to_ascii_lowercase();
                prop_assert!(first.starts_with("select") || first.starts_with("with") || first.starts_with("--")
                    || first.starts_with("/*") || first.starts_with('#'));
                prop_assert!(checked.params.iter().any(|p| p.name == "from"));
                prop_assert!(checked.params.iter().any(|p| p.name == "to"));
            }
        }

        /// A text built from SQL fragments: errors always point inside the text.
        #[test]
        fn errors_point_inside_the_text(parts in prop::collection::vec(prop::sample::select(vec![
            "SELECT", "WITH", "FROM spans", "WHERE", "'str", "'s'", ";", "SETTINGS a", "FORMAT x", "INTO OUTFILE",
            "{from:DateTime64(6)}", "{to:DateTime}", "/*", "*/", "--", "\n", "`q`", "$a$", "(", ")", "{x:Int64}",
        ]), 0..12)) {
            let text = parts.join(" ");
            if let Err(error) = check(&text) {
                let lines = text.matches('\n').count() + 1;
                prop_assert!(usize::try_from(error.line).unwrap() <= lines);
                prop_assert!(error.column >= 1);
            }
        }
    }
}
