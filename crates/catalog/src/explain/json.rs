//! A JSON tree that keeps every object member in document order, duplicate
//! keys included.
//!
//! MariaDB writes the tables of a join as repeated `"table"` members of one
//! object (`{"table": {...}, "table": {...}}`), which `serde_json::Value`
//! would collapse to the last one. Plans are therefore read into this type.

use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

/// A parsed JSON value.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Json {
    /// `null`.
    Null,
    /// `true` / `false`.
    Bool(bool),
    /// A number, as parsed.
    Number(serde_json::Number),
    /// A string.
    String(String),
    /// An array.
    Array(Vec<Json>),
    /// An object, members in document order, duplicates kept.
    Object(Vec<(String, Json)>),
}

const EMPTY_ENTRIES: &[(String, Json)] = &[];
const EMPTY_ITEMS: &[Json] = &[];

impl Json {
    /// Parses a JSON document.
    pub(crate) fn parse(text: &str) -> Result<Json, String> {
        serde_json::from_str(text).map_err(|e| e.to_string())
    }

    /// The first member called `key` of an object.
    pub(crate) fn get(&self, key: &str) -> Option<&Json> {
        self.entries()
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    /// The members of an object; empty for anything else.
    pub(crate) fn entries(&self) -> &[(String, Json)] {
        match self {
            Json::Object(entries) => entries,
            _ => EMPTY_ENTRIES,
        }
    }

    /// The items of an array; empty for anything else.
    pub(crate) fn items(&self) -> &[Json] {
        match self {
            Json::Array(items) => items,
            _ => EMPTY_ITEMS,
        }
    }

    /// True for objects.
    pub(crate) fn is_object(&self) -> bool {
        matches!(self, Json::Object(_))
    }

    /// The value as a number. MySQL writes costs and percentages as strings
    /// (`"1155.44"`), so numeric strings count.
    pub(crate) fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Number(n) => n.as_f64(),
            Json::String(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    /// The value as a non-negative integer (rounded when it has a fraction,
    /// as PostgreSQL 18 writes `"Actual Rows": 12.00`).
    pub(crate) fn as_u64(&self) -> Option<u64> {
        if let Json::Number(n) = self
            && let Some(u) = n.as_u64()
        {
            return Some(u);
        }
        let f = self.as_f64()?;
        if f.is_finite() && f >= 0.0 && f <= u64::MAX as f64 {
            // In range and finite, so the conversion is exact up to rounding.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            Some(f.round() as u64)
        } else {
            None
        }
    }

    /// The value as a string slice, for strings only.
    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(s) => Some(s),
            _ => None,
        }
    }

    /// The value as a boolean. MariaDB writes some flags as `1` / `0`.
    pub(crate) fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            Json::Number(n) => n.as_f64().map(|f| f != 0.0),
            _ => None,
        }
    }

    /// Strings of an array of strings (or a single string as one item).
    pub(crate) fn strings(&self) -> Vec<String> {
        match self {
            Json::Array(items) => items.iter().map(Json::display).collect(),
            Json::Null => Vec::new(),
            other => vec![other.display()],
        }
    }

    /// Text for showing the value as a property: strings as they are,
    /// numbers and booleans in JSON notation, arrays of scalars joined with
    /// `, `, anything else as compact JSON.
    pub(crate) fn display(&self) -> String {
        match self {
            Json::Null => "null".into(),
            Json::Bool(b) => b.to_string(),
            Json::Number(n) => n.to_string(),
            Json::String(s) => s.clone(),
            Json::Array(items) if items.iter().all(Json::is_scalar) => items
                .iter()
                .map(Json::display)
                .collect::<Vec<_>>()
                .join(", "),
            other => other.to_string(),
        }
    }

    fn is_scalar(&self) -> bool {
        !matches!(self, Json::Array(_) | Json::Object(_))
    }
}

impl fmt::Display for Json {
    /// Compact JSON text.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Json::Null => f.write_str("null"),
            Json::Bool(b) => write!(f, "{b}"),
            Json::Number(n) => write!(f, "{n}"),
            Json::String(s) => write_string(f, s),
            Json::Array(items) => {
                f.write_str("[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    write!(f, "{item}")?;
                }
                f.write_str("]")
            }
            Json::Object(entries) => {
                f.write_str("{")?;
                for (i, (key, value)) in entries.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    write_string(f, key)?;
                    write!(f, ":{value}")?;
                }
                f.write_str("}")
            }
        }
    }
}

fn write_string(f: &mut fmt::Formatter<'_>, s: &str) -> fmt::Result {
    match serde_json::to_string(s) {
        Ok(quoted) => f.write_str(&quoted),
        Err(_) => Err(fmt::Error),
    }
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(JsonVisitor)
    }
}

struct JsonVisitor;

impl<'de> Visitor<'de> for JsonVisitor {
    type Value = Json;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Json, E> {
        Ok(Json::Bool(v))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Json, E> {
        Ok(Json::Number(v.into()))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Json, E> {
        Ok(Json::Number(v.into()))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Json, E> {
        serde_json::Number::from_f64(v)
            .map(Json::Number)
            .ok_or_else(|| E::custom("a number that is not finite"))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Json, E> {
        Ok(Json::String(v.to_owned()))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<Json, E> {
        Ok(Json::String(v))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }

    fn visit_none<E: de::Error>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Json, D::Error> {
        Json::deserialize(deserializer)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(Json::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
        let mut entries = Vec::new();
        while let Some((key, value)) = map.next_entry::<String, Json>()? {
            entries.push((key, value));
        }
        Ok(Json::Object(entries))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_duplicate_keys_in_order() -> Result<(), String> {
        let json = Json::parse(r#"{"table": {"n": 1}, "x": "1.5", "table": {"n": 2}}"#)?;
        let tables: Vec<_> = json
            .entries()
            .iter()
            .filter(|(k, _)| k == "table")
            .filter_map(|(_, v)| v.get("n").and_then(Json::as_u64))
            .collect();
        assert_eq!(tables, vec![1, 2]);
        assert_eq!(json.get("x").and_then(Json::as_f64), Some(1.5));
        Ok(())
    }

    #[test]
    fn displays_values() -> Result<(), String> {
        let json = Json::parse(r#"{"a": ["x", 1, true], "b": [{"k": "v\""}], "c": 12.00}"#)?;
        assert_eq!(
            json.get("a").map(Json::display).as_deref(),
            Some("x, 1, true")
        );
        assert_eq!(
            json.get("b").map(Json::display).as_deref(),
            Some(r#"[{"k":"v\""}]"#)
        );
        assert_eq!(json.get("c").and_then(Json::as_u64), Some(12));
        Ok(())
    }
}
