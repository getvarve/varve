//! Cell values, engine-independent and shaped for the UI and exports.

use serde::{Deserialize, Serialize, Serializer};

/// A cell value.
///
/// Numbers that do not fit an `i64`/`u64`/`f64` exactly (PostgreSQL
/// `numeric`, MySQL `DECIMAL`) and temporal values are carried as their
/// canonical text, so nothing is rounded or shifted between the server and
/// the grid. Serialized as `{"t": "<type>", "v": <value>}`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "t", content = "v", rename_all = "camelCase")]
pub enum Value {
    /// SQL `NULL` or Redis nil.
    Null,
    /// Boolean.
    Bool(bool),
    /// Signed integer.
    Int(i64),
    /// Unsigned integer that does not fit an `i64`.
    UInt(u64),
    /// Floating point number.
    Float(f64),
    /// Exact decimal, as text.
    Decimal(String),
    /// Text.
    Text(String),
    /// Binary data, serialized as lowercase hex.
    #[serde(serialize_with = "hex", deserialize_with = "unhex")]
    Bytes(Vec<u8>),
    /// Calendar date, ISO 8601 (`2026-10-19`).
    Date(String),
    /// Time of day, ISO 8601 (`13:45:00.123`), with offset when the type has one.
    Time(String),
    /// Timestamp without time zone, ISO 8601 (`2026-10-19T13:45:00`).
    Timestamp(String),
    /// Timestamp with time zone, ISO 8601 in UTC (`2026-10-19T11:45:00Z`).
    TimestampTz(String),
    /// Interval or duration, as the server formats it.
    Interval(String),
    /// UUID, lowercase hyphenated.
    Uuid(String),
    /// JSON document.
    Json(serde_json::Value),
    /// Array of values (PostgreSQL arrays, Redis multi-bulk replies).
    Array(Vec<Value>),
    /// Any other type, as the server's text form.
    Other(String),
}

impl Value {
    /// Text for CSV export and copy: `None` for `NULL`.
    #[must_use]
    pub fn to_plain_text(&self) -> Option<String> {
        Some(match self {
            Value::Null => return None,
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => i.to_string(),
            Value::UInt(u) => u.to_string(),
            Value::Float(f) => f.to_string(),
            Value::Bytes(b) => format!("\\x{}", hex_string(b)),
            Value::Json(j) => j.to_string(),
            Value::Array(items) => {
                let parts: Vec<String> = items
                    .iter()
                    .map(|v| v.to_plain_text().unwrap_or_else(|| "NULL".into()))
                    .collect();
                format!("{{{}}}", parts.join(","))
            }
            Value::Decimal(s)
            | Value::Text(s)
            | Value::Date(s)
            | Value::Time(s)
            | Value::Timestamp(s)
            | Value::TimestampTz(s)
            | Value::Interval(s)
            | Value::Uuid(s)
            | Value::Other(s) => s.clone(),
        })
    }
}

fn hex_string(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

fn hex<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&hex_string(bytes))
}

fn unhex<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    let text = String::deserialize(deserializer)?;
    if text.len() % 2 != 0 {
        return Err(serde::de::Error::custom("hex string has an odd length"));
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).map_err(serde::de::Error::custom))
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn serializes_tagged() -> Result<(), serde_json::Error> {
        assert_eq!(serde_json::to_value(Value::Null)?, json!({ "t": "null" }));
        assert_eq!(
            serde_json::to_value(Value::Int(5))?,
            json!({ "t": "int", "v": 5 })
        );
        assert_eq!(
            serde_json::to_value(Value::Decimal("10.50".into()))?,
            json!({ "t": "decimal", "v": "10.50" })
        );
        assert_eq!(
            serde_json::to_value(Value::Bytes(vec![0, 171, 255]))?,
            json!({ "t": "bytes", "v": "00abff" })
        );
        assert_eq!(
            serde_json::to_value(Value::TimestampTz("2026-10-19T11:45:00Z".into()))?,
            json!({ "t": "timestampTz", "v": "2026-10-19T11:45:00Z" })
        );
        Ok(())
    }

    #[test]
    fn bytes_round_trip() -> Result<(), serde_json::Error> {
        let value = Value::Bytes(vec![1, 2, 254]);
        let back: Value = serde_json::from_str(&serde_json::to_string(&value)?)?;
        assert_eq!(back, value);
        assert!(serde_json::from_value::<Value>(json!({ "t": "bytes", "v": "abc" })).is_err());
        Ok(())
    }

    #[test]
    fn plain_text_for_exports() {
        assert_eq!(Value::Null.to_plain_text(), None);
        assert_eq!(Value::Bool(true).to_plain_text().as_deref(), Some("true"));
        assert_eq!(
            Value::Bytes(vec![0xde, 0xad]).to_plain_text().as_deref(),
            Some("\\xdead")
        );
        assert_eq!(
            Value::Array(vec![Value::Int(1), Value::Null, Value::Text("a".into())])
                .to_plain_text()
                .as_deref(),
            Some("{1,NULL,a}")
        );
        assert_eq!(
            Value::Json(json!({ "a": 1 })).to_plain_text().as_deref(),
            Some("{\"a\":1}")
        );
    }
}
