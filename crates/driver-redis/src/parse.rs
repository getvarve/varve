//! Console text parsing, compatible with `redis-cli`, and lossless key
//! display text.

use varve_driver::{DriverError, ErrorKind};
pub use varve_redis_syntax::init_command_problem;
use varve_redis_syntax::{lines, split_args};

/// One command of a console script: its arguments and where it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCommand {
    /// Arguments, the command name first. Never empty.
    pub args: Vec<Vec<u8>>,
    /// 1-based line number in the script.
    pub line: usize,
    /// 1-based character position of the command's first character in the
    /// script.
    pub position: usize,
}

impl ParsedCommand {
    /// Upper-cased command name.
    pub fn name(&self) -> String {
        self.args
            .first()
            .map(|n| String::from_utf8_lossy(n).to_ascii_uppercase())
            .unwrap_or_default()
    }

    /// Upper-cased first argument, used as the subcommand of container
    /// commands such as `CONFIG SET`.
    pub fn sub(&self) -> Option<String> {
        self.args
            .get(1)
            .map(|s| String::from_utf8_lossy(s).to_ascii_uppercase())
    }
}

/// Splits a console script into commands: one per line, skipping blank lines
/// and lines whose first non-blank character is `#`. Lines are split with
/// [`varve_redis_syntax::split_args`], the splitter the activity redactor
/// uses too, so both always see the same arguments.
///
/// # Errors
///
/// [`ErrorKind::Query`] for a line with an unterminated quote or a closing
/// quote that is not followed by a space. `position` is the 1-based
/// character position of the offending quote in `text`.
pub fn parse_script(text: &str) -> Result<Vec<ParsedCommand>, DriverError> {
    let mut commands = Vec::new();
    let mut chars = CharPositions::new(text);
    for line in lines(text) {
        if line.comment {
            continue;
        }
        match split_args(line.text) {
            Ok(args) => {
                let Some(first) = args.first() else {
                    continue;
                };
                let position = chars.position(line.offset + first.start);
                commands.push(ParsedCommand {
                    args: args.into_iter().map(|arg| arg.value).collect(),
                    line: line.number,
                    position,
                });
            }
            Err(error) => {
                let position = chars.position(line.offset + error.offset);
                let mut driver_error = DriverError::new(
                    ErrorKind::Query,
                    format!("Line {}: {}", line.number, error.message),
                );
                driver_error.position = u32::try_from(position).ok();
                return Err(driver_error);
            }
        }
    }
    Ok(commands)
}

/// Turns increasing byte offsets into 1-based character positions without
/// counting the script from the start every time.
struct CharPositions<'a> {
    text: &'a str,
    byte: usize,
    chars: usize,
}

impl<'a> CharPositions<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            byte: 0,
            chars: 0,
        }
    }

    /// 1-based character position of byte offset `byte`, which must be on a
    /// character boundary and not before the previous offset asked for.
    fn position(&mut self, byte: usize) -> usize {
        if byte < self.byte {
            self.byte = 0;
            self.chars = 0;
        }
        self.chars += self
            .text
            .get(self.byte..byte)
            .map_or(0, |part| part.chars().count());
        self.byte = byte;
        self.chars + 1
    }
}

/// Parses the init commands of a connection profile and checks each with
/// [`init_command_problem`] (by the typed name; the driver checks again
/// by the real name when it connects). The same rules as
/// [`varve_redis_syntax::check_init_script`], which profile validation
/// uses, with the driver's error shape and character positions.
///
/// # Errors
///
/// [`ErrorKind::Query`] for unparsable text or a command that cannot be an
/// init command, with its position.
pub fn check_init_script(text: &str) -> Result<Vec<ParsedCommand>, DriverError> {
    let commands = parse_script(text)?;
    for command in &commands {
        if let Some(problem) = init_command_problem(&command.name(), &command.args) {
            return Err(init_error(command, &command.name(), problem));
        }
    }
    Ok(commands)
}

/// The error for an init command that cannot run.
pub(crate) fn init_error(command: &ParsedCommand, name: &str, problem: &str) -> DriverError {
    let mut error = DriverError::new(
        ErrorKind::Query,
        format!(
            "Init command on line {}: {name} cannot be an init command: it {problem}",
            command.line
        ),
    );
    error.position = u32::try_from(command.position).ok();
    error
}

/// Lossless display text for a key: valid UTF-8 stays as is, a backslash
/// becomes `\\`, and control characters and bytes that are not valid UTF-8
/// become `\xHH`. [`key_from_display`] reverses it.
#[must_use]
pub fn key_display(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut rest = bytes;
    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(text) => {
                escape_text(text, &mut out);
                break;
            }
            Err(error) => {
                let (valid, invalid) = rest.split_at(error.valid_up_to());
                // `valid` is valid UTF-8 by construction.
                escape_text(&String::from_utf8_lossy(valid), &mut out);
                let bad = error.error_len().unwrap_or(invalid.len());
                for byte in &invalid[..bad] {
                    out.push_str(&format!("\\x{byte:02x}"));
                }
                rest = &invalid[bad..];
            }
        }
    }
    out
}

fn escape_text(text: &str, out: &mut String) {
    for c in text.chars() {
        if c == '\\' {
            out.push_str("\\\\");
        } else if c.is_control() && u32::from(c) < 0x80 {
            out.push_str(&format!("\\x{:02x}", u32::from(c)));
        } else {
            out.push(c);
        }
    }
}

/// Turns display text from [`key_display`] back into the key bytes.
///
/// # Errors
///
/// [`ErrorKind::Query`] for a backslash that does not start `\\` or `\xHH`.
pub fn key_from_display(text: &str) -> Result<Vec<u8>, DriverError> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '\\' {
            push_char(&mut out, c);
            i += 1;
            continue;
        }
        match chars.get(i + 1) {
            Some('\\') => {
                out.push(b'\\');
                i += 2;
            }
            Some('x') => match hex_byte(chars.get(i + 2), chars.get(i + 3)) {
                Some(byte) => {
                    out.push(byte);
                    i += 4;
                }
                None => return Err(bad_escape()),
            },
            _ => return Err(bad_escape()),
        }
    }
    Ok(out)
}

fn push_char(out: &mut Vec<u8>, c: char) {
    let mut buf = [0u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
}

fn hex_byte(high: Option<&char>, low: Option<&char>) -> Option<u8> {
    let high = high?.to_digit(16)?;
    let low = low?.to_digit(16)?;
    u8::try_from(high * 16 + low).ok()
}

fn bad_escape() -> DriverError {
    DriverError::new(
        ErrorKind::Query,
        "Invalid escape in key text: use \\\\ or \\xHH",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Arguments of the only command of a one-line script.
    fn args(line: &str) -> Vec<Vec<u8>> {
        match parse_script(line) {
            Ok(mut commands) if commands.len() == 1 => commands.remove(0).args,
            other => panic!("{line:?}: {other:?}"),
        }
    }

    fn strs(line: &str) -> Vec<String> {
        args(line)
            .into_iter()
            .map(|a| String::from_utf8_lossy(&a).into_owned())
            .collect()
    }

    fn error_at(script: &str) -> (String, Option<u32>) {
        match parse_script(script) {
            Err(error) => {
                assert_eq!(error.kind, ErrorKind::Query);
                (error.message, error.position)
            }
            Ok(commands) => panic!("{script:?} parsed: {commands:?}"),
        }
    }

    #[test]
    fn splits_on_whitespace() -> Result<(), DriverError> {
        assert_eq!(strs("  SET  key \t value  "), ["SET", "key", "value"]);
        assert!(parse_script("   ")?.is_empty());
        Ok(())
    }

    #[test]
    fn vertical_tab_and_form_feed_separate_like_c_isspace() -> Result<(), DriverError> {
        assert_eq!(strs("SET\u{b}k\u{c}v"), ["SET", "k", "v"]);
        assert!(parse_script("\u{b}\u{c}\n")?.is_empty());
        let commands = parse_script("\u{c}\u{b}PING")?;
        assert_eq!(commands[0].position, 3);
        Ok(())
    }

    /// Deliberately not `redis-cli` behaviour: `redis-cli` reads a line as a
    /// C string and would end it at the NUL. Varve separates there, so the
    /// driver and the activity redactor see the same arguments and no text
    /// after a NUL is silently dropped.
    #[test]
    fn nul_separates_arguments_instead_of_ending_the_line() -> Result<(), DriverError> {
        assert_eq!(strs("AUTH\0hunter2"), ["AUTH", "hunter2"]);
        assert_eq!(strs("SET\0k \0 v\0"), ["SET", "k", "v"]);
        // A line of separators only is blank.
        assert!(parse_script("\0\0\n \0 ")?.is_empty());
        // The position skips leading separators.
        let commands = parse_script("\0\0PING")?;
        assert_eq!(commands[0].position, 3);
        Ok(())
    }

    #[test]
    fn double_quotes_take_escapes() {
        assert_eq!(
            strs(r#"SET k "a b\n\r\t\"\\ \x41\x7a""#),
            ["SET", "k", "a b\n\r\t\"\\ Az"]
        );
        assert_eq!(args(r#""\xff\x00""#), vec![vec![0xff, 0x00]]);
        assert_eq!(strs(r#""\q""#), ["q"]);
        assert_eq!(strs(r#""\xzz""#), ["xzz"]);
        assert_eq!(strs(r#""" x"#), ["", "x"]);
    }

    #[test]
    fn single_quotes_are_literal() {
        assert_eq!(strs(r"'a\nb' 'it\'s'"), [r"a\nb", "it's"]);
        assert_eq!(strs("''"), [""]);
    }

    #[test]
    fn quotes_can_start_mid_token_like_redis_cli() {
        assert_eq!(strs(r#"foo"bar baz""#), ["foobar baz"]);
        assert_eq!(strs("héllo wörld"), ["héllo", "wörld"]);
    }

    #[test]
    fn rejects_bad_quotes() {
        assert_eq!(
            error_at(r#"SET k "abc"#),
            ("Line 1: unterminated double quote".into(), Some(7))
        );
        assert_eq!(
            error_at("SET k 'abc"),
            ("Line 1: unterminated single quote".into(), Some(7))
        );
        assert_eq!(
            error_at(r#"SET "a"b"#),
            (
                "Line 1: closing quote must be followed by a space".into(),
                Some(7)
            )
        );
        assert_eq!(error_at("SET 'a'b").1, Some(7));
        assert_eq!(error_at(r#"SET "a\""#).1, Some(5));
    }

    #[test]
    fn script_skips_blank_and_comment_lines() -> Result<(), DriverError> {
        let commands = parse_script("# setup\r\n\nSET a 1\r\n   # indented\n  GET a\n\n")?;
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].line, 3);
        assert_eq!(commands[0].name(), "SET");
        assert_eq!(commands[0].position, 11);
        assert_eq!(commands[1].line, 5);
        assert_eq!(commands[1].position, 36);
        assert_eq!(commands[1].args, vec![b"GET".to_vec(), b"a".to_vec()]);
        assert!(parse_script("\n  \n# only\n")?.is_empty());
        Ok(())
    }

    #[test]
    fn script_errors_point_at_the_quote() {
        let (message, position) = error_at("PING\nSET k \"oops");
        assert!(message.starts_with("Line 2:"), "{message}");
        // "PING\n" is 5 characters, the quote is at offset 6 of line 2.
        assert_eq!(position, Some(12));
    }

    #[test]
    fn positions_count_characters_not_bytes() -> Result<(), DriverError> {
        // "é" is two bytes but one character.
        let commands = parse_script("SET é ü\r\n  GET é\nGET 'ü'")?;
        assert_eq!(
            commands.iter().map(|c| c.position).collect::<Vec<_>>(),
            [1, 12, 18]
        );
        let (message, position) = error_at("SET é ü\nSET ü \"open");
        assert!(message.starts_with("Line 2:"), "{message}");
        assert_eq!(position, Some(15));
        Ok(())
    }

    #[test]
    fn subcommand_and_name_are_uppercased() -> Result<(), DriverError> {
        let commands = parse_script("config set maxmemory 1")?;
        assert_eq!(commands[0].name(), "CONFIG");
        assert_eq!(commands[0].sub().as_deref(), Some("SET"));
        Ok(())
    }

    #[test]
    fn init_commands_that_break_the_connection_are_refused() -> Result<(), DriverError> {
        for bad in [
            "MULTI",
            "exec",
            "DISCARD",
            "WATCH k",
            "QUIT",
            "RESET",
            "SUBSCRIBE news",
            "psubscribe *",
            "MONITOR",
            "PSYNC ? -1",
            "CLIENT REPLY OFF",
            "client reply skip",
            "HELLO 3",
            "BLPOP q 0",
            "BZPOPMIN z 1",
            "WAIT 1 0",
            "XREAD BLOCK 0 STREAMS s $",
            "xreadgroup group g c block 10 streams s >",
            "SHUTDOWN NOSAVE",
            "FLUSHALL",
            "CLIENT PAUSE 3000",
            "CONFIG SET maxmemory 1",
        ] {
            let error = check_init_script(&format!("SELECT 1\n{bad}")).err();
            let error = error.unwrap_or_else(|| panic!("{bad} accepted"));
            assert!(
                error.message.starts_with("Init command on line 2:"),
                "{}",
                error.message
            );
            assert_eq!(error.position, Some(10));
        }
        for good in [
            "SELECT 3",
            "CLIENT SETNAME x",
            "CLIENT NO-TOUCH ON",
            "HELLO 2",
            "HELLO",
            "CLIENT NO-EVICT on",
            "READONLY",
        ] {
            assert_eq!(check_init_script(good)?.len(), 1, "{good}");
        }
        Ok(())
    }

    #[test]
    fn key_display_round_trips() -> Result<(), DriverError> {
        let samples: Vec<Vec<u8>> = vec![
            b"user:1".to_vec(),
            "naïve:ключ".as_bytes().to_vec(),
            vec![0xff, b'a', 0x00, b'\\', b'x', 0xc3],
            b"back\\slash\\x41".to_vec(),
            vec![b'\n', 0x7f, 0xe2, 0x82],
            Vec::new(),
        ];
        for sample in samples {
            let text = key_display(&sample);
            assert_eq!(key_from_display(&text)?, sample, "{text}");
        }
        assert_eq!(key_display(&[0xff, b'a']), "\\xffa");
        assert_eq!(key_display(b"a\\b"), "a\\\\b");
        assert_eq!(key_display("é".as_bytes()), "é");
        assert!(key_from_display("bad\\q").is_err());
        assert!(key_from_display("bad\\x4").is_err());
        Ok(())
    }
}
