//! Turns RESP replies into console results and cell values.

use redis::Value as Resp;
use varve_driver::{Column, DriverError, ErrorKind, StatementResult, Value};

fn column(name: &str, type_name: &str) -> Column {
    Column {
        name: name.into(),
        type_name: type_name.into(),
    }
}

/// Text when the bytes are valid UTF-8, otherwise raw bytes.
pub fn bytes_value(bytes: Vec<u8>) -> Value {
    match String::from_utf8(bytes) {
        Ok(text) => Value::Text(text),
        Err(error) => Value::Bytes(error.into_bytes()),
    }
}

/// A double as a cell value. JSON has no infinity or NaN, so those become
/// [`Value::Other`] with the text Redis uses (`inf`, `-inf`, `nan`) instead
/// of a float that would serialize as `null`.
pub fn double_value(f: f64) -> Value {
    if f.is_finite() {
        Value::Float(f)
    } else if f.is_nan() {
        Value::Other("nan".into())
    } else if f.is_sign_positive() {
        Value::Other("inf".into())
    } else {
        Value::Other("-inf".into())
    }
}

/// The server error text as `redis-cli` shows it: `CODE detail`.
pub fn server_error_text(error: &redis::ServerError) -> String {
    match error.details() {
        Some(detail) => format!("{} {detail}", error.code()),
        None => error.code().to_string(),
    }
}

/// Most values converted for one console command or one browser page
/// (1,000,000), counted at every depth: an array counts once plus its
/// elements. The wire limit ([`crate::MAX_REPLY_BYTES`]) alone allows
/// several million tiny values, each of which takes far more memory decoded
/// than on the wire; this caps that.
pub const MAX_RESULT_VALUES: usize = 1_000_000;

/// Values left to convert; see [`MAX_RESULT_VALUES`].
#[derive(Debug)]
pub struct ValueBudget {
    left: usize,
}

impl ValueBudget {
    /// A budget of [`MAX_RESULT_VALUES`].
    pub fn new() -> Self {
        Self::with_limit(MAX_RESULT_VALUES)
    }

    fn with_limit(left: usize) -> Self {
        Self { left }
    }

    /// Takes one value; an error once the budget is used up.
    fn take(&mut self) -> Result<(), DriverError> {
        match self.left.checked_sub(1) {
            Some(left) => {
                self.left = left;
                Ok(())
            }
            None => Err(too_many_values()),
        }
    }
}

impl Default for ValueBudget {
    fn default() -> Self {
        Self::new()
    }
}

/// Error for a reply with more than [`MAX_RESULT_VALUES`] values.
pub fn too_many_values() -> DriverError {
    DriverError::new(
        ErrorKind::Query,
        format!(
            "The reply has more than {MAX_RESULT_VALUES} values, the most Varve shows for one \
             command. Read it in parts instead, for example with SCAN, HSCAN, SSCAN, LRANGE, \
             ZRANGE or XRANGE ... COUNT"
        ),
    )
}

/// A cell value for a reply nested in an array or map, charged to
/// `budget` at every depth.
///
/// # Errors
///
/// [`too_many_values`] once the budget is used up.
pub fn to_value(reply: Resp, budget: &mut ValueBudget) -> Result<Value, DriverError> {
    budget.take()?;
    Ok(match reply {
        Resp::Nil => Value::Null,
        Resp::Int(i) => Value::Int(i),
        Resp::BulkString(bytes) => bytes_value(bytes),
        Resp::SimpleString(text) => Value::Text(text),
        Resp::Okay => Value::Text("OK".into()),
        Resp::Double(f) => double_value(f),
        Resp::Boolean(b) => Value::Bool(b),
        Resp::VerbatimString { text, .. } => Value::Text(text),
        Resp::BigNumber(digits) => Value::Decimal(String::from_utf8_lossy(&digits).into_owned()),
        Resp::Array(items) | Resp::Set(items) | Resp::Push { data: items, .. } => Value::Array(
            items
                .into_iter()
                .map(|item| to_value(item, budget))
                .collect::<Result<_, _>>()?,
        ),
        Resp::Map(pairs) => Value::Array(
            pairs
                .into_iter()
                .map(|(k, v)| {
                    budget.take()?;
                    Ok(Value::Array(vec![
                        to_value(k, budget)?,
                        to_value(v, budget)?,
                    ]))
                })
                .collect::<Result<_, DriverError>>()?,
        ),
        Resp::Attribute { data, .. } => to_value(*data, budget)?,
        Resp::ServerError(error) => Value::Other(format!("(error) {}", server_error_text(&error))),
        // Reply kinds added by future versions of the protocol library.
        other => Value::Other(format!("{other:?}")),
    })
}

/// The console result for a top-level reply that is not an error. At most
/// `max_rows` top-level entries become rows (the result is then marked
/// truncated), and at most [`MAX_RESULT_VALUES`] values are converted.
///
/// # Errors
///
/// [`too_many_values`] when the kept rows hold more values than that.
pub fn to_result(reply: Resp, max_rows: usize) -> Result<StatementResult, DriverError> {
    convert_result(reply, max_rows, &mut ValueBudget::new())
}

fn convert_result(
    reply: Resp,
    max_rows: usize,
    budget: &mut ValueBudget,
) -> Result<StatementResult, DriverError> {
    Ok(match reply {
        Resp::Okay => StatementResult::Command {
            tag: "OK".into(),
            rows_affected: None,
        },
        Resp::SimpleString(text) => StatementResult::Command {
            tag: text,
            rows_affected: None,
        },
        Resp::Int(i) => single("integer", "integer", Value::Int(i)),
        Resp::Nil => single("value", "nil", Value::Null),
        Resp::BulkString(bytes) => single("value", "bulk", bytes_value(bytes)),
        Resp::VerbatimString { text, .. } => single("value", "verbatim", Value::Text(text)),
        Resp::Double(f) => single("value", "double", double_value(f)),
        Resp::Boolean(b) => single("value", "boolean", Value::Bool(b)),
        Resp::BigNumber(digits) => single(
            "value",
            "big_number",
            Value::Decimal(String::from_utf8_lossy(&digits).into_owned()),
        ),
        Resp::Attribute { data, .. } => convert_result(*data, max_rows, budget)?,
        Resp::ServerError(error) => single(
            "value",
            "error",
            Value::Other(format!("(error) {}", server_error_text(&error))),
        ),
        Resp::Array(items) | Resp::Set(items) | Resp::Push { data: items, .. } => {
            let truncated = items.len() > max_rows;
            let rows = items
                .into_iter()
                .take(max_rows)
                .enumerate()
                .map(|(i, item)| {
                    Ok(vec![
                        Value::Int(i64::try_from(i + 1).unwrap_or(i64::MAX)),
                        to_value(item, budget)?,
                    ])
                })
                .collect::<Result<_, DriverError>>()?;
            StatementResult::Rows {
                columns: vec![column("#", "integer"), column("value", "value")],
                rows,
                truncated,
            }
        }
        Resp::Map(pairs) => {
            let truncated = pairs.len() > max_rows;
            let rows = pairs
                .into_iter()
                .take(max_rows)
                .map(|(k, v)| Ok(vec![to_value(k, budget)?, to_value(v, budget)?]))
                .collect::<Result<_, DriverError>>()?;
            StatementResult::Rows {
                columns: vec![column("key", "value"), column("value", "value")],
                rows,
                truncated,
            }
        }
        // Reply kinds added by future versions of the protocol library.
        other => single("value", "other", Value::Other(format!("{other:?}"))),
    })
}

fn single(name: &str, type_name: &str, value: Value) -> StatementResult {
    StatementResult::Rows {
        columns: vec![column(name, type_name)],
        rows: vec![vec![value]],
        truncated: false,
    }
}

/// Text of a status or bulk string reply.
pub fn as_text(reply: &Resp) -> Option<String> {
    match reply {
        Resp::BulkString(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        Resp::SimpleString(text) | Resp::VerbatimString { text, .. } => Some(text.clone()),
        Resp::Okay => Some("OK".into()),
        Resp::Int(i) => Some(i.to_string()),
        _ => None,
    }
}

/// Integer value of an integer reply or a numeric string reply.
pub fn as_int(reply: &Resp) -> Option<i64> {
    match reply {
        Resp::Int(i) => Some(*i),
        Resp::BulkString(_) | Resp::SimpleString(_) => as_text(reply)?.trim().parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_result_ok(reply: Resp, max_rows: usize) -> StatementResult {
        to_result(reply, max_rows).unwrap_or_else(|e| panic!("{e:?}"))
    }

    fn value(reply: Resp) -> Value {
        to_value(reply, &mut ValueBudget::new()).unwrap_or_else(|e| panic!("{e:?}"))
    }

    #[test]
    fn nested_values_count_against_the_budget() {
        // One top-level row holding a million and one integers: max_rows
        // does not help, the value budget does.
        let huge = Resp::Array(vec![Resp::Array(vec![Resp::Int(1); MAX_RESULT_VALUES])]);
        let error = to_result(huge, 10).err();
        assert_eq!(error.as_ref().map(|e| e.kind), Some(ErrorKind::Query));
        assert!(
            error.is_some_and(|e| e.message.contains("more than 1000000 values")),
            "the nested array must be refused"
        );
        // Exactly at the limit: the inner array and its elements fit.
        let fits = Resp::Array(vec![Resp::Array(vec![Resp::Int(1); MAX_RESULT_VALUES - 1])]);
        assert!(to_result(fits, 10).is_ok());
        // Map entries count once per pair plus key and value.
        let mut budget = ValueBudget::with_limit(3);
        let map = Resp::Map(vec![(Resp::Int(1), Resp::Int(2))]);
        assert!(to_value(map.clone(), &mut budget).is_err());
        let mut budget = ValueBudget::with_limit(4);
        assert!(to_value(map, &mut budget).is_ok());
        // Rows dropped by max_rows are not converted at all.
        let wide = Resp::Array(vec![Resp::Int(1); MAX_RESULT_VALUES * 2]);
        assert!(to_result(wide, 5).is_ok());
    }

    fn rows(result: StatementResult) -> (Vec<String>, Vec<Vec<Value>>, bool) {
        match result {
            StatementResult::Rows {
                columns,
                rows,
                truncated,
            } => (
                columns.into_iter().map(|c| c.name).collect(),
                rows,
                truncated,
            ),
            StatementResult::Command { tag, .. } => panic!("expected rows, got command {tag}"),
        }
    }

    #[test]
    fn status_replies_are_commands() {
        assert_eq!(
            to_result_ok(Resp::Okay, 10),
            StatementResult::Command {
                tag: "OK".into(),
                rows_affected: None
            }
        );
        assert_eq!(
            to_result_ok(Resp::SimpleString("PONG".into()), 10),
            StatementResult::Command {
                tag: "PONG".into(),
                rows_affected: None
            }
        );
    }

    #[test]
    fn scalars_are_single_cells() {
        assert_eq!(
            rows(to_result_ok(Resp::Int(7), 10)),
            (vec!["integer".into()], vec![vec![Value::Int(7)]], false)
        );
        assert_eq!(
            rows(to_result_ok(Resp::BulkString(b"hi".to_vec()), 10)).1,
            vec![vec![Value::Text("hi".into())]]
        );
        assert_eq!(
            rows(to_result_ok(Resp::BulkString(vec![0xff]), 10)).1,
            vec![vec![Value::Bytes(vec![0xff])]]
        );
        assert_eq!(rows(to_result_ok(Resp::Nil, 10)).1, vec![vec![Value::Null]]);
        assert_eq!(
            rows(to_result_ok(Resp::Double(1.5), 10)).1,
            vec![vec![Value::Float(1.5)]]
        );
        assert_eq!(
            rows(to_result_ok(Resp::Boolean(true), 10)).1,
            vec![vec![Value::Bool(true)]]
        );
    }

    #[test]
    fn arrays_are_numbered_and_truncated() {
        let reply = Resp::Array(vec![
            Resp::BulkString(b"a".to_vec()),
            Resp::Array(vec![Resp::Int(1), Resp::Nil]),
            Resp::Int(3),
        ]);
        let (columns, data, truncated) = rows(to_result_ok(reply.clone(), 10));
        assert_eq!(columns, ["#", "value"]);
        assert!(!truncated);
        assert_eq!(
            data[1],
            vec![
                Value::Int(2),
                Value::Array(vec![Value::Int(1), Value::Null])
            ]
        );
        let (_, data, truncated) = rows(to_result_ok(reply, 2));
        assert!(truncated);
        assert_eq!(data.len(), 2);
        let (_, empty, _) = rows(to_result_ok(Resp::Array(Vec::new()), 2));
        assert!(empty.is_empty());
    }

    #[test]
    fn maps_have_key_and_value_columns() {
        let reply = Resp::Map(vec![
            (
                Resp::SimpleString("server".into()),
                Resp::BulkString(b"redis".to_vec()),
            ),
            (Resp::BulkString(b"proto".to_vec()), Resp::Int(3)),
        ]);
        let (columns, data, truncated) = rows(to_result_ok(reply, 1));
        assert_eq!(columns, ["key", "value"]);
        assert!(truncated);
        assert_eq!(
            data,
            vec![vec![
                Value::Text("server".into()),
                Value::Text("redis".into())
            ]]
        );
    }

    #[test]
    fn non_finite_doubles_keep_their_text() -> Result<(), serde_json::Error> {
        for (f, text) in [
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
            (f64::NAN, "nan"),
        ] {
            let top = rows(to_result_ok(Resp::Double(f), 10)).1;
            assert_eq!(top, vec![vec![Value::Other(text.into())]]);
            let nested = value(Resp::Array(vec![Resp::Double(f)]));
            assert_eq!(nested, Value::Array(vec![Value::Other(text.into())]));
            let json = serde_json::to_string(&double_value(f))?;
            assert!(!json.contains("null"), "{json}");
        }
        assert_eq!(double_value(-0.5), Value::Float(-0.5));
        Ok(())
    }

    #[test]
    fn numeric_helpers() {
        assert_eq!(as_int(&Resp::BulkString(b" 42".to_vec())), Some(42));
        assert_eq!(as_int(&Resp::Nil), None);
        assert_eq!(as_text(&Resp::Okay).as_deref(), Some("OK"));
    }
}
