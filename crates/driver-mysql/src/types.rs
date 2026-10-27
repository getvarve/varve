//! Column type names and value mapping for text protocol results.
//!
//! The text protocol sends every value as its text form (or raw bytes for
//! binary types), so the mapping is driven by the column metadata: the wire
//! type, the flags and the character set.

use mysql_async::consts::{ColumnFlags, ColumnType};
use mysql_async::{Column as MyColumn, Value as MyValue};
use varve_driver::{Column, Value};

/// Character set id of the `binary` pseudo charset.
pub(crate) const BINARY_CHARSET: u16 = 63;

/// The metadata of a result column that the mapping needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ColumnMeta {
    /// Wire type.
    pub column_type: ColumnType,
    /// Column flags.
    pub flags: ColumnFlags,
    /// Character set id (63 is binary).
    pub charset: u16,
    /// Column length in bytes (bits for `BIT`).
    pub length: u32,
}

impl ColumnMeta {
    /// Reads the metadata of a result column.
    pub(crate) fn of(column: &MyColumn) -> Self {
        Self {
            column_type: column.column_type(),
            flags: column.flags(),
            charset: column.character_set(),
            length: column.column_length(),
        }
    }

    fn unsigned(&self) -> bool {
        self.flags.contains(ColumnFlags::UNSIGNED_FLAG)
    }

    fn binary(&self) -> bool {
        self.charset == BINARY_CHARSET
    }

    fn is_enum(&self) -> bool {
        self.column_type == ColumnType::MYSQL_TYPE_ENUM
            || self.flags.contains(ColumnFlags::ENUM_FLAG)
    }

    fn is_set(&self) -> bool {
        self.column_type == ColumnType::MYSQL_TYPE_SET || self.flags.contains(ColumnFlags::SET_FLAG)
    }
}

/// Builds the UI column for a result column.
pub(crate) fn column(column: &MyColumn) -> Column {
    Column {
        name: column.name_str().into_owned(),
        type_name: type_name(&ColumnMeta::of(column)),
    }
}

/// The SQL type name of a result column, for example `int unsigned`,
/// `varchar`, `longblob` or `json`.
///
/// MariaDB sends `JSON` columns as `LONGTEXT` without a marker that the
/// protocol library exposes, so they are reported as `longtext`.
pub(crate) fn type_name(meta: &ColumnMeta) -> String {
    use ColumnType::*;
    let base = match meta.column_type {
        MYSQL_TYPE_TINY => "tinyint",
        MYSQL_TYPE_SHORT => "smallint",
        MYSQL_TYPE_INT24 => "mediumint",
        MYSQL_TYPE_LONG => "int",
        MYSQL_TYPE_LONGLONG => "bigint",
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => "decimal",
        MYSQL_TYPE_FLOAT => "float",
        MYSQL_TYPE_DOUBLE => "double",
        MYSQL_TYPE_NULL => "null",
        MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => "timestamp",
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => "date",
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => "time",
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => "datetime",
        MYSQL_TYPE_YEAR => "year",
        MYSQL_TYPE_BIT => "bit",
        MYSQL_TYPE_JSON => "json",
        MYSQL_TYPE_GEOMETRY => "geometry",
        MYSQL_TYPE_VECTOR => "vector",
        MYSQL_TYPE_TYPED_ARRAY | MYSQL_TYPE_UNKNOWN => "unknown",
        MYSQL_TYPE_ENUM | MYSQL_TYPE_SET | MYSQL_TYPE_STRING if meta.is_enum() => "enum",
        MYSQL_TYPE_ENUM | MYSQL_TYPE_SET | MYSQL_TYPE_STRING if meta.is_set() => "set",
        MYSQL_TYPE_ENUM => "enum",
        MYSQL_TYPE_SET => "set",
        MYSQL_TYPE_STRING if meta.binary() => "binary",
        MYSQL_TYPE_STRING => "char",
        MYSQL_TYPE_VARCHAR | MYSQL_TYPE_VAR_STRING if meta.binary() => "varbinary",
        MYSQL_TYPE_VARCHAR | MYSQL_TYPE_VAR_STRING => "varchar",
        MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB => {
            blob_name(meta)
        }
    };
    let numeric = matches!(
        meta.column_type,
        MYSQL_TYPE_TINY
            | MYSQL_TYPE_SHORT
            | MYSQL_TYPE_INT24
            | MYSQL_TYPE_LONG
            | MYSQL_TYPE_LONGLONG
            | MYSQL_TYPE_DECIMAL
            | MYSQL_TYPE_NEWDECIMAL
            | MYSQL_TYPE_FLOAT
            | MYSQL_TYPE_DOUBLE
    );
    if numeric && meta.unsigned() {
        format!("{base} unsigned")
    } else {
        base.to_owned()
    }
}

/// Text and blob types all arrive as `BLOB` in a result set; the size class
/// follows from the length. Text lengths are in bytes, so they are the
/// character limit times up to four bytes per character; the ranges of the
/// four size classes do not overlap for one to four bytes per character.
fn blob_name(meta: &ColumnMeta) -> &'static str {
    let length = u64::from(meta.length);
    match meta.column_type {
        ColumnType::MYSQL_TYPE_TINY_BLOB if meta.binary() => return "tinyblob",
        ColumnType::MYSQL_TYPE_MEDIUM_BLOB if meta.binary() => return "mediumblob",
        ColumnType::MYSQL_TYPE_LONG_BLOB if meta.binary() => return "longblob",
        ColumnType::MYSQL_TYPE_TINY_BLOB => return "tinytext",
        ColumnType::MYSQL_TYPE_MEDIUM_BLOB => return "mediumtext",
        ColumnType::MYSQL_TYPE_LONG_BLOB => return "longtext",
        _ => {}
    }
    let class = if length <= 255 * 4 {
        0
    } else if length <= 65_535 * 4 {
        1
    } else if length <= 16_777_215 * 4 {
        2
    } else {
        3
    };
    match (meta.binary(), class) {
        (true, 0) => "tinyblob",
        (true, 1) => "blob",
        (true, 2) => "mediumblob",
        (true, _) => "longblob",
        (false, 0) => "tinytext",
        (false, 1) => "text",
        (false, 2) => "mediumtext",
        (false, _) => "longtext",
    }
}

/// How `TIMESTAMP` values are labelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimestampZone {
    /// The session time zone is UTC: `TIMESTAMP` values are
    /// [`Value::TimestampTz`] in UTC.
    Utc,
    /// The session time zone is something else, or not known for this
    /// result: `TIMESTAMP` values are [`Value::Timestamp`] without a zone.
    Unknown,
}

/// Maps one value of a text protocol row.
pub(crate) fn value(meta: &ColumnMeta, value: MyValue, zone: TimestampZone) -> Value {
    match value {
        MyValue::NULL => Value::Null,
        MyValue::Bytes(bytes) => from_bytes(meta, bytes, zone),
        // The text protocol only sends bytes; the other variants are mapped
        // through their text form so the result is the same either way.
        MyValue::Int(i) => from_bytes(meta, i.to_string().into_bytes(), zone),
        MyValue::UInt(u) => from_bytes(meta, u.to_string().into_bytes(), zone),
        MyValue::Float(f) => from_bytes(meta, f.to_string().into_bytes(), zone),
        MyValue::Double(f) => from_bytes(meta, f.to_string().into_bytes(), zone),
        MyValue::Date(year, month, day, hour, minute, second, micros) => {
            let mut text = format!("{year:04}-{month:02}-{day:02}");
            if !matches!(
                meta.column_type,
                ColumnType::MYSQL_TYPE_DATE | ColumnType::MYSQL_TYPE_NEWDATE
            ) {
                text.push_str(&format!(" {hour:02}:{minute:02}:{second:02}"));
                if micros > 0 {
                    text.push_str(&format!(".{micros:06}"));
                }
            }
            from_bytes(meta, text.into_bytes(), zone)
        }
        MyValue::Time(negative, days, hours, minutes, seconds, micros) => {
            let total_hours = u64::from(days) * 24 + u64::from(hours);
            let sign = if negative { "-" } else { "" };
            let mut text = format!("{sign}{total_hours:02}:{minutes:02}:{seconds:02}");
            if micros > 0 {
                text.push_str(&format!(".{micros:06}"));
            }
            from_bytes(meta, text.into_bytes(), zone)
        }
    }
}

fn from_bytes(meta: &ColumnMeta, bytes: Vec<u8>, zone: TimestampZone) -> Value {
    use ColumnType::*;
    match meta.column_type {
        MYSQL_TYPE_NULL => Value::Null,
        MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_INT24 | MYSQL_TYPE_LONG
        | MYSQL_TYPE_LONGLONG | MYSQL_TYPE_YEAR => integer(bytes),
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => Value::Decimal(text_lossless(bytes)),
        MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => {
            let text = text_lossless(bytes);
            match text.parse::<f64>() {
                Ok(f) => Value::Float(f),
                Err(_) => Value::Other(text),
            }
        }
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => temporal(bytes, Value::Date),
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => {
            temporal(bytes, |text| Value::Timestamp(text.replacen(' ', "T", 1)))
        }
        // The server renders TIMESTAMP values in the session time zone.
        // Only a UTC session gives them a zone; otherwise the local time is
        // passed on without one rather than mislabelled.
        MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => temporal(bytes, |text| {
            let local = text.replacen(' ', "T", 1);
            match zone {
                TimestampZone::Utc => Value::TimestampTz(format!("{local}Z")),
                TimestampZone::Unknown => Value::Timestamp(local),
            }
        }),
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => Value::Time(text_lossless(bytes)),
        MYSQL_TYPE_BIT => bit(meta, bytes),
        // Kept as the server's text, so key order and number precision
        // survive; `Value::json` falls back to text for invalid JSON.
        MYSQL_TYPE_JSON => match String::from_utf8(bytes) {
            Ok(text) => Value::json(text),
            Err(error) => Value::Bytes(error.into_bytes()),
        },
        MYSQL_TYPE_GEOMETRY | MYSQL_TYPE_VECTOR => Value::Bytes(bytes),
        MYSQL_TYPE_ENUM | MYSQL_TYPE_SET => text_or_bytes(bytes),
        MYSQL_TYPE_STRING if meta.is_enum() || meta.is_set() => text_or_bytes(bytes),
        MYSQL_TYPE_STRING
        | MYSQL_TYPE_VARCHAR
        | MYSQL_TYPE_VAR_STRING
        | MYSQL_TYPE_TINY_BLOB
        | MYSQL_TYPE_MEDIUM_BLOB
        | MYSQL_TYPE_LONG_BLOB
        | MYSQL_TYPE_BLOB => {
            if meta.binary() {
                Value::Bytes(bytes)
            } else {
                text_or_bytes(bytes)
            }
        }
        MYSQL_TYPE_TYPED_ARRAY | MYSQL_TYPE_UNKNOWN => text_or_bytes(bytes),
    }
}

/// Integers: `Int` when the value fits an `i64`, `UInt` for unsigned values
/// above `i64::MAX`.
fn integer(bytes: Vec<u8>) -> Value {
    let text = text_lossless(bytes);
    if let Ok(i) = text.parse::<i64>() {
        Value::Int(i)
    } else if let Ok(u) = text.parse::<u64>() {
        Value::UInt(u)
    } else {
        Value::Other(text)
    }
}

/// `BIT(1)` to `BIT(63)` become integers; `BIT(64)` stays bytes because it
/// does not fit an `i64` in every case.
fn bit(meta: &ColumnMeta, bytes: Vec<u8>) -> Value {
    if (1..=63).contains(&meta.length) && bytes.len() <= 8 {
        let number = bytes
            .iter()
            .fold(0_u64, |acc, byte| (acc << 8) | u64::from(*byte));
        match i64::try_from(number) {
            Ok(i) => Value::Int(i),
            Err(_) => Value::Bytes(bytes),
        }
    } else {
        Value::Bytes(bytes)
    }
}

/// Dates with a zero part (`0000-00-00`, `2026-00-01`) are valid in MySQL
/// but not in ISO 8601, so they are passed on as plain text.
fn temporal(bytes: Vec<u8>, build: impl FnOnce(String) -> Value) -> Value {
    let text = text_lossless(bytes);
    let date = text.get(..10).unwrap_or(&text);
    let zero_part = date
        .split('-')
        .any(|part| part.trim_start_matches('0').is_empty());
    if zero_part || date.len() < 10 {
        Value::Other(text)
    } else {
        build(text)
    }
}

/// Text that the server sends for numbers and temporal values is ASCII.
fn text_lossless(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
    }
}

/// Character data arrives as utf8mb4 (the connection character set); data
/// that is not valid UTF-8 is kept as bytes so nothing is lost.
fn text_or_bytes(bytes: Vec<u8>) -> Value {
    match String::from_utf8(bytes) {
        Ok(text) => Value::Text(text),
        Err(error) => Value::Bytes(error.into_bytes()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(column_type: ColumnType) -> ColumnMeta {
        ColumnMeta {
            column_type,
            flags: ColumnFlags::empty(),
            charset: 255,
            length: 0,
        }
    }

    fn with(mut meta: ColumnMeta, flags: ColumnFlags, charset: u16, length: u32) -> ColumnMeta {
        meta.flags = flags;
        meta.charset = charset;
        meta.length = length;
        meta
    }

    /// Maps a value in a UTC session.
    fn map(meta: &ColumnMeta, value: MyValue) -> Value {
        super::value(meta, value, TimestampZone::Utc)
    }

    fn bytes(text: &str) -> MyValue {
        MyValue::Bytes(text.as_bytes().to_vec())
    }

    #[test]
    fn names_numeric_types() {
        use ColumnType::*;
        assert_eq!(type_name(&meta(MYSQL_TYPE_LONG)), "int");
        let unsigned = with(meta(MYSQL_TYPE_LONG), ColumnFlags::UNSIGNED_FLAG, 63, 10);
        assert_eq!(type_name(&unsigned), "int unsigned");
        let big = with(
            meta(MYSQL_TYPE_LONGLONG),
            ColumnFlags::UNSIGNED_FLAG,
            63,
            20,
        );
        assert_eq!(type_name(&big), "bigint unsigned");
        assert_eq!(type_name(&meta(MYSQL_TYPE_TINY)), "tinyint");
        assert_eq!(type_name(&meta(MYSQL_TYPE_SHORT)), "smallint");
        assert_eq!(type_name(&meta(MYSQL_TYPE_INT24)), "mediumint");
        assert_eq!(type_name(&meta(MYSQL_TYPE_NEWDECIMAL)), "decimal");
        assert_eq!(type_name(&meta(MYSQL_TYPE_DOUBLE)), "double");
        assert_eq!(type_name(&meta(MYSQL_TYPE_FLOAT)), "float");
        assert_eq!(type_name(&meta(MYSQL_TYPE_YEAR)), "year");
        assert_eq!(type_name(&meta(MYSQL_TYPE_BIT)), "bit");
    }

    #[test]
    fn names_string_types() {
        use ColumnType::*;
        assert_eq!(type_name(&meta(MYSQL_TYPE_VAR_STRING)), "varchar");
        let varbinary = with(
            meta(MYSQL_TYPE_VAR_STRING),
            ColumnFlags::BINARY_FLAG,
            63,
            16,
        );
        assert_eq!(type_name(&varbinary), "varbinary");
        let bin_collation = with(
            meta(MYSQL_TYPE_VAR_STRING),
            ColumnFlags::BINARY_FLAG,
            46,
            16,
        );
        assert_eq!(type_name(&bin_collation), "varchar");
        assert_eq!(type_name(&meta(MYSQL_TYPE_STRING)), "char");
        let binary = with(meta(MYSQL_TYPE_STRING), ColumnFlags::BINARY_FLAG, 63, 4);
        assert_eq!(type_name(&binary), "binary");
        let enumeration = with(meta(MYSQL_TYPE_STRING), ColumnFlags::ENUM_FLAG, 255, 20);
        assert_eq!(type_name(&enumeration), "enum");
        let set = with(meta(MYSQL_TYPE_STRING), ColumnFlags::SET_FLAG, 255, 20);
        assert_eq!(type_name(&set), "set");
        assert_eq!(type_name(&meta(MYSQL_TYPE_JSON)), "json");
        assert_eq!(type_name(&meta(MYSQL_TYPE_GEOMETRY)), "geometry");
    }

    #[test]
    fn names_blob_and_text_by_length() {
        use ColumnType::*;
        let blob = |charset, length| {
            type_name(&with(
                meta(MYSQL_TYPE_BLOB),
                ColumnFlags::BLOB_FLAG,
                charset,
                length,
            ))
        };
        assert_eq!(blob(63, 255), "tinyblob");
        assert_eq!(blob(63, 65_535), "blob");
        assert_eq!(blob(63, 16_777_215), "mediumblob");
        assert_eq!(blob(63, u32::MAX), "longblob");
        assert_eq!(blob(255, 1_020), "tinytext");
        assert_eq!(blob(255, 262_140), "text");
        assert_eq!(blob(8, 65_535), "text");
        assert_eq!(blob(255, 67_108_860), "mediumtext");
        assert_eq!(blob(255, u32::MAX), "longtext");
    }

    #[test]
    fn maps_integers() {
        use ColumnType::*;
        assert_eq!(map(&meta(MYSQL_TYPE_LONG), bytes("-42")), Value::Int(-42));
        let unsigned = with(
            meta(MYSQL_TYPE_LONGLONG),
            ColumnFlags::UNSIGNED_FLAG,
            63,
            20,
        );
        assert_eq!(
            map(&unsigned, bytes("18446744073709551615")),
            Value::UInt(u64::MAX)
        );
        assert_eq!(map(&unsigned, bytes("7")), Value::Int(7));
        assert_eq!(map(&meta(MYSQL_TYPE_YEAR), bytes("2026")), Value::Int(2026));
        assert_eq!(map(&meta(MYSQL_TYPE_LONG), MyValue::NULL), Value::Null);
    }

    #[test]
    fn maps_numbers() {
        use ColumnType::*;
        assert_eq!(
            map(
                &meta(MYSQL_TYPE_NEWDECIMAL),
                bytes("123456789012345678901234567890.0123456789")
            ),
            Value::Decimal("123456789012345678901234567890.0123456789".into())
        );
        assert_eq!(
            map(&meta(MYSQL_TYPE_DOUBLE), bytes("1.5")),
            Value::Float(1.5)
        );
        assert_eq!(
            map(&meta(MYSQL_TYPE_FLOAT), bytes("-2.25")),
            Value::Float(-2.25)
        );
    }

    #[test]
    fn maps_temporal_types() {
        use ColumnType::*;
        assert_eq!(
            map(&meta(MYSQL_TYPE_DATE), bytes("2026-10-19")),
            Value::Date("2026-10-19".into())
        );
        assert_eq!(
            map(
                &meta(MYSQL_TYPE_DATETIME),
                bytes("2026-10-19 13:45:00.123456")
            ),
            Value::Timestamp("2026-10-19T13:45:00.123456".into())
        );
        assert_eq!(
            map(&meta(MYSQL_TYPE_TIMESTAMP), bytes("2026-10-19 11:45:00")),
            Value::TimestampTz("2026-10-19T11:45:00Z".into())
        );
        assert_eq!(
            map(&meta(MYSQL_TYPE_TIME), bytes("-838:59:59.000000")),
            Value::Time("-838:59:59.000000".into())
        );
        assert_eq!(
            map(&meta(MYSQL_TYPE_DATETIME), bytes("0000-00-00 00:00:00")),
            Value::Other("0000-00-00 00:00:00".into())
        );
        assert_eq!(
            map(&meta(MYSQL_TYPE_DATE), bytes("2026-00-10")),
            Value::Other("2026-00-10".into())
        );
        assert_eq!(
            map(
                &meta(MYSQL_TYPE_DATETIME),
                MyValue::Date(2026, 10, 19, 1, 2, 3, 4)
            ),
            Value::Timestamp("2026-10-19T01:02:03.000004".into())
        );
        assert_eq!(
            map(&meta(MYSQL_TYPE_TIME), MyValue::Time(true, 1, 2, 3, 4, 0)),
            Value::Time("-26:03:04".into())
        );
    }

    #[test]
    fn timestamps_get_a_zone_only_in_utc_sessions() {
        use ColumnType::*;
        let ts = meta(MYSQL_TYPE_TIMESTAMP);
        assert_eq!(
            super::value(&ts, bytes("2026-10-19 13:45:00"), TimestampZone::Utc),
            Value::TimestampTz("2026-10-19T13:45:00Z".into())
        );
        assert_eq!(
            super::value(&ts, bytes("2026-10-19 13:45:00"), TimestampZone::Unknown),
            Value::Timestamp("2026-10-19T13:45:00".into())
        );
        // DATETIME never has a zone, whatever the session zone.
        assert_eq!(
            super::value(
                &meta(MYSQL_TYPE_DATETIME),
                bytes("2026-10-19 13:45:00"),
                TimestampZone::Utc
            ),
            Value::Timestamp("2026-10-19T13:45:00".into())
        );
    }

    #[test]
    fn json_keeps_the_server_text() {
        use ColumnType::*;
        let text = r#"{"b": 1, "a": 12345678901234567890123, "c": 1.10}"#;
        match map(&meta(MYSQL_TYPE_JSON), bytes(text)) {
            Value::Json(json) => assert_eq!(json.as_str(), text),
            other => panic!("expected JSON, got {other:?}"),
        }
        // Invalid JSON (which a server should never send) stays text.
        assert_eq!(
            map(&meta(MYSQL_TYPE_JSON), bytes("{")),
            Value::Text("{".into())
        );
    }

    #[test]
    fn maps_bits() {
        use ColumnType::*;
        let bit8 = with(meta(MYSQL_TYPE_BIT), ColumnFlags::UNSIGNED_FLAG, 63, 8);
        assert_eq!(
            map(&bit8, MyValue::Bytes(vec![0b1010_0101])),
            Value::Int(165)
        );
        let bit12 = with(meta(MYSQL_TYPE_BIT), ColumnFlags::UNSIGNED_FLAG, 63, 12);
        assert_eq!(
            map(&bit12, MyValue::Bytes(vec![0x0f, 0xff])),
            Value::Int(4095)
        );
        let bit64 = with(meta(MYSQL_TYPE_BIT), ColumnFlags::UNSIGNED_FLAG, 63, 64);
        assert_eq!(
            map(&bit64, MyValue::Bytes(vec![0xff; 8])),
            Value::Bytes(vec![0xff; 8])
        );
    }

    #[test]
    fn maps_strings_json_and_binary() {
        use ColumnType::*;
        assert_eq!(
            map(&meta(MYSQL_TYPE_VAR_STRING), bytes("héllo")),
            Value::Text("héllo".into())
        );
        assert_eq!(
            map(&meta(MYSQL_TYPE_JSON), bytes(r#"{"a": [1, 2]}"#)),
            Value::json(r#"{"a": [1, 2]}"#)
        );
        let blob = with(meta(MYSQL_TYPE_BLOB), ColumnFlags::BLOB_FLAG, 63, 65_535);
        assert_eq!(
            map(&blob, MyValue::Bytes(vec![0, 1, 255])),
            Value::Bytes(vec![0, 1, 255])
        );
        let binary = with(meta(MYSQL_TYPE_STRING), ColumnFlags::BINARY_FLAG, 63, 4);
        assert_eq!(map(&binary, bytes("ab")), Value::Bytes(b"ab".to_vec()));
        let enumeration = with(meta(MYSQL_TYPE_STRING), ColumnFlags::ENUM_FLAG, 255, 20);
        assert_eq!(map(&enumeration, bytes("red")), Value::Text("red".into()));
        assert_eq!(
            map(&meta(MYSQL_TYPE_GEOMETRY), MyValue::Bytes(vec![1, 2])),
            Value::Bytes(vec![1, 2])
        );
        assert_eq!(
            map(
                &meta(MYSQL_TYPE_VAR_STRING),
                MyValue::Bytes(vec![0xff, 0xfe])
            ),
            Value::Bytes(vec![0xff, 0xfe])
        );
        assert_eq!(map(&meta(MYSQL_TYPE_NULL), MyValue::NULL), Value::Null);
    }
}
