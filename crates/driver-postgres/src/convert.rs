//! Turns PostgreSQL text-format values into [`Value`]s.
//!
//! The session sets `DateStyle = ISO, YMD`, `IntervalStyle = iso_8601`,
//! `TimeZone = 'UTC'` and `extra_float_digits = 3`, so the text format is
//! lossless and predictable. A user can change those settings in a script;
//! every parser here therefore validates its input and returns
//! [`Value::Other`] with the server's text when the text is not in the
//! expected shape, instead of producing a wrong typed value.

use std::collections::HashMap;

use tokio_postgres::types::{Kind, Type};
use varve_driver::Value;

/// How the text of one column is turned into a [`Value`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ValueKind {
    Bool,
    Int,
    Float,
    Decimal,
    Text,
    Bytes,
    Date,
    Time,
    Timestamp,
    TimestampTz,
    Interval,
    Uuid,
    Json,
    Array {
        element: Box<ValueKind>,
        delimiter: char,
    },
    Other,
}

/// Whether a type's text depends on a setting the server does not report
/// (`extra_float_digits`, `bytea_output`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Dependence {
    Yes,
    No,
    /// It depends on these types, which may be multiranges (tokio-postgres
    /// reports a multirange as a simple type, without its element type);
    /// look them up in `pg_range`.
    Unknown(Vec<u32>),
}

impl Dependence {
    fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::Yes, _) | (_, Self::Yes) => Self::Yes,
            (Self::Unknown(mut a), Self::Unknown(b)) => {
                a.extend(b);
                Self::Unknown(a)
            }
            (Self::Unknown(a), Self::No) | (Self::No, Self::Unknown(a)) => Self::Unknown(a),
            (Self::No, Self::No) => Self::No,
        }
    }

    /// Unknown counts as yes: better one check too many.
    pub(crate) fn likely(&self) -> bool {
        !matches!(self, Self::No)
    }
}

/// Whether the server's text for values of `ty` depends on an unreported
/// setting: float and bytea, and arrays, domains, ranges, multiranges and
/// composites built from them. An anonymous `record` may hold anything, so
/// it counts too. `multiranges` maps the multirange types looked up so far
/// to their answer; `major` is the server's major version (multiranges
/// exist from PostgreSQL 14).
pub(crate) fn dependence(ty: &Type, multiranges: &HashMap<u32, bool>, major: u32) -> Dependence {
    match ty.kind() {
        Kind::Array(inner) | Kind::Domain(inner) | Kind::Range(inner) => {
            dependence(inner, multiranges, major)
        }
        Kind::Composite(fields) => fields
            .iter()
            .map(|f| dependence(f.type_(), multiranges, major))
            .fold(Dependence::No, Dependence::or),
        // The geometric types print their coordinates as float8.
        _ if matches!(
            ty.oid(),
            oid::FLOAT4
                | oid::FLOAT8
                | oid::BYTEA
                | oid::RECORD
                | oid::RECORD_ARRAY
                | oid::POINT
                | oid::LSEG
                | oid::PATH
                | oid::BOX
                | oid::POLYGON
                | oid::LINE
                | oid::CIRCLE
        ) =>
        {
            Dependence::Yes
        }
        // contrib's cube prints float8 coordinates too.
        Kind::Simple if ty.name() == "cube" && Type::from_oid(ty.oid()).is_none() => {
            Dependence::Yes
        }
        // A user type tokio-postgres knows only as "simple".
        Kind::Simple if major >= 14 && Type::from_oid(ty.oid()).is_none() => {
            match multiranges.get(&ty.oid()) {
                Some(true) => Dependence::Yes,
                Some(false) => Dependence::No,
                None => Dependence::Unknown(vec![ty.oid()]),
            }
        }
        _ => Dependence::No,
    }
}

/// [`dependence`] for a built-in type (a multirange's element type).
pub(crate) fn builtin_dependence(oid: u32) -> Option<bool> {
    Type::from_oid(oid).map(|ty| dependence(&ty, &HashMap::new(), 0) == Dependence::Yes)
}

/// A resolved column type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColumnType {
    /// Name shown to the user: the `pg_type` name, arrays as `int4[]`.
    pub(crate) name: String,
    /// How values are converted.
    pub(crate) kind: ValueKind,
    /// The PostgreSQL type, for [`dependence`].
    pub(crate) ty: Type,
}

/// Per-session cache of resolved column types, keyed by type OID.
#[derive(Debug, Default)]
pub(crate) struct TypeCache {
    types: HashMap<u32, ColumnType>,
    /// Multirange (or other simple user) types looked up in `pg_range`:
    /// whether their text depends on an unreported setting.
    pub(crate) multiranges: HashMap<u32, bool>,
}

impl TypeCache {
    /// Resolves `ty`, using the cache when it was seen before.
    pub(crate) fn resolve(&mut self, ty: &Type) -> ColumnType {
        self.types
            .entry(ty.oid())
            .or_insert_with(|| ColumnType {
                name: display_name(ty),
                kind: value_kind(ty),
                ty: ty.clone(),
            })
            .clone()
    }
}

mod oid {
    pub(super) const BOOL: u32 = 16;
    pub(super) const BYTEA: u32 = 17;
    pub(super) const RECORD: u32 = 2249;
    pub(super) const RECORD_ARRAY: u32 = 2287;
    pub(super) const CHAR: u32 = 18;
    pub(super) const NAME: u32 = 19;
    pub(super) const INT8: u32 = 20;
    pub(super) const INT2: u32 = 21;
    pub(super) const INT4: u32 = 23;
    pub(super) const TEXT: u32 = 25;
    pub(super) const OID: u32 = 26;
    pub(super) const JSON: u32 = 114;
    pub(super) const XML: u32 = 142;
    pub(super) const POINT: u32 = 600;
    pub(super) const LSEG: u32 = 601;
    pub(super) const PATH: u32 = 602;
    pub(super) const BOX: u32 = 603;
    pub(super) const POLYGON: u32 = 604;
    pub(super) const LINE: u32 = 628;
    pub(super) const CIRCLE: u32 = 718;
    pub(super) const FLOAT4: u32 = 700;
    pub(super) const FLOAT8: u32 = 701;
    pub(super) const UNKNOWN: u32 = 705;
    pub(super) const MONEY: u32 = 790;
    pub(super) const BPCHAR: u32 = 1042;
    pub(super) const VARCHAR: u32 = 1043;
    pub(super) const DATE: u32 = 1082;
    pub(super) const TIME: u32 = 1083;
    pub(super) const TIMESTAMP: u32 = 1114;
    pub(super) const TIMESTAMPTZ: u32 = 1184;
    pub(super) const INTERVAL: u32 = 1186;
    pub(super) const TIMETZ: u32 = 1266;
    pub(super) const NUMERIC: u32 = 1700;
    pub(super) const UUID: u32 = 2950;
    pub(super) const JSONB: u32 = 3802;
}

fn display_name(ty: &Type) -> String {
    match ty.kind() {
        Kind::Array(element) => format!("{}[]", display_name(element)),
        _ => ty.name().to_owned(),
    }
}

/// Follows domains to their base type; arrays to their element type.
fn value_kind(ty: &Type) -> ValueKind {
    match ty.kind() {
        Kind::Domain(base) => value_kind(base),
        Kind::Enum(_) => ValueKind::Text,
        Kind::Array(element) => {
            let base = base_type(element);
            ValueKind::Array {
                element: Box::new(value_kind(element)),
                // `box` is the only built-in type whose array delimiter is not
                // a comma.
                delimiter: if base.oid() == oid::BOX { ';' } else { ',' },
            }
        }
        Kind::Simple | Kind::Pseudo => simple_kind(ty),
        _ => ValueKind::Other,
    }
}

fn base_type(ty: &Type) -> &Type {
    match ty.kind() {
        Kind::Domain(base) => base_type(base),
        _ => ty,
    }
}

fn simple_kind(ty: &Type) -> ValueKind {
    match ty.oid() {
        oid::BOOL => ValueKind::Bool,
        oid::INT2 | oid::INT4 | oid::INT8 | oid::OID => ValueKind::Int,
        oid::FLOAT4 | oid::FLOAT8 => ValueKind::Float,
        oid::NUMERIC => ValueKind::Decimal,
        oid::TEXT
        | oid::VARCHAR
        | oid::BPCHAR
        | oid::CHAR
        | oid::NAME
        | oid::XML
        | oid::UNKNOWN => ValueKind::Text,
        oid::BYTEA => ValueKind::Bytes,
        oid::DATE => ValueKind::Date,
        oid::TIME | oid::TIMETZ => ValueKind::Time,
        oid::TIMESTAMP => ValueKind::Timestamp,
        oid::TIMESTAMPTZ => ValueKind::TimestampTz,
        oid::INTERVAL => ValueKind::Interval,
        oid::UUID => ValueKind::Uuid,
        oid::JSON | oid::JSONB => ValueKind::Json,
        oid::MONEY => ValueKind::Other,
        // Extension types have no fixed OID; recognise them by name.
        _ if ty.name() == "citext" => ValueKind::Text,
        _ => ValueKind::Other,
    }
}

/// Converts one non-NULL value in text format.
pub(crate) fn convert(kind: &ValueKind, text: &str) -> Value {
    let other = || Value::Other(text.to_owned());
    match kind {
        ValueKind::Bool => match text {
            "t" => Value::Bool(true),
            "f" => Value::Bool(false),
            _ => other(),
        },
        ValueKind::Int => text.parse().map_or_else(|_| other(), Value::Int),
        ValueKind::Float => match text.parse::<f64>() {
            Ok(f) if f.is_finite() => Value::Float(f),
            _ => other(),
        },
        ValueKind::Decimal => {
            if is_decimal(text) {
                Value::Decimal(text.to_owned())
            } else {
                other()
            }
        }
        ValueKind::Text => Value::Text(text.to_owned()),
        ValueKind::Bytes => parse_bytea(text).map_or_else(other, Value::Bytes),
        ValueKind::Date => {
            if is_date(text) {
                Value::Date(text.to_owned())
            } else {
                other()
            }
        }
        ValueKind::Time => {
            if is_time(text) {
                Value::Time(text.to_owned())
            } else {
                other()
            }
        }
        ValueKind::Timestamp => match text.split_once(' ') {
            Some((date, time)) if is_date(date) && is_plain_time(time) => {
                Value::Timestamp(format!("{date}T{time}"))
            }
            _ => other(),
        },
        ValueKind::TimestampTz => timestamptz_to_utc(text).map_or_else(other, Value::TimestampTz),
        ValueKind::Interval => Value::Interval(text.to_owned()),
        ValueKind::Uuid => Value::Uuid(text.to_owned()),
        // Kept as the server's text: key order, duplicate keys and number
        // precision survive (jsonb has already normalized its own text).
        ValueKind::Json => Value::json(text),
        ValueKind::Array { element, delimiter } => {
            parse_array(text, *delimiter).map_or_else(other, |node| node_to_value(&node, element))
        }
        ValueKind::Other => other(),
    }
}

fn is_decimal(text: &str) -> bool {
    let digits = text.strip_prefix('-').unwrap_or(text);
    let (int, frac) = match digits.split_once('.') {
        Some((int, frac)) if !frac.is_empty() => (int, frac),
        Some(_) => return false,
        None => (digits, ""),
    };
    !int.is_empty()
        && int.bytes().all(|b| b.is_ascii_digit())
        && frac.bytes().all(|b| b.is_ascii_digit())
}

fn all_digits(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|b| b.is_ascii_digit())
}

/// `YYYY-MM-DD` with a year of four or more digits (AD only).
fn is_date(text: &str) -> bool {
    let mut parts = text.splitn(3, '-');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(year), Some(month), Some(day)) => {
            year.len() >= 4
                && year.bytes().all(|b| b.is_ascii_digit())
                && all_digits(month, 2)
                && all_digits(day, 2)
        }
        _ => false,
    }
}

/// `HH:MM:SS` with an optional fraction.
fn is_plain_time(text: &str) -> bool {
    let (clock, fraction) = text.split_once('.').unwrap_or((text, "0"));
    let mut parts = clock.split(':');
    let ok = matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some(h), Some(m), Some(s), None) if all_digits(h, 2) && all_digits(m, 2) && all_digits(s, 2)
    );
    ok && !fraction.is_empty() && fraction.bytes().all(|b| b.is_ascii_digit())
}

/// `time` or `timetz`: a plain time, optionally followed by an offset.
fn is_time(text: &str) -> bool {
    match text.find(['+', '-']) {
        Some(at) => is_plain_time(&text[..at]) && parse_offset(&text[at..]).is_some(),
        None => is_plain_time(text),
    }
}

/// Parses `+HH`, `-HH:MM` or `+HH:MM:SS` into seconds east of UTC.
fn parse_offset(text: &str) -> Option<i64> {
    let (sign, rest) = match text.as_bytes().first()? {
        b'+' => (1, &text[1..]),
        b'-' => (-1, &text[1..]),
        _ => return None,
    };
    let mut seconds = 0i64;
    let mut count = 0;
    for (index, part) in rest.split(':').enumerate() {
        if index > 2 || !all_digits(part, 2) {
            return None;
        }
        let value: i64 = part.parse().ok()?;
        seconds += value * [3600, 60, 1][index];
        count += 1;
    }
    (count > 0).then_some(sign * seconds)
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// `2026-10-19 13:45:00.5+02` becomes `2026-10-19T11:45:00.5Z`.
fn timestamptz_to_utc(text: &str) -> Option<String> {
    let (date, rest) = text.split_once(' ')?;
    if !is_date(date) {
        return None;
    }
    let at = rest.find(['+', '-'])?;
    let (time, offset) = rest.split_at(at);
    if !is_plain_time(time) {
        return None;
    }
    let offset = parse_offset(offset)?;
    let (clock, fraction) = match time.split_once('.') {
        Some((clock, fraction)) => (clock, Some(fraction)),
        None => (time, None),
    };
    let fraction = fraction.map(|f| format!(".{f}")).unwrap_or_default();
    if offset == 0 {
        return Some(format!("{date}T{clock}{fraction}Z"));
    }
    let numbers =
        |s: &str, sep: char| -> Option<Vec<i64>> { s.split(sep).map(|p| p.parse().ok()).collect() };
    let ymd = numbers(date, '-')?;
    let hms = numbers(clock, ':')?;
    let (&[year, month, day], &[hour, minute, second]) = (ymd.as_slice(), hms.as_slice()) else {
        return None;
    };
    let local = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second;
    let utc = local - offset;
    let (year, month, day) = civil_from_days(utc.div_euclid(86_400));
    let seconds = utc.rem_euclid(86_400);
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}{fraction}Z",
        seconds / 3600,
        seconds % 3600 / 60,
        seconds % 60
    ))
}

/// Decodes `bytea` in the hex format (`\x0aff`) or the escape format.
fn parse_bytea(text: &str) -> Option<Vec<u8>> {
    if let Some(hex) = text.strip_prefix("\\x") {
        if hex.len() % 2 != 0 {
            return None;
        }
        return (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
            .collect();
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
        } else if bytes.get(i + 1) == Some(&b'\\') {
            out.push(b'\\');
            i += 2;
        } else {
            let octal = text.get(i + 1..i + 4)?;
            if !octal.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
                return None;
            }
            out.push(u8::from_str_radix(octal, 8).ok()?);
            i += 4;
        }
    }
    Some(out)
}

/// A parsed array literal.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Null,
    Item(String),
    List(Vec<Node>),
}

fn node_to_value(node: &Node, element: &ValueKind) -> Value {
    match node {
        Node::Null => Value::Null,
        Node::Item(text) => convert(element, text),
        Node::List(items) => Value::Array(
            items
                .iter()
                .map(|item| node_to_value(item, element))
                .collect(),
        ),
    }
}

/// Parses an array literal such as `{1,NULL,"a \"b\""}`, `{{1,2},{3,4}}` or
/// `[0:1]={7,8}`.
fn parse_array(text: &str, delimiter: char) -> Option<Node> {
    let body = if text.starts_with('[') {
        &text[text.find('=')? + 1..]
    } else {
        text
    };
    let chars: Vec<char> = body.chars().collect();
    let mut parser = ArrayParser {
        chars: &chars,
        at: 0,
        delimiter,
    };
    parser.skip_whitespace();
    let node = parser.list()?;
    parser.skip_whitespace();
    (parser.at == chars.len()).then_some(node)
}

struct ArrayParser<'a> {
    chars: &'a [char],
    at: usize,
    delimiter: char,
}

impl ArrayParser<'_> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn skip_whitespace(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.at += 1;
        }
    }

    fn list(&mut self) -> Option<Node> {
        if self.peek()? != '{' {
            return None;
        }
        self.at += 1;
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.peek()? == '}' {
            self.at += 1;
            return Some(Node::List(items));
        }
        loop {
            self.skip_whitespace();
            let item = match self.peek()? {
                '{' => self.list()?,
                '"' => self.quoted()?,
                _ => self.unquoted()?,
            };
            items.push(item);
            self.skip_whitespace();
            match self.peek()? {
                '}' => {
                    self.at += 1;
                    return Some(Node::List(items));
                }
                c if c == self.delimiter => self.at += 1,
                _ => return None,
            }
        }
    }

    fn quoted(&mut self) -> Option<Node> {
        self.at += 1;
        let mut out = String::new();
        loop {
            match self.peek()? {
                '"' => {
                    self.at += 1;
                    return Some(Node::Item(out));
                }
                '\\' => {
                    out.push(*self.chars.get(self.at + 1)?);
                    self.at += 2;
                }
                c => {
                    out.push(c);
                    self.at += 1;
                }
            }
        }
    }

    fn unquoted(&mut self) -> Option<Node> {
        let mut out = String::new();
        let mut escaped = false;
        // Length of `out` up to the last character that was not trailing
        // unescaped whitespace.
        let mut kept = 0;
        loop {
            let c = self.peek()?;
            if c == self.delimiter || c == '}' {
                break;
            }
            if c == '{' || c == '"' {
                return None;
            }
            if c == '\\' {
                out.push(*self.chars.get(self.at + 1)?);
                self.at += 2;
                escaped = true;
                kept = out.len();
                continue;
            }
            out.push(c);
            self.at += 1;
            if !c.is_whitespace() {
                kept = out.len();
            }
        }
        out.truncate(kept);
        if out.is_empty() {
            return None;
        }
        if !escaped && out.eq_ignore_ascii_case("NULL") {
            return Some(Node::Null);
        }
        Some(Node::Item(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn array(element: ValueKind) -> ValueKind {
        ValueKind::Array {
            element: Box::new(element),
            delimiter: ',',
        }
    }

    #[test]
    fn json_keeps_key_order_and_number_precision() -> Result<(), serde_json::Error> {
        let text = r#"{"b": 1, "a": 12345678901234567890123, "c": 1.10, "b": 2}"#;
        let value = convert(&ValueKind::Json, text);
        let Value::Json(json) = &value else {
            panic!("not JSON: {value:?}");
        };
        assert_eq!(json.as_str(), text);
        // Serialized for the UI as the original text (a JSON string), not
        // re-encoded.
        assert!(serde_json::to_string(&value)?.contains(&serde_json::to_string(text)?));
        Ok(())
    }

    #[test]
    fn kinds_that_depend_on_unreported_settings() {
        let none = HashMap::new();
        let dep = |ty: &Type| dependence(ty, &none, 18);
        assert_eq!(dep(&Type::FLOAT4), Dependence::Yes);
        assert_eq!(dep(&Type::BYTEA_ARRAY), Dependence::Yes);
        let range = Type::new(
            "fr".into(),
            90_003,
            Kind::Range(Type::FLOAT8),
            "public".into(),
        );
        assert_eq!(dep(&range), Dependence::Yes);
        assert_eq!(dep(&Type::RECORD), Dependence::Yes);
        assert_eq!(dep(&Type::NUMERIC), Dependence::No);
        assert_eq!(dep(&Type::POINT), Dependence::Yes);
        assert_eq!(dep(&Type::CIRCLE_ARRAY), Dependence::Yes);
        assert_eq!(dep(&Type::LINE), Dependence::Yes);
        let cube = Type::new("cube".into(), 90_010, Kind::Simple, "public".into());
        assert_eq!(dep(&cube), Dependence::Yes);
        assert_eq!(dep(&Type::INT4_ARRAY), Dependence::No);
        let composite = Type::new(
            "pair".into(),
            90_001,
            Kind::Composite(vec![
                tokio_postgres::types::Field::new("a".into(), Type::INT4),
                tokio_postgres::types::Field::new("b".into(), Type::FLOAT8),
            ]),
            "public".into(),
        );
        assert_eq!(dep(&composite), Dependence::Yes);
        let domain = Type::new(
            "d".into(),
            90_002,
            Kind::Domain(Type::TEXT),
            "public".into(),
        );
        assert_eq!(dep(&domain), Dependence::No);
        // A user multirange: unknown until looked up, then known.
        let multi = Type::new("frm".into(), 90_004, Kind::Simple, "public".into());
        let array = Type::new(
            "_frm".into(),
            90_005,
            Kind::Array(multi.clone()),
            "public".into(),
        );
        assert_eq!(dep(&array), Dependence::Unknown(vec![90_004]));
        assert!(dep(&array).likely());
        assert_eq!(dependence(&multi, &none, 13), Dependence::No);
        let known = HashMap::from([(90_004, true)]);
        assert_eq!(dependence(&array, &known, 18), Dependence::Yes);
        let known = HashMap::from([(90_004, false)]);
        assert_eq!(dependence(&multi, &known, 18), Dependence::No);
        assert_eq!(builtin_dependence(701), Some(true));
        assert_eq!(builtin_dependence(23), Some(false));
        assert_eq!(builtin_dependence(90_004), None);
    }

    #[test]
    fn scalars() {
        assert_eq!(convert(&ValueKind::Bool, "t"), Value::Bool(true));
        assert_eq!(convert(&ValueKind::Bool, "f"), Value::Bool(false));
        assert_eq!(
            convert(&ValueKind::Int, "-9223372036854775808"),
            Value::Int(i64::MIN)
        );
        assert_eq!(convert(&ValueKind::Float, "1.5e+20"), Value::Float(1.5e20));
        assert_eq!(
            convert(&ValueKind::Float, "NaN"),
            Value::Other("NaN".into())
        );
        assert_eq!(
            convert(&ValueKind::Float, "-Infinity"),
            Value::Other("-Infinity".into())
        );
        assert_eq!(
            convert(&ValueKind::Decimal, "-12.500"),
            Value::Decimal("-12.500".into())
        );
        assert_eq!(
            convert(&ValueKind::Decimal, "NaN"),
            Value::Other("NaN".into())
        );
        assert_eq!(
            convert(&ValueKind::Json, "{\"a\": [1, 2]}"),
            Value::json("{\"a\": [1, 2]}")
        );
        assert_eq!(
            convert(&ValueKind::Json, "{oops"),
            Value::Text("{oops".into())
        );
    }

    #[test]
    fn dates_and_times() {
        assert_eq!(
            convert(&ValueKind::Date, "2026-10-19"),
            Value::Date("2026-10-19".into())
        );
        assert_eq!(
            convert(&ValueKind::Date, "infinity"),
            Value::Other("infinity".into())
        );
        assert_eq!(
            convert(&ValueKind::Date, "0044-03-15 BC"),
            Value::Other("0044-03-15 BC".into())
        );
        assert_eq!(
            convert(&ValueKind::Time, "13:45:00.123"),
            Value::Time("13:45:00.123".into())
        );
        assert_eq!(
            convert(&ValueKind::Time, "13:45:00+05:30"),
            Value::Time("13:45:00+05:30".into())
        );
        assert_eq!(
            convert(&ValueKind::Timestamp, "2026-10-19 13:45:00.5"),
            Value::Timestamp("2026-10-19T13:45:00.5".into())
        );
        assert_eq!(
            convert(&ValueKind::Timestamp, "10/19/2026 13:45:00"),
            Value::Other("10/19/2026 13:45:00".into())
        );
    }

    #[test]
    fn timestamptz_is_normalised_to_utc() {
        let tz = |s: &str| convert(&ValueKind::TimestampTz, s);
        assert_eq!(
            tz("2026-10-19 11:45:00+00"),
            Value::TimestampTz("2026-10-19T11:45:00Z".into())
        );
        assert_eq!(
            tz("2026-10-19 13:45:00.25+02"),
            Value::TimestampTz("2026-10-19T11:45:00.25Z".into())
        );
        assert_eq!(
            tz("2026-01-01 01:00:00+05:30"),
            Value::TimestampTz("2025-12-31T19:30:00Z".into())
        );
        assert_eq!(
            tz("2024-02-28 22:00:00-03"),
            Value::TimestampTz("2024-02-29T01:00:00Z".into())
        );
        assert_eq!(
            tz("1900-01-01 00:00:00+00:19:32"),
            Value::TimestampTz("1899-12-31T23:40:28Z".into())
        );
        assert_eq!(tz("infinity"), Value::Other("infinity".into()));
    }

    #[test]
    fn civil_round_trip() {
        for days in [-1_000_000, -719_468, -1, 0, 1, 19_000, 2_932_896] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(20_745), (2026, 10, 19));
    }

    #[test]
    fn bytea_formats() {
        assert_eq!(
            convert(&ValueKind::Bytes, "\\xdeadbeef"),
            Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef])
        );
        assert_eq!(convert(&ValueKind::Bytes, "\\x"), Value::Bytes(vec![]));
        assert_eq!(
            convert(&ValueKind::Bytes, "a\\000\\\\b"),
            Value::Bytes(vec![b'a', 0, b'\\', b'b'])
        );
        assert_eq!(
            convert(&ValueKind::Bytes, "\\xabc"),
            Value::Other("\\xabc".into())
        );
    }

    #[test]
    fn one_dimensional_arrays() {
        assert_eq!(
            convert(&array(ValueKind::Int), "{1,NULL,3}"),
            Value::Array(vec![Value::Int(1), Value::Null, Value::Int(3)])
        );
        assert_eq!(convert(&array(ValueKind::Int), "{}"), Value::Array(vec![]));
        assert_eq!(
            convert(
                &array(ValueKind::Text),
                r#"{plain,"with space","quote \" and \\ slash","NULL",null," x ",a\,b}"#
            ),
            Value::Array(vec![
                Value::Text("plain".into()),
                Value::Text("with space".into()),
                Value::Text("quote \" and \\ slash".into()),
                Value::Text("NULL".into()),
                Value::Null,
                Value::Text(" x ".into()),
                Value::Text("a,b".into()),
            ])
        );
        assert_eq!(
            convert(&array(ValueKind::Bytes), r#"{"\\x00ff",NULL}"#),
            Value::Array(vec![Value::Bytes(vec![0, 255]), Value::Null])
        );
        assert_eq!(
            convert(&array(ValueKind::Json), r#"{"{\"a\": 1}",null}"#),
            Value::Array(vec![Value::json("{\"a\": 1}"), Value::Null])
        );
    }

    #[test]
    fn nested_and_decorated_arrays() {
        assert_eq!(
            convert(&array(ValueKind::Int), "{{1,2},{3,4}}"),
            Value::Array(vec![
                Value::Array(vec![Value::Int(1), Value::Int(2)]),
                Value::Array(vec![Value::Int(3), Value::Int(4)]),
            ])
        );
        assert_eq!(
            convert(&array(ValueKind::Int), "[0:1]={7,8}"),
            Value::Array(vec![Value::Int(7), Value::Int(8)])
        );
        let boxes = ValueKind::Array {
            element: Box::new(ValueKind::Other),
            delimiter: ';',
        };
        assert_eq!(
            convert(&boxes, "{(1,1),(0,0);(2,2),(1,1)}"),
            Value::Array(vec![
                Value::Other("(1,1),(0,0)".into()),
                Value::Other("(2,2),(1,1)".into()),
            ])
        );
    }

    #[test]
    fn malformed_arrays_stay_text() {
        for bad in ["{1,2", "1,2}", "{1,,2}", "{\"a}", "{1}x"] {
            assert_eq!(
                convert(&array(ValueKind::Int), bad),
                Value::Other(bad.into()),
                "{bad}"
            );
        }
    }

    #[test]
    fn type_names_and_kinds() {
        let mut cache = TypeCache::default();
        let int4_array = cache.resolve(&Type::INT4_ARRAY);
        assert_eq!(int4_array.name, "int4[]");
        assert_eq!(int4_array.kind, array(ValueKind::Int));
        assert_eq!(cache.resolve(&Type::INT8).name, "int8");
        assert_eq!(cache.resolve(&Type::MONEY).kind, ValueKind::Other);
        assert_eq!(cache.resolve(&Type::JSONB).kind, ValueKind::Json);

        let mood = Type::new(
            "mood".into(),
            90_001,
            Kind::Enum(vec!["sad".into(), "ok".into()]),
            "public".into(),
        );
        assert_eq!(cache.resolve(&mood).kind, ValueKind::Text);
        assert_eq!(cache.resolve(&mood).name, "mood");

        let positive = Type::new(
            "positive".into(),
            90_002,
            Kind::Domain(Type::INT4),
            "public".into(),
        );
        assert_eq!(cache.resolve(&positive).kind, ValueKind::Int);
        let positives = Type::new(
            "_positive".into(),
            90_003,
            Kind::Array(positive),
            "public".into(),
        );
        assert_eq!(cache.resolve(&positives).name, "positive[]");
        assert_eq!(cache.resolve(&positives).kind, array(ValueKind::Int));

        let citext = Type::new("citext".into(), 90_004, Kind::Simple, "public".into());
        assert_eq!(cache.resolve(&citext).kind, ValueKind::Text);
        assert_eq!(
            cache.resolve(&Type::BOX_ARRAY).kind,
            ValueKind::Array {
                element: Box::new(ValueKind::Other),
                delimiter: ';',
            }
        );
    }
}
