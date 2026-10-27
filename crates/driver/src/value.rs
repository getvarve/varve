//! Cell values, engine-independent and shaped for the UI and exports.

use serde::{Deserialize, Serialize, Serializer};

/// A cell value.
///
/// Numbers that do not fit an `i64`/`u64`/`f64` exactly (PostgreSQL
/// `numeric`, MySQL `DECIMAL`) and temporal values are carried as their
/// canonical text, so nothing is rounded or shifted between the server and
/// the grid. Serialized as `{"t": "<type>", "v": <value>}`.
///
/// The UI is JavaScript, whose numbers are doubles, so the wire format never
/// relies on JSON numbers that a double cannot hold: integers outside
/// `-(2^53 - 1) ..= 2^53 - 1` are sent as decimal strings, non-finite floats
/// as the strings `NaN`, `Infinity` and `-Infinity`, and JSON documents as
/// their text in a string.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "t", content = "v", rename_all = "camelCase")]
pub enum Value {
    /// SQL `NULL` or Redis nil.
    Null,
    /// Boolean.
    Bool(bool),
    /// Signed integer.
    #[serde(serialize_with = "int_out", deserialize_with = "int_in")]
    Int(i64),
    /// Unsigned integer that does not fit an `i64`.
    #[serde(serialize_with = "uint_out", deserialize_with = "uint_in")]
    UInt(u64),
    /// Floating point number.
    #[serde(serialize_with = "float_out", deserialize_with = "float_in")]
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
    /// JSON document as the server sent it: key order and number precision
    /// are kept exactly (serialized as its text in a string).
    Json(JsonText),
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
            Value::Json(j) => j.as_str().to_string(),
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

/// A validated JSON document kept as its original text.
///
/// Parsing into `serde_json::Value` would reorder object keys and round
/// large or precise numbers; this type keeps the server's text and writes it
/// out verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonText(String);

impl JsonText {
    /// Wraps `text` when it is valid JSON.
    #[must_use]
    pub fn parse(text: impl Into<String>) -> Option<Self> {
        let text = text.into();
        serde_json::from_str::<serde::de::IgnoredAny>(&text)
            .ok()
            .map(|_| Self(text))
    }

    /// The JSON text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for JsonText {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for JsonText {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(text).ok_or_else(|| serde::de::Error::custom("not a valid JSON document"))
    }
}

/// Largest integer a JavaScript number holds exactly (`2^53 - 1`).
pub const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

fn int_out<S: Serializer>(value: &i64, serializer: S) -> Result<S::Ok, S::Error> {
    if (-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(value) {
        serializer.serialize_i64(*value)
    } else {
        serializer.serialize_str(&value.to_string())
    }
}

fn uint_out<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
    if i64::try_from(*value).is_ok_and(|v| v <= MAX_SAFE_INTEGER) {
        serializer.serialize_u64(*value)
    } else {
        serializer.serialize_str(&value.to_string())
    }
}

fn float_out<S: Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    if value.is_finite() {
        serializer.serialize_f64(*value)
    } else if value.is_nan() {
        serializer.serialize_str("NaN")
    } else if *value > 0.0 {
        serializer.serialize_str("Infinity")
    } else {
        serializer.serialize_str("-Infinity")
    }
}

/// A number as JSON number or as decimal text.
#[derive(Deserialize)]
#[serde(untagged)]
enum NumberOrText {
    Signed(i64),
    Unsigned(u64),
    Float(f64),
    Text(String),
}

fn int_in<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
    match NumberOrText::deserialize(deserializer)? {
        NumberOrText::Signed(v) => Ok(v),
        NumberOrText::Unsigned(v) => i64::try_from(v).map_err(serde::de::Error::custom),
        NumberOrText::Text(t) => t.parse().map_err(serde::de::Error::custom),
        NumberOrText::Float(_) => Err(serde::de::Error::custom("expected an integer")),
    }
}

fn uint_in<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    match NumberOrText::deserialize(deserializer)? {
        NumberOrText::Unsigned(v) => Ok(v),
        NumberOrText::Signed(v) => u64::try_from(v).map_err(serde::de::Error::custom),
        NumberOrText::Text(t) => t.parse().map_err(serde::de::Error::custom),
        NumberOrText::Float(_) => Err(serde::de::Error::custom("expected an integer")),
    }
}

#[allow(clippy::cast_precision_loss)]
fn float_in<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
    match NumberOrText::deserialize(deserializer)? {
        NumberOrText::Float(v) => Ok(v),
        NumberOrText::Signed(v) => Ok(v as f64),
        NumberOrText::Unsigned(v) => Ok(v as f64),
        NumberOrText::Text(t) => match t.as_str() {
            "NaN" => Ok(f64::NAN),
            "Infinity" => Ok(f64::INFINITY),
            "-Infinity" => Ok(f64::NEG_INFINITY),
            _ => Err(serde::de::Error::custom(
                "expected a number, NaN or Infinity",
            )),
        },
    }
}

impl Value {
    /// A JSON value from server text; `Text` when the text is not valid JSON.
    #[must_use]
    pub fn json(text: impl Into<String>) -> Value {
        let text = text.into();
        match JsonText::parse(text.clone()) {
            Some(json) => Value::Json(json),
            None => Value::Text(text),
        }
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
    // Work on bytes, never on string offsets: a non-ASCII character would
    // put a char boundary inside a pair and panic when sliced.
    let bytes = text.as_bytes();
    if bytes.len() % 2 != 0 {
        return Err(serde::de::Error::custom("hex string has an odd length"));
    }
    let digit = |b: u8| -> Result<u8, D::Error> {
        char::from(b)
            .to_digit(16)
            .and_then(|d| u8::try_from(d).ok())
            .ok_or_else(|| {
                serde::de::Error::custom("hex string has a character that is not a hex digit")
            })
    };
    // The odd-length check above leaves no remainder.
    let (pairs, _) = bytes.as_chunks::<2>();
    pairs
        .iter()
        .map(|&[high, low]| Ok(digit(high)? << 4 | digit(low)?))
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
        // Non-ASCII text is refused, not sliced inside a character.
        for bad in ["a\u{e9}b", "\u{e9}", "zz", "0g", "\u{1f600}"] {
            assert!(
                serde_json::from_value::<Value>(json!({ "t": "bytes", "v": bad })).is_err(),
                "{bad}"
            );
        }
        assert_eq!(
            serde_json::from_value::<Value>(json!({ "t": "bytes", "v": "00AbFf" }))?,
            Value::Bytes(vec![0, 171, 255])
        );
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
            Value::json(r#"{"a":1}"#).to_plain_text().as_deref(),
            Some("{\"a\":1}")
        );
    }

    #[test]
    fn json_keeps_key_order_and_precision() -> Result<(), serde_json::Error> {
        let text = r#"{"b":1,"a":2,"id":12345678901234567890123,"price":1.10}"#;
        let value = Value::json(text);
        assert_eq!(
            serde_json::to_value(&value)?,
            json!({ "t": "json", "v": text })
        );
        let back: Value = serde_json::from_str(&serde_json::to_string(&value)?)?;
        assert_eq!(back, value);
        assert!(serde_json::from_value::<Value>(json!({ "t": "json", "v": "{" })).is_err());
        assert_eq!(Value::json("not json"), Value::Text("not json".into()));
        assert!(JsonText::parse("{").is_none());
        Ok(())
    }

    #[test]
    fn numbers_survive_javascript() -> Result<(), serde_json::Error> {
        assert_eq!(
            serde_json::to_value(Value::Int(MAX_SAFE_INTEGER))?,
            json!({ "t": "int", "v": 9_007_199_254_740_991_i64 })
        );
        assert_eq!(
            serde_json::to_value(Value::Int(MAX_SAFE_INTEGER + 1))?,
            json!({ "t": "int", "v": "9007199254740992" })
        );
        assert_eq!(
            serde_json::to_value(Value::Int(i64::MIN))?,
            json!({ "t": "int", "v": "-9223372036854775808" })
        );
        assert_eq!(
            serde_json::to_value(Value::UInt(u64::MAX))?,
            json!({ "t": "uInt", "v": "18446744073709551615" })
        );
        assert_eq!(
            serde_json::to_value(Value::Float(f64::NAN))?,
            json!({ "t": "float", "v": "NaN" })
        );
        assert_eq!(
            serde_json::to_value(Value::Float(f64::NEG_INFINITY))?,
            json!({ "t": "float", "v": "-Infinity" })
        );
        for value in [
            Value::Int(i64::MIN),
            Value::Int(-3),
            Value::UInt(u64::MAX),
            Value::Float(1.5),
            Value::Float(f64::INFINITY),
        ] {
            let back: Value = serde_json::from_str(&serde_json::to_string(&value)?)?;
            assert_eq!(back, value);
        }
        let nan: Value = serde_json::from_value(json!({ "t": "float", "v": "NaN" }))?;
        assert!(matches!(nan, Value::Float(f) if f.is_nan()));
        Ok(())
    }
}
