//! PostgreSQL's type-name grammar, over the lexer's tokens: a
//! (qualified, possibly quoted) name or one of the multi-word SQL type
//! names, optional modifiers, then array dimensions. Nothing else may
//! follow, so a type field cannot carry clauses such as `PRIMARY KEY`,
//! `SECURITY DEFINER` or `GENERATED ... AS IDENTITY`.

use varve_driver::{LexKind, Lexeme};

use super::common::RESERVED_WORDS;

/// Walks the tokens of one fragment.
struct Cursor<'a> {
    text: &'a str,
    tokens: &'a [Lexeme],
    pos: usize,
}

impl Cursor<'_> {
    fn done(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    /// The lower-cased word at `pos + offset`, if that token is a plain word.
    fn word(&self, offset: usize) -> Option<String> {
        self.tokens
            .get(self.pos + offset)
            .filter(|t| t.kind == LexKind::Word)
            .map(|t| self.text[t.start..t.end].to_ascii_lowercase())
    }

    fn is_word(&self, offset: usize, words: &[&str]) -> bool {
        self.word(offset)
            .is_some_and(|w| words.contains(&w.as_str()))
    }

    fn is_punct(&self, offset: usize, c: char) -> bool {
        self.tokens
            .get(self.pos + offset)
            .is_some_and(|t| t.kind == LexKind::Punct(c))
    }

    fn eat_punct(&mut self, c: char) -> bool {
        let found = self.is_punct(0, c);
        if found {
            self.pos += 1;
        }
        found
    }

    fn eat_word(&mut self, words: &[&str]) -> bool {
        let found = self.is_word(0, words);
        if found {
            self.pos += 1;
        }
        found
    }

    /// An unsigned integer literal.
    fn eat_integer(&mut self) -> bool {
        let found = self.tokens.get(self.pos).is_some_and(|t| {
            t.kind == LexKind::Number
                && self.text[t.start..t.end]
                    .bytes()
                    .all(|b| b.is_ascii_digit())
        });
        if found {
            self.pos += 1;
        }
        found
    }

    fn eat_name(&mut self) -> bool {
        let found = self
            .tokens
            .get(self.pos)
            .is_some_and(|t| matches!(t.kind, LexKind::Word | LexKind::QuotedIdent));
        if found {
            self.pos += 1;
        }
        found
    }
}

/// The fields of an `INTERVAL` qualifier.
const INTERVAL_FIELDS: &[&str] = &["year", "month", "day", "hour", "minute", "second"];

/// `(n)` with one integer.
fn precision(c: &mut Cursor<'_>) -> Result<(), ()> {
    if c.eat_punct('(') && !(c.eat_integer() && c.eat_punct(')')) {
        return Err(());
    }
    Ok(())
}

/// `(m, ...)`: type modifiers. PostgreSQL passes them to the type's
/// modifier function and only allows constants and plain names there
/// (`numeric(10, -2)`, `geometry(Point, 4326)`, `x('a')`), so each item is
/// one (optionally negative) number, one string, or one name that is not a
/// reserved word. No expressions, no nested parentheses.
fn modifiers(c: &mut Cursor<'_>) -> Result<(), ()> {
    if !c.eat_punct('(') {
        return Ok(());
    }
    loop {
        modifier(c)?;
        if c.eat_punct(')') {
            return Ok(());
        }
        if !c.eat_punct(',') {
            return Err(());
        }
    }
}

fn modifier(c: &mut Cursor<'_>) -> Result<(), ()> {
    let Some(token) = c.tokens.get(c.pos).copied() else {
        return Err(());
    };
    let ok = match token.kind {
        LexKind::Punct('-') => {
            c.pos += 1;
            return match c.tokens.get(c.pos) {
                Some(t) if t.kind == LexKind::Number => {
                    c.pos += 1;
                    Ok(())
                }
                _ => Err(()),
            };
        }
        LexKind::Number | LexKind::String | LexKind::QuotedIdent => true,
        LexKind::Word => {
            !RESERVED_WORDS.contains(&c.text[token.start..token.end].to_ascii_lowercase().as_str())
        }
        _ => false,
    };
    if ok {
        c.pos += 1;
        Ok(())
    } else {
        Err(())
    }
}

/// `INTERVAL [fields] [(p)]`, after the word `interval`.
fn interval_tail(c: &mut Cursor<'_>) -> Result<(), ()> {
    let Some(first) = c.word(0).filter(|w| INTERVAL_FIELDS.contains(&w.as_str())) else {
        return precision(c);
    };
    c.pos += 1;
    let mut last = first.clone();
    if c.eat_word(&["to"]) {
        let allowed: &[&str] = match first.as_str() {
            "year" => &["month"],
            "day" => &["hour", "minute", "second"],
            "hour" => &["minute", "second"],
            "minute" => &["second"],
            _ => &[],
        };
        match c.word(0) {
            Some(w) if allowed.contains(&w.as_str()) => {
                c.pos += 1;
                last = w;
            }
            _ => return Err(()),
        }
    }
    if last == "second" {
        precision(c)
    } else {
        Ok(())
    }
}

/// `[]`, `[n]` (repeated) or `ARRAY [n]`.
fn dimensions(c: &mut Cursor<'_>) -> Result<(), ()> {
    if c.eat_word(&["array"]) {
        if c.eat_punct('[') && (!c.eat_integer() || !c.eat_punct(']')) {
            return Err(());
        }
        return Ok(());
    }
    while c.eat_punct('[') {
        c.eat_integer();
        if !c.eat_punct(']') {
            return Err(());
        }
    }
    Ok(())
}

/// One type name starting at the cursor.
fn type_name(c: &mut Cursor<'_>) -> Result<(), ()> {
    let qualified_next = c.is_punct(1, '.');
    if !qualified_next && c.is_word(0, &["double"]) && c.is_word(1, &["precision"]) {
        c.pos += 2;
    } else if !qualified_next && c.eat_word(&["national"]) {
        if !c.eat_word(&["character", "char"]) {
            return Err(());
        }
        c.eat_word(&["varying"]);
        modifiers(c)?;
    } else if !qualified_next
        && c.is_word(0, &["character", "char", "nchar", "bit"])
        && c.is_word(1, &["varying"])
    {
        c.pos += 2;
        modifiers(c)?;
    } else if !qualified_next && c.eat_word(&["time", "timestamp"]) {
        precision(c)?;
        if c.eat_word(&["with", "without"]) && !(c.eat_word(&["time"]) && c.eat_word(&["zone"])) {
            return Err(());
        }
    } else if !qualified_next && c.eat_word(&["interval"]) {
        interval_tail(c)?;
    } else {
        if !c.eat_name() {
            return Err(());
        }
        for _ in 0..2 {
            if !c.eat_punct('.') {
                break;
            }
            if !c.eat_name() {
                return Err(());
            }
        }
        modifiers(c)?;
    }
    dimensions(c)
}

/// True when `tokens` are exactly one type name.
pub(crate) fn is_type(text: &str, tokens: &[Lexeme]) -> bool {
    let mut c = Cursor {
        text,
        tokens,
        pos: 0,
    };
    type_name(&mut c).is_ok() && c.done()
}

/// True when `tokens` are a type name or a column's type taken with
/// `%TYPE` (`t.col%TYPE`, `s.t.col%TYPE`), as routine argument and return
/// types allow.
pub(crate) fn is_func_type(text: &str, tokens: &[Lexeme]) -> bool {
    is_type(text, tokens) || is_percent_type(text, tokens)
}

/// `name.name[.name[.name]]%TYPE`.
fn is_percent_type(text: &str, tokens: &[Lexeme]) -> bool {
    let mut c = Cursor {
        text,
        tokens,
        pos: 0,
    };
    if !c.eat_name() {
        return false;
    }
    let mut parts = 1;
    while c.eat_punct('.') {
        if !c.eat_name() {
            return false;
        }
        parts += 1;
    }
    (2..=4).contains(&parts) && c.eat_punct('%') && c.eat_word(&["type"]) && c.done()
}

/// True when `tokens` are one routine argument as
/// `pg_get_function_identity_arguments` writes it: `[mode] [name] type`.
pub(crate) fn is_argument(text: &str, tokens: &[Lexeme]) -> bool {
    let skip_mode = usize::from(
        Cursor {
            text,
            tokens,
            pos: 0,
        }
        .is_word(0, &["in", "out", "inout", "variadic"]),
    );
    let rest = &tokens[skip_mode.min(tokens.len())..];
    // `name type` or just `type`; a bare type wins when both parse.
    is_func_type(text, rest)
        || (rest.len() > 1
            && matches!(rest[0].kind, LexKind::Word | LexKind::QuotedIdent)
            && is_func_type(text, &rest[1..]))
}
