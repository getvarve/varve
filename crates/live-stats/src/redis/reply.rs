//! Running commands through [`Session::execute`] and reading the replies
//! back as [`Value`]s.
//!
//! The Redis driver turns a reply into a console result: a status reply
//! becomes a command tag, a scalar a single cell, an array numbered rows
//! (`#`, `value`) and a RESP3 map key/value rows. [`reply_value`] rebuilds
//! the reply from that shape. Nested maps arrive as arrays of two-element
//! arrays (RESP3) or as flat `key, value, ...` arrays (RESP2); [`pairs`]
//! reads both.

use varve_catalog::{CatalogError, CatalogResult};
use varve_driver::{ExecOptions, Session, StatementResult, Value};
use varve_redis_syntax::join_args;

/// Most top-level reply entries read for one command. The driver also caps
/// the number of converted values (one million), so this only guards
/// against a silently truncated reply.
const MAX_ROWS: usize = 1_000_000;

/// A command line built from raw arguments. Every argument is quoted with
/// [`varve_redis_syntax::quote_arg`], so the driver reads back exactly the
/// bytes given here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Cmd {
    args: Vec<Vec<u8>>,
}

impl Cmd {
    /// A command with its name.
    pub(crate) fn new(name: &str) -> Self {
        Self {
            args: vec![name.as_bytes().to_vec()],
        }
    }

    /// Adds one argument.
    pub(crate) fn arg(mut self, arg: impl AsRef<[u8]>) -> Self {
        self.args.push(arg.as_ref().to_vec());
        self
    }

    /// Adds several arguments.
    pub(crate) fn args<A: AsRef<[u8]>>(mut self, args: impl IntoIterator<Item = A>) -> Self {
        self.args
            .extend(args.into_iter().map(|a| a.as_ref().to_vec()));
        self
    }

    /// The console line.
    pub(crate) fn line(&self) -> String {
        join_args(&self.args)
    }
}

/// Runs one command and returns its reply.
///
/// # Errors
///
/// Driver errors, and [`CatalogError::Unexpected`] when the reply has an
/// unexpected shape.
pub(crate) async fn run(session: &dyn Session, cmd: &Cmd) -> CatalogResult<Value> {
    let output = session
        .execute(&cmd.line(), &ExecOptions { max_rows: MAX_ROWS })
        .await?;
    let mut results = output.results.into_iter();
    match (results.next(), results.next()) {
        (Some(result), None) => reply_value(result),
        _ => Err(unexpected("one reply per command")),
    }
}

/// Runs several commands in one call (one console line each) and returns
/// one reply per command, in order.
///
/// # Errors
///
/// As [`run`]; the first failing command fails the whole batch.
pub(crate) async fn run_batch(session: &dyn Session, cmds: &[Cmd]) -> CatalogResult<Vec<Value>> {
    if cmds.is_empty() {
        return Ok(Vec::new());
    }
    let script = cmds.iter().map(Cmd::line).collect::<Vec<_>>().join("\n");
    let output = session
        .execute(&script, &ExecOptions { max_rows: MAX_ROWS })
        .await?;
    if output.results.len() != cmds.len() {
        return Err(unexpected("one reply per command"));
    }
    output.results.into_iter().map(reply_value).collect()
}

/// Rebuilds a reply from the console result the driver made of it.
///
/// # Errors
///
/// [`CatalogError::Unexpected`] for a truncated result or an unknown shape.
pub(crate) fn reply_value(result: StatementResult) -> CatalogResult<Value> {
    match result {
        StatementResult::Command { tag, .. } => Ok(Value::Text(tag)),
        StatementResult::Rows {
            columns,
            rows,
            truncated,
        } => {
            if truncated {
                return Err(unexpected("the reply was truncated"));
            }
            let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
            match names.as_slice() {
                ["#", "value"] => rows
                    .into_iter()
                    .map(|row| {
                        row.into_iter()
                            .nth(1)
                            .ok_or_else(|| unexpected("short row"))
                    })
                    .collect::<CatalogResult<Vec<_>>>()
                    .map(Value::Array),
                ["key", "value"] => Ok(Value::Array(rows.into_iter().map(Value::Array).collect())),
                [_] => rows
                    .into_iter()
                    .next()
                    .and_then(|row| row.into_iter().next())
                    .ok_or_else(|| unexpected("empty single-value reply")),
                _ => Err(unexpected(format!(
                    "reply with columns {}",
                    names.join(", ")
                ))),
            }
        }
    }
}

/// An [`CatalogError::Unexpected`] error.
pub(crate) fn unexpected(message: impl Into<String>) -> CatalogError {
    CatalogError::Unexpected {
        message: message.into(),
    }
}

/// An [`CatalogError::Unsupported`] error.
pub(crate) fn unsupported(message: impl Into<String>) -> CatalogError {
    CatalogError::Unsupported {
        message: message.into(),
    }
}

/// An [`CatalogError::Invalid`] error.
pub(crate) fn invalid(message: impl Into<String>) -> CatalogError {
    CatalogError::Invalid {
        message: message.into(),
    }
}

/// Text of a string, number or status reply. Invalid UTF-8 is replaced.
pub(crate) fn text(value: &Value) -> Option<String> {
    match value {
        Value::Text(s) | Value::Decimal(s) | Value::Other(s) => Some(s.clone()),
        Value::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
        Value::Int(i) => Some(i.to_string()),
        Value::UInt(u) => Some(u.to_string()),
        Value::Float(f) => Some(f.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Raw bytes of a string reply (a key or channel name).
pub(crate) fn bytes(value: &Value) -> Option<Vec<u8>> {
    match value {
        Value::Text(s) => Some(s.as_bytes().to_vec()),
        Value::Bytes(b) => Some(b.clone()),
        Value::Int(i) => Some(i.to_string().into_bytes()),
        _ => None,
    }
}

/// A key or channel name as a cell value: text when it is valid UTF-8,
/// otherwise bytes.
pub(crate) fn name_value(raw: Vec<u8>) -> Value {
    match String::from_utf8(raw) {
        Ok(text) => Value::Text(text),
        Err(error) => Value::Bytes(error.into_bytes()),
    }
}

/// Integer of an integer reply or a numeric string.
pub(crate) fn int(value: &Value) -> Option<i64> {
    match value {
        Value::Int(i) => Some(*i),
        Value::UInt(u) => i64::try_from(*u).ok(),
        Value::Text(s) | Value::Decimal(s) => s.trim().parse().ok(),
        Value::Bytes(b) => std::str::from_utf8(b).ok()?.trim().parse().ok(),
        _ => None,
    }
}

/// Number of an integer, double or numeric string reply.
pub(crate) fn float(value: &Value) -> Option<f64> {
    match value {
        Value::Float(f) => Some(*f),
        #[allow(clippy::cast_precision_loss)]
        Value::Int(i) => Some(*i as f64),
        #[allow(clippy::cast_precision_loss)]
        Value::UInt(u) => Some(*u as f64),
        Value::Text(s) | Value::Decimal(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Elements of an array reply; `None` for nil.
pub(crate) fn array(value: &Value) -> Option<&[Value]> {
    match value {
        Value::Array(items) => Some(items),
        _ => None,
    }
}

/// Key/value pairs of a map reply: a RESP3 map (an array of two-element
/// arrays) or a RESP2 flat array (`key, value, key, value`).
pub(crate) fn pairs(value: &Value) -> Option<Vec<(String, &Value)>> {
    let items = array(value)?;
    let resp3 = items
        .first()
        .is_some_and(|first| matches!(first, Value::Array(pair) if pair.len() == 2));
    if resp3 {
        items
            .iter()
            .map(|item| match item {
                Value::Array(pair) => match pair.as_slice() {
                    [k, v] => Some((text(k)?, v)),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    } else {
        if items.len() % 2 != 0 {
            return None;
        }
        items
            .chunks(2)
            .map(|chunk| match chunk {
                [k, v] => Some((text(k)?, v)),
                _ => None,
            })
            .collect()
    }
}

/// Field lookup in a map reply.
pub(crate) struct Fields<'a> {
    entries: Vec<(String, &'a Value)>,
}

impl<'a> Fields<'a> {
    /// Reads a map reply.
    pub(crate) fn of(value: &'a Value) -> Option<Self> {
        pairs(value).map(|entries| Self { entries })
    }

    /// The value of `key`.
    pub(crate) fn get(&self, key: &str) -> Option<&'a Value> {
        self.entries
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| *v)
    }

    /// Text of `key`.
    pub(crate) fn text(&self, key: &str) -> Option<String> {
        self.get(key).and_then(text)
    }

    /// Integer of `key`.
    pub(crate) fn int(&self, key: &str) -> Option<i64> {
        self.get(key).and_then(int)
    }

    /// Number of `key`.
    pub(crate) fn float(&self, key: &str) -> Option<f64> {
        self.get(key).and_then(float)
    }

    /// All entries in reply order.
    pub(crate) fn entries(&self) -> &[(String, &'a Value)] {
        &self.entries
    }
}

/// True for a server error that means the command or subcommand is not
/// available: unknown, renamed away, or not allowed for this ACL user.
pub(crate) fn is_unavailable(error: &CatalogError) -> bool {
    match error {
        CatalogError::Driver { error } => {
            let code = error.code.as_deref().unwrap_or_default();
            code == "NOPERM"
                || (code == "ERR"
                    && (error.message.contains("unknown command")
                        || error.message.contains("Unknown subcommand")
                        || error.message.contains("unknown subcommand")))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use varve_driver::Column;

    use super::*;

    fn col(name: &str) -> Column {
        Column {
            name: name.into(),
            type_name: "value".into(),
        }
    }

    #[test]
    fn commands_are_quoted_losslessly() {
        let cmd = Cmd::new("SET")
            .arg("key with space")
            .arg([0xff_u8, b'"'])
            .arg("");
        assert_eq!(cmd.line(), r#"SET "key with space" "\xff\"" """#);
        let lines = Cmd::new("GET").arg("a\nb").line();
        assert!(!lines.contains('\n'));
    }

    #[test]
    fn rebuilds_replies_from_console_results() -> CatalogResult<()> {
        assert_eq!(
            reply_value(StatementResult::Command {
                tag: "OK".into(),
                rows_affected: None
            })?,
            Value::Text("OK".into())
        );
        let array = StatementResult::Rows {
            columns: vec![col("#"), col("value")],
            rows: vec![
                vec![Value::Int(1), Value::Text("a".into())],
                vec![Value::Int(2), Value::Int(5)],
            ],
            truncated: false,
        };
        assert_eq!(
            reply_value(array)?,
            Value::Array(vec![Value::Text("a".into()), Value::Int(5)])
        );
        let map = StatementResult::Rows {
            columns: vec![col("key"), col("value")],
            rows: vec![vec![Value::Text("k".into()), Value::Int(1)]],
            truncated: false,
        };
        let rebuilt = reply_value(map)?;
        assert_eq!(
            pairs(&rebuilt).map(|p| p.len()),
            Some(1),
            "a RESP3 map reads as pairs"
        );
        let single = StatementResult::Rows {
            columns: vec![col("integer")],
            rows: vec![vec![Value::Int(7)]],
            truncated: false,
        };
        assert_eq!(reply_value(single)?, Value::Int(7));
        let truncated = StatementResult::Rows {
            columns: vec![col("#"), col("value")],
            rows: Vec::new(),
            truncated: true,
        };
        assert!(reply_value(truncated).is_err());
        Ok(())
    }

    #[test]
    fn reads_resp2_and_resp3_maps() {
        let flat = Value::Array(vec![
            Value::Text("a".into()),
            Value::Int(1),
            Value::Text("b".into()),
            Value::Text("x".into()),
        ]);
        let nested = Value::Array(vec![
            Value::Array(vec![Value::Text("a".into()), Value::Int(1)]),
            Value::Array(vec![Value::Text("b".into()), Value::Text("x".into())]),
        ]);
        for value in [&flat, &nested] {
            let fields = Fields::of(value);
            assert_eq!(fields.as_ref().and_then(|f| f.int("a")), Some(1));
            assert_eq!(
                fields.as_ref().and_then(|f| f.text("B")).as_deref(),
                Some("x")
            );
        }
        assert!(pairs(&Value::Array(vec![Value::Int(1)])).is_none());
        assert_eq!(pairs(&Value::Array(Vec::new())).map(|p| p.len()), Some(0));
    }

    #[test]
    fn numbers_from_strings_and_doubles() {
        assert_eq!(int(&Value::Text(" 42".into())), Some(42));
        assert_eq!(float(&Value::Text("1.5".into())), Some(1.5));
        assert_eq!(float(&Value::Float(2.5)), Some(2.5));
        assert_eq!(int(&Value::Null), None);
    }
}
