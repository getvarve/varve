//! Redis console syntax, compatible with `redis-cli` (`sdssplitargs`), with
//! one deliberate difference: a NUL separates arguments instead of ending
//! the line (see [`is_space`]).
//!
//! The driver runs what [`split_args`] returns and the activity redactor
//! hides secrets in what [`split_args_lenient`] returns. Both use the same
//! splitter, so an argument the driver sends is always the argument the
//! redactor sees. The lenient variant never fails, because text that the
//! driver rejects is still written to the activity log.

use std::borrow::Cow;

/// One argument of a line, with byte offsets into that line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arg {
    /// Decoded value: quotes removed, escapes applied.
    pub value: Vec<u8>,
    /// Byte offset of the argument's first character.
    pub start: usize,
    /// Byte offset just past the argument (past its closing quote).
    pub end: usize,
    /// Start of the text between the quotes when the whole argument is one
    /// quoted string; otherwise equal to `start`.
    pub content_start: usize,
    /// End of the text between the quotes when the whole argument is one
    /// quoted string; otherwise equal to `end`.
    pub content_end: usize,
}

impl Arg {
    /// The value as text, with invalid UTF-8 replaced.
    #[must_use]
    pub fn text(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.value)
    }

    /// Returns the argument with all offsets moved by `by` bytes.
    #[must_use]
    pub fn shifted(mut self, by: usize) -> Self {
        self.start += by;
        self.end += by;
        self.content_start += by;
        self.content_end += by;
        self
    }
}

/// A syntax error inside one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitError {
    /// What is wrong.
    pub message: &'static str,
    /// Byte offset in the line of the quote that caused it.
    pub offset: usize,
}

impl std::fmt::Display for SplitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message)
    }
}

impl std::error::Error for SplitError {}

/// Splits one line into arguments the way `redis-cli` does (`sdssplitargs`):
/// arguments are separated by whitespace; a quote may start anywhere in an
/// argument; double quotes allow the escapes `\n`, `\r`, `\t`, `\b`, `\a`,
/// `\xHH` (any other escaped character stands for itself); single quotes
/// are literal except for `\'`. A closing quote must be followed by
/// whitespace or the end of the line.
///
/// # Errors
///
/// Unterminated quotes, and a closing quote followed by another character.
pub fn split_args(line: &str) -> Result<Vec<Arg>, SplitError> {
    split(line, true)
}

/// Like [`split_args`], but never fails: an unterminated quote runs to the
/// end of the line and a closing quote followed by another character
/// continues the same argument. For valid input the result is exactly
/// that of [`split_args`].
#[must_use]
pub fn split_args_lenient(line: &str) -> Vec<Arg> {
    split(line, false).unwrap_or_default()
}

/// True for argument separators: what C `isspace` accepts, as in
/// `redis-cli` (space, `\t`, `\n`, `\v`, `\f`, `\r`), plus NUL.
///
/// `redis-cli` reads a line as a C string, so it never sees anything after a
/// NUL. Varve has the whole text, and treating NUL as a separator keeps the
/// driver and the activity redactor in agreement about every byte (text
/// after a NUL is never silently dropped from what the redactor checks).
#[must_use]
pub fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | '\0')
}

fn split(line: &str, strict: bool) -> Result<Vec<Arg>, SplitError> {
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    let byte_at = |i: usize| chars.get(i).map_or(line.len(), |(b, _)| *b);
    let char_at = |i: usize| chars.get(i).map(|(_, c)| *c);
    let mut args = Vec::new();
    let mut i = 0;
    loop {
        while char_at(i).is_some_and(is_space) {
            i += 1;
        }
        if i >= chars.len() {
            return Ok(args);
        }
        let start = i;
        let mut value: Vec<u8> = Vec::new();
        // (opening quote index, closing quote index or None) of every
        // quoted section; used to find the content span.
        let mut sections: Vec<(usize, Option<usize>)> = Vec::new();
        while let Some(c) = char_at(i) {
            match c {
                '"' | '\'' => {
                    let quote = c;
                    let open = i;
                    i += 1;
                    let mut close = None;
                    while let Some(q) = char_at(i) {
                        if q == '\\' && quote == '"' && i + 1 < chars.len() {
                            let next = chars[i + 1].1;
                            if next == 'x'
                                && let Some(byte) = hex_byte(char_at(i + 2), char_at(i + 3))
                            {
                                value.push(byte);
                                i += 4;
                                continue;
                            }
                            push_char(
                                &mut value,
                                match next {
                                    'n' => '\n',
                                    'r' => '\r',
                                    't' => '\t',
                                    'b' => '\u{8}',
                                    'a' => '\u{7}',
                                    other => other,
                                },
                            );
                            i += 2;
                        } else if q == '\\' && quote == '\'' && char_at(i + 1) == Some('\'') {
                            value.push(b'\'');
                            i += 2;
                        } else if q == quote {
                            close = Some(i);
                            i += 1;
                            break;
                        } else {
                            push_char(&mut value, q);
                            i += 1;
                        }
                    }
                    sections.push((open, close));
                    match close {
                        None if strict => {
                            return Err(SplitError {
                                message: if quote == '"' {
                                    "unterminated double quote"
                                } else {
                                    "unterminated single quote"
                                },
                                offset: byte_at(open),
                            });
                        }
                        None => break,
                        Some(close) => {
                            if char_at(i).is_some_and(|n| !is_space(n)) {
                                if strict {
                                    return Err(SplitError {
                                        message: "closing quote must be followed by a space",
                                        offset: byte_at(close),
                                    });
                                }
                                continue;
                            }
                            break;
                        }
                    }
                }
                c if is_space(c) => break,
                c => {
                    push_char(&mut value, c);
                    i += 1;
                }
            }
        }
        let end = byte_at(i);
        let (content_start, content_end) = match sections.as_slice() {
            [(open, close)] if *open == start && close.is_none_or(|c| c + 1 == i) => {
                (byte_at(open + 1), close.map_or(line.len(), &byte_at))
            }
            _ => (byte_at(start), end),
        };
        args.push(Arg {
            value,
            start: byte_at(start),
            end,
            content_start,
            content_end,
        });
    }
}

/// Quotes one argument so that [`split_args`] reads it back as exactly
/// `value`: plain when it has no separators, quotes or backslashes, does not
/// start with `#` and is not empty, otherwise a double-quoted string with `\\`, `\"`, `\n`,
/// `\r`, `\t` and `\xHH` escapes for every other control byte and every
/// byte that is not valid UTF-8.
#[must_use]
pub fn quote_arg(value: &[u8]) -> String {
    // A leading `#` is quoted too: as the first argument of a line it would
    // make [`lines`] read the whole line as a comment.
    let plain = !value.is_empty()
        && value.first() != Some(&b'#')
        && std::str::from_utf8(value).is_ok_and(|text| {
            !text
                .chars()
                .any(|c| is_space(c) || c == '"' || c == '\'' || c == '\\' || c.is_control())
        });
    if plain {
        return String::from_utf8_lossy(value).into_owned();
    }
    let mut out = String::from("\"");
    let mut rest = value;
    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(text) => {
                escape_text_into(text, &mut out);
                rest = &[];
            }
            Err(error) => {
                let (valid, invalid) = rest.split_at(error.valid_up_to());
                escape_text_into(&String::from_utf8_lossy(valid), &mut out);
                let bad = error.error_len().unwrap_or(invalid.len());
                for byte in &invalid[..bad] {
                    out.push_str(&format!("\\x{byte:02x}"));
                }
                rest = &invalid[bad..];
            }
        }
    }
    out.push('"');
    out
}

/// Joins arguments into one console line that [`split_args`] reads back
/// exactly.
#[must_use]
pub fn join_args<A: AsRef<[u8]>>(args: &[A]) -> String {
    args.iter()
        .map(|a| quote_arg(a.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn escape_text_into(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let mut buf = [0u8; 4];
                for byte in c.encode_utf8(&mut buf).as_bytes() {
                    out.push_str(&format!("\\x{byte:02x}"));
                }
            }
            c => out.push(c),
        }
    }
}

fn push_char(out: &mut Vec<u8>, c: char) {
    let mut buf = [0u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
}

fn hex_byte(high: Option<char>, low: Option<char>) -> Option<u8> {
    let high = high?.to_digit(16)?;
    let low = low?.to_digit(16)?;
    u8::try_from(high * 16 + low).ok()
}

/// One non-blank line of a console script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Line<'a> {
    /// 1-based line number.
    pub number: usize,
    /// Byte offset of the line in the script.
    pub offset: usize,
    /// The line without its line ending.
    pub text: &'a str,
    /// The first non-blank character is `#`: `redis-cli` skips the line.
    pub comment: bool,
}

impl Line<'_> {
    /// Byte offset in the line of the text after the `#` of a comment line.
    #[must_use]
    pub fn comment_body_offset(&self) -> Option<usize> {
        self.comment
            .then(|| self.text.find('#').map(|at| at + 1))
            .flatten()
    }
}

/// The non-blank lines of a script, comments included.
pub fn lines(script: &str) -> impl Iterator<Item = Line<'_>> {
    let mut offset = 0;
    script
        .split('\n')
        .enumerate()
        .filter_map(move |(index, raw)| {
            let line_offset = offset;
            offset += raw.len() + 1;
            let text = raw.strip_suffix('\r').unwrap_or(raw);
            let trimmed = text.trim_start_matches(is_space);
            (!trimmed.is_empty()).then_some(Line {
                number: index + 1,
                offset: line_offset,
                text,
                comment: trimmed.starts_with('#'),
            })
        })
}

/// The connection setup commands allowed as init commands, as
/// `(name, subcommand)`: they only change the state of the connection
/// itself, and running them again on every reconnect is harmless. `AUTH`
/// is not among them: a password belongs in the profile's credentials
/// (stored in the keychain), not in the init text.
pub const INIT_COMMANDS: &[(&str, Option<&str>)] = &[
    ("CLIENT", Some("NO-EVICT")),
    ("CLIENT", Some("NO-TOUCH")),
    ("CLIENT", Some("SETINFO")),
    ("CLIENT", Some("SETNAME")),
    ("HELLO", None),
    ("READONLY", None),
    ("READWRITE", None),
    ("SELECT", None),
];

/// Why `HELLO` with these arguments is not one the driver can run, or
/// `None`: the first argument must be protocol version 2 (the driver
/// speaks RESP2), or absent.
#[must_use]
pub fn hello_problem<A: AsRef<[u8]>>(args: &[A]) -> Option<&'static str> {
    let version = args.get(1)?;
    let version = String::from_utf8_lossy(version.as_ref());
    if version.is_empty() || !version.bytes().all(|b| b.is_ascii_digit()) {
        return Some(
            "needs the protocol version as its first argument (HELLO 2 ...), and Varve speaks \
             version 2",
        );
    }
    (version != "2").then_some("switches the protocol version (Varve speaks version 2, RESP2)")
}

/// Why a command cannot run as an init command of a connection profile, or
/// `None`. Init commands run on every connect and reconnect, so only the
/// connection setup commands in [`INIT_COMMANDS`] are allowed, `HELLO` only
/// with protocol version 2, and nothing that carries a password (`AUTH`,
/// `HELLO ... AUTH`). `name` is the command name (any case), `args` all
/// arguments with the name first.
#[must_use]
pub fn init_command_problem<A: AsRef<[u8]>>(name: &str, args: &[A]) -> Option<&'static str> {
    const PASSWORD: &str = "puts a password in the init text; enter the user name and password \
                            in the profile's credentials instead (the password is kept in the \
                            keychain)";
    let name = name.to_ascii_uppercase();
    let sub = args
        .get(1)
        .map(|a| String::from_utf8_lossy(a.as_ref()).to_ascii_uppercase());
    if name == "AUTH" || (name == "HELLO" && hello_has_auth(args)) {
        return Some(PASSWORD);
    }
    let allowed = INIT_COMMANDS
        .iter()
        .any(|(n, s)| *n == name && s.is_none_or(|s| sub.as_deref() == Some(s)));
    if !allowed {
        return Some(
            "is not a connection setup command (init commands run on every reconnect and may \
             only be SELECT, HELLO 2, CLIENT SETNAME, CLIENT SETINFO, CLIENT NO-EVICT, \
             CLIENT NO-TOUCH, READONLY or READWRITE)",
        );
    }
    if name == "HELLO" {
        return hello_problem(args);
    }
    if name == "SELECT" && !selects_trackable_db(args) {
        return Some("selects a database number outside 0 to 65535, which Varve cannot follow");
    }
    None
}

/// True when `HELLO`'s options include `AUTH <user> <password>`. The
/// options are read by position after the version (`AUTH` takes two
/// values, `SETNAME` one), so a client named `auth` is not mistaken for a
/// password.
#[must_use]
pub fn hello_has_auth<A: AsRef<[u8]>>(args: &[A]) -> bool {
    let is = |a: &A, word: &str| a.as_ref().eq_ignore_ascii_case(word.as_bytes());
    // The options follow the version; without a version (which the server
    // refuses anyway) they start right after HELLO.
    let versioned = args
        .get(1)
        .is_some_and(|v| !v.as_ref().is_empty() && v.as_ref().iter().all(u8::is_ascii_digit));
    let mut i = if versioned { 2 } else { 1 };
    while let Some(arg) = args.get(i) {
        if is(arg, "AUTH") {
            return true;
        }
        i += if is(arg, "SETNAME") { 2 } else { 1 };
    }
    false
}

/// False for `SELECT` with a decimal index above 65535 (or a negative
/// one): the driver follows the database as a 16-bit number. Other
/// malformed indexes are left to the server.
fn selects_trackable_db<A: AsRef<[u8]>>(args: &[A]) -> bool {
    let Some(index) = args.get(1) else {
        return true;
    };
    let text = String::from_utf8_lossy(index.as_ref());
    match text.trim().parse::<i128>() {
        Ok(n) => (0..=i128::from(u16::MAX)).contains(&n),
        Err(_) => true,
    }
}

/// A problem with the init commands of a connection profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitProblem {
    /// 1-based line number in the script.
    pub line: usize,
    /// Byte offset in the script of the command (or of the quote that
    /// could not be parsed).
    pub offset: usize,
    /// What is wrong, naming the command.
    pub message: String,
}

impl std::fmt::Display for InitProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for InitProblem {}

/// Checks the init commands of a connection profile (one per line, like
/// the console): every line must parse, and every command must be a
/// connection setup command ([`init_command_problem`]). Checks the typed names; the driver
/// checks again by the server's real names (renamed commands) when it
/// connects.
///
/// # Errors
///
/// The first problem, with its line.
pub fn check_init_script(script: &str) -> Result<(), InitProblem> {
    for line in lines(script).filter(|l| !l.comment) {
        let args = split_args(line.text).map_err(|e| InitProblem {
            line: line.number,
            offset: line.offset + e.offset,
            message: e.message.to_string(),
        })?;
        let Some(first) = args.first() else {
            continue;
        };
        let name = first.text().into_owned();
        let values: Vec<&[u8]> = args.iter().map(|a| a.value.as_slice()).collect();
        if let Some(problem) = init_command_problem(&name, &values) {
            return Err(InitProblem {
                line: line.number,
                offset: line.offset + first.start,
                message: format!(
                    "{} cannot be an init command: it {problem}",
                    name.to_ascii_uppercase()
                ),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strict(line: &str) -> Vec<String> {
        split_args(line)
            .unwrap_or_else(|e| panic!("{line}: {e:?}"))
            .iter()
            .map(|a| a.text().into_owned())
            .collect()
    }

    fn lenient(line: &str) -> Vec<String> {
        split_args_lenient(line)
            .iter()
            .map(|a| a.text().into_owned())
            .collect()
    }

    #[test]
    fn splits_on_whitespace() {
        assert_eq!(
            strict("  SET  key\tvalue \0x "),
            ["SET", "key", "value", "x"]
        );
        assert!(strict("   ").is_empty());
    }

    #[test]
    fn double_quotes_take_escapes() -> Result<(), SplitError> {
        assert_eq!(
            strict(r#"SET k "a b\n\t\"\\\x41\q""#),
            ["SET", "k", "a b\n\t\"\\Aq"]
        );
        assert_eq!(split_args(r#""\xff""#)?[0].value, [0xff]);
        assert_eq!(strict(r#""\xZZ""#), ["xZZ"]);
        Ok(())
    }

    #[test]
    fn single_quotes_are_literal() {
        assert_eq!(strict(r"SET k 'it\'s \n'"), ["SET", "k", "it's \\n"]);
    }

    #[test]
    fn quotes_can_start_mid_token_like_redis_cli() {
        assert_eq!(strict(r#"SET a"b c" x"#), ["SET", "ab c", "x"]);
    }

    #[test]
    fn rejects_bad_quotes() {
        assert_eq!(
            split_args(r#"SET k "abc"#),
            Err(SplitError {
                message: "unterminated double quote",
                offset: 6
            })
        );
        assert!(split_args("SET k 'abc").is_err());
        assert_eq!(split_args(r#"SET "a"b"#).map_err(|e| e.offset), Err(6));
        assert!(split_args("SET 'a'b").is_err());
        assert!(split_args(r#"SET "a\""#).is_err());
    }

    #[test]
    fn offsets_are_bytes() -> Result<(), SplitError> {
        let args = split_args("SET é \"ü v\" w")?;
        assert_eq!((args[1].start, args[1].end), (4, 6));
        assert_eq!((args[2].start, args[2].end), (7, 13));
        assert_eq!((args[2].content_start, args[2].content_end), (8, 12));
        assert_eq!((args[3].start, args[3].content_start), (14, 14));
        Ok(())
    }

    #[test]
    fn mixed_arguments_have_no_inner_content_span() -> Result<(), SplitError> {
        let args = split_args(r#"a"b c""#)?;
        assert_eq!((args[0].content_start, args[0].content_end), (0, 6));
        Ok(())
    }

    #[test]
    fn lenient_matches_strict_on_valid_input() {
        for line in [
            r#"SET k "a b\n\t\"\\\x41""#,
            r"AUTH user 'pa ss\'w'",
            r#"SET a"b c" x"#,
            "  PING  ",
        ] {
            assert_eq!(
                split_args(line).ok(),
                Some(split_args_lenient(line)),
                "{line}"
            );
        }
    }

    #[test]
    fn lenient_recovers() {
        assert_eq!(lenient(r#"AUTH "abc def"#), ["AUTH", "abc def"]);
        let args = split_args_lenient(r#"AUTH "abc def"#);
        assert_eq!(args[1].content_end, r#"AUTH "abc def"#.len());
        assert_eq!(lenient(r#"AUTH "pw"tail next"#), ["AUTH", "pwtail", "next"]);
    }

    #[test]
    fn quoting_round_trips() -> Result<(), SplitError> {
        let cases: Vec<Vec<u8>> = vec![
            b"plain".to_vec(),
            b"".to_vec(),
            b"two words".to_vec(),
            b"quote\"and'single".to_vec(),
            b"back\\slash".to_vec(),
            b"line\nbreak\ttab\r".to_vec(),
            vec![0, 1, 0x7f, 0xff, 0xfe, b'a'],
            "caf\u{e9} \u{1F600}".as_bytes().to_vec(),
            b"#notacomment".to_vec(),
        ];
        let line = join_args(&cases);
        let back: Vec<Vec<u8>> = split_args(&line)?.into_iter().map(|a| a.value).collect();
        assert_eq!(back, cases, "{line}");
        assert_eq!(quote_arg(b"plain"), "plain");
        assert_eq!(quote_arg(b"#tag"), "\"#tag\"");
        let line = join_args(&[b"#cmd".as_slice(), b"x"]);
        let found: Vec<Line<'_>> = lines(&line).collect();
        assert!(found.iter().all(|l| !l.comment), "{line}");
        assert_eq!(quote_arg(b""), "\"\"");
        Ok(())
    }

    #[test]
    fn lines_keep_offsets_and_mark_comments() {
        let script = "# setup\r\n\nSET a 1\r\n   # indented\n  GET a\n\n";
        let found: Vec<Line<'_>> = lines(script).collect();
        assert_eq!(
            found
                .iter()
                .map(|l| (l.number, l.comment, l.text))
                .collect::<Vec<_>>(),
            [
                (1, true, "# setup"),
                (3, false, "SET a 1"),
                (4, true, "   # indented"),
                (5, false, "  GET a"),
            ]
        );
        assert_eq!(&script[found[3].offset..found[3].offset + 7], "  GET a");
        assert_eq!(found[2].comment_body_offset(), Some(4));
    }

    #[test]
    fn init_commands_are_connection_setup_only() {
        for bad in [
            "MULTI",
            "exec",
            "QUIT",
            "RESET",
            "SUBSCRIBE news",
            "MONITOR",
            "CLIENT REPLY OFF",
            "CLIENT PAUSE 3000 ALL",
            "CLIENT KILL ID 5",
            "CLIENT TRACKING ON",
            "BLPOP q 0",
            "XREAD COUNT 1 STREAMS s 0",
            "SHUTDOWN NOSAVE",
            "flushall",
            "CONFIG SET maxmemory 1",
            "CONFIG GET maxmemory",
            "DEL k",
            "MOVE k 1",
            "FUNCTION LOAD x",
            "SAVE",
            "BGSAVE",
            "SORT k",
            "FT.DROPINDEX idx",
            "ACL LOAD",
            "SLOWLOG RESET",
            "PING",
            "EVAL \"return 1\" 0",
        ] {
            let script = format!("SELECT 1\n  {bad}");
            let problem = check_init_script(&script).err();
            let problem = problem.unwrap_or_else(|| panic!("{bad} accepted"));
            assert_eq!((problem.line, problem.offset), (2, 11), "{bad}");
            let name = bad
                .split(' ')
                .next()
                .unwrap_or_default()
                .to_ascii_uppercase();
            assert!(
                problem.to_string().starts_with(&format!(
                    "Line 2: {name} cannot be an init command: it is not a connection setup command"
                )),
                "{problem}"
            );
        }
        for (hello, reason) in [
            ("HELLO 3", "switches the protocol version"),
            (
                "HELLO SETNAME n",
                "needs the protocol version as its first argument",
            ),
            ("hello 3 AUTH u p", "puts a password in the init text"),
            (
                "HELLO 2 AUTH default pw SETNAME n",
                "puts a password in the init text",
            ),
            (
                "HELLO 2 SETNAME n AUTH u p",
                "puts a password in the init text",
            ),
            ("HELLO AUTH default pw", "puts a password in the init text"),
            (
                "hello auth u p setname n",
                "puts a password in the init text",
            ),
            ("SELECT 70000", "outside 0 to 65535"),
            ("SELECT -1", "outside 0 to 65535"),
            ("AUTH pw", "puts a password in the init text"),
            ("auth user pw", "puts a password in the init text"),
        ] {
            let problem = check_init_script(hello).err();
            assert!(
                problem.as_ref().is_some_and(|p| p.message.contains(reason)),
                "{hello}: {problem:?}"
            );
        }
        assert_eq!(hello_problem(&[b"HELLO".as_slice()]), None);
        assert_eq!(hello_problem(&[b"HELLO".as_slice(), b"2"]), None);
        assert!(hello_problem(&[b"HELLO".as_slice(), b"AUTH"]).is_some());
        for good in [
            "SELECT 3",
            "CLIENT SETNAME x",
            "client setinfo lib-name varve",
            "CLIENT NO-EVICT on",
            "CLIENT NO-TOUCH ON",
            "HELLO 2",
            "HELLO 2 SETNAME n",
            "HELLO 2 SETNAME auth",
            "SELECT 65535",
            "HELLO",
            "READONLY",
            "READWRITE",
            "# MULTI in a comment\nSELECT 2",
            "",
        ] {
            assert_eq!(check_init_script(good), Ok(()), "{good}");
        }
    }

    #[test]
    fn unparsable_init_lines_are_reported() {
        let problem = check_init_script("SELECT 1\nSET k \"open").err();
        assert_eq!(
            problem,
            Some(InitProblem {
                line: 2,
                offset: 15,
                message: "unterminated double quote".into()
            })
        );
    }
}
