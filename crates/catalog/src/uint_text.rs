//! Unsigned 64-bit integers that travel as decimal text.
//!
//! The `u64` counterpart of `postgres::edit::int_text`, for values of
//! either engine: JavaScript numbers are exact only up to 2^53 - 1, so a
//! MySQL `AUTO_INCREMENT` counter of a `BIGINT UNSIGNED` column such as
//! 18446744073709551615 would arrive rounded in the UI and be sent back
//! rounded. Optional fields that hold such values use [`option`] with
//! `#[serde(default, with = ...)]`: they are written as decimal strings and
//! read from a decimal string or a JSON integer.

use serde::Deserialize;

/// A JSON integer or decimal text.
#[derive(Deserialize)]
#[serde(
    untagged,
    expecting = "expected a whole number of zero or more, as a number or as text"
)]
enum UIntOrText {
    Unsigned(u64),
    Signed(i64),
    Text(String),
}

fn parse<E: serde::de::Error>(value: UIntOrText) -> Result<u64, E> {
    match value {
        UIntOrText::Unsigned(v) => Ok(v),
        UIntOrText::Signed(v) => u64::try_from(v)
            .map_err(|_| E::custom(format!("{v} is outside the unsigned 64-bit integer range"))),
        UIntOrText::Text(t) => {
            let trimmed = t.trim();
            let digits = trimmed.strip_prefix('+').unwrap_or(trimmed);
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return Err(E::custom(format!(
                    "{t:?} is not a whole number of zero or more"
                )));
            }
            digits
                .parse()
                .map_err(|_| E::custom(format!("{t} is outside the unsigned 64-bit integer range")))
        }
    }
}

/// `Option<u64>` as decimal text or `null`. Fields using it also need
/// `#[serde(default)]` when they may be absent.
pub(crate) mod option {
    use serde::{Deserialize, Deserializer, Serializer};

    use super::{UIntOrText, parse};

    /// Writes `Some(u64)` as decimal text and `None` as `null`.
    #[allow(clippy::ref_option)]
    pub(crate) fn serialize<S: Serializer>(
        value: &Option<u64>,
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
    ) -> Result<Option<u64>, D::Error> {
        Option::<UIntOrText>::deserialize(deserializer)?
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
        #[serde(default, with = "super::option")]
        a: Option<u64>,
    }

    fn probe(a: Option<u64>) -> Probe {
        Probe { a }
    }

    #[test]
    fn unsigned_values_travel_as_exact_text() -> Result<(), serde_json::Error> {
        for (value, text) in [
            (u64::MAX, "18446744073709551615"),
            (9_007_199_254_740_993, "9007199254740993"),
            (0, "0"),
        ] {
            let json = serde_json::to_value(probe(Some(value)))?;
            assert_eq!(json, json!({ "a": text }));
            assert_eq!(serde_json::from_value::<Probe>(json)?, probe(Some(value)));
        }
        assert_eq!(serde_json::to_value(probe(None))?, json!({ "a": null }));
        for (input, expected) in [
            (json!({"a": 5}), Some(5)),
            (json!({"a": 18_446_744_073_709_551_615_u64}), Some(u64::MAX)),
            (json!({"a": "+7"}), Some(7)),
            (json!({"a": " 12 "}), Some(12)),
            (json!({"a": null}), None),
            (json!({}), None),
        ] {
            assert_eq!(serde_json::from_value::<Probe>(input)?, probe(expected));
        }
        for bad in [
            json!({"a": "18446744073709551616"}),
            json!({"a": "-1"}),
            json!({"a": -1}),
            json!({"a": "1e3"}),
            json!({"a": ""}),
            json!({"a": "+"}),
            json!({"a": 1.5}),
            json!({"a": "x"}),
        ] {
            assert!(serde_json::from_value::<Probe>(bad).is_err());
        }
        // A value of the wrong shape says what was expected.
        for bad in [json!({"a": 1.5}), json!({"a": true}), json!({"a": [1]})] {
            let error = serde_json::from_value::<Probe>(bad)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            assert_eq!(
                error,
                "expected a whole number of zero or more, as a number or as text"
            );
        }
        Ok(())
    }
}
