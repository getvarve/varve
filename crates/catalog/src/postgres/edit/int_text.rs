//! 64-bit integers that travel as decimal text.
//!
//! JavaScript numbers are exact only up to 2^53 - 1, so a `bigint` sequence
//! limit such as 9223372036854775806 would arrive rounded in the UI and be
//! sent back rounded. Fields that hold such values use these helpers with
//! `#[serde(with = ...)]`: they are written as decimal strings and read from
//! a decimal string or (for older callers and tests) a JSON integer.

use serde::{Deserialize, Deserializer, Serializer};

/// A JSON integer or decimal text.
#[derive(Deserialize)]
#[serde(untagged)]
enum IntOrText {
    Signed(i64),
    Unsigned(u64),
    Text(String),
}

fn parse<E: serde::de::Error>(value: IntOrText) -> Result<i64, E> {
    match value {
        IntOrText::Signed(v) => Ok(v),
        IntOrText::Unsigned(v) => i64::try_from(v)
            .map_err(|_| E::custom(format!("{v} is outside the 64-bit integer range"))),
        IntOrText::Text(t) => {
            let trimmed = t.trim();
            let digits = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return Err(E::custom(format!("{t:?} is not a whole number")));
            }
            trimmed
                .parse()
                .map_err(|_| E::custom(format!("{t} is outside the 64-bit integer range")))
        }
    }
}

/// Writes an `i64` as decimal text.
pub(crate) fn serialize<S: Serializer>(value: &i64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&value.to_string())
}

/// Reads an `i64` from decimal text or a JSON integer.
pub(crate) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
    parse(IntOrText::deserialize(deserializer)?)
}

/// The same for `Option<i64>` (`null` for `None`). Fields using it also need
/// `#[serde(default)]` when they may be absent.
pub(crate) mod option {
    use serde::{Deserialize, Deserializer, Serializer};

    use super::{IntOrText, parse};

    /// Writes `Some(i64)` as decimal text and `None` as `null`.
    #[allow(clippy::ref_option)]
    pub(crate) fn serialize<S: Serializer>(
        value: &Option<i64>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(v) => serializer.serialize_str(&v.to_string()),
            None => serializer.serialize_none(),
        }
    }

    /// Reads `null`, decimal text or a JSON integer.
    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<i64>, D::Error> {
        Option::<IntOrText>::deserialize(deserializer)?
            .map(parse)
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Probe {
        #[serde(with = "super")]
        a: i64,
        #[serde(default, with = "super::option")]
        b: Option<i64>,
    }

    #[test]
    fn bigint_values_travel_as_exact_text() -> Result<(), serde_json::Error> {
        let p = Probe {
            a: 9_223_372_036_854_775_806,
            b: Some(i64::MIN),
        };
        let json = serde_json::to_value(&p)?;
        assert_eq!(
            json,
            json!({"a": "9223372036854775806", "b": "-9223372036854775808"})
        );
        assert_eq!(serde_json::from_value::<Probe>(json)?, p);
        assert_eq!(
            serde_json::from_value::<Probe>(json!({"a": 5}))?,
            Probe { a: 5, b: None }
        );
        assert_eq!(
            serde_json::from_value::<Probe>(json!({"a": "+7", "b": null}))?,
            Probe { a: 7, b: None }
        );
        for bad in [
            json!({"a": "9223372036854775808"}),
            json!({"a": "1e3"}),
            json!({"a": ""}),
            json!({"a": 1.5}),
            json!({"a": 18_446_744_073_709_551_615_u64}),
        ] {
            assert!(serde_json::from_value::<Probe>(bad).is_err());
        }
        Ok(())
    }
}
