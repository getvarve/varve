//! Building blocks shared by the editors: data types, column definitions,
//! index definitions, definers.

use serde::{Deserialize, Serialize};
use varve_driver::{Dialect, Value};

use super::{Plan, Target, fragment, ident, invalid, plain_text, unsupported};
use crate::model::CatalogResult;
use crate::sql;

/// A MySQL or MariaDB data type name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TypeName {
    /// `TINYINT`.
    Tinyint,
    /// `SMALLINT`.
    Smallint,
    /// `MEDIUMINT`.
    Mediumint,
    /// `INT`.
    Int,
    /// `BIGINT`.
    Bigint,
    /// `DECIMAL(precision, scale)`.
    Decimal,
    /// `FLOAT`.
    Float,
    /// `DOUBLE`.
    Double,
    /// `BIT(width)`.
    Bit,
    /// `BOOLEAN` (an alias of `TINYINT(1)`).
    Boolean,
    /// `CHAR(length)`.
    Char,
    /// `VARCHAR(length)`.
    Varchar,
    /// `BINARY(length)`.
    Binary,
    /// `VARBINARY(length)`.
    Varbinary,
    /// `TINYTEXT`.
    Tinytext,
    /// `TEXT`.
    Text,
    /// `MEDIUMTEXT`.
    Mediumtext,
    /// `LONGTEXT`.
    Longtext,
    /// `TINYBLOB`.
    Tinyblob,
    /// `BLOB`.
    Blob,
    /// `MEDIUMBLOB`.
    Mediumblob,
    /// `LONGBLOB`.
    Longblob,
    /// `ENUM('a', ...)`.
    Enum,
    /// `SET('a', ...)`.
    Set,
    /// `DATE`.
    Date,
    /// `TIME(fsp)`.
    Time,
    /// `DATETIME(fsp)`.
    Datetime,
    /// `TIMESTAMP(fsp)`.
    Timestamp,
    /// `YEAR`.
    Year,
    /// `JSON` (on MariaDB an alias of `LONGTEXT` with a `JSON_VALID` check).
    Json,
    /// `GEOMETRY`.
    Geometry,
    /// `POINT`.
    Point,
    /// `LINESTRING`.
    Linestring,
    /// `POLYGON`.
    Polygon,
    /// `MULTIPOINT`.
    Multipoint,
    /// `MULTILINESTRING`.
    Multilinestring,
    /// `MULTIPOLYGON`.
    Multipolygon,
    /// `GEOMETRYCOLLECTION`.
    Geometrycollection,
    /// `INET4` (MariaDB 10.10+).
    Inet4,
    /// `INET6` (MariaDB 10.5+).
    Inet6,
    /// `UUID` (MariaDB 10.7+).
    Uuid,
    /// `VECTOR(dimensions)` (MySQL 9.0+, MariaDB 11.7+).
    Vector,
}

impl TypeName {
    fn keyword(self) -> &'static str {
        match self {
            Self::Tinyint => "TINYINT",
            Self::Smallint => "SMALLINT",
            Self::Mediumint => "MEDIUMINT",
            Self::Int => "INT",
            Self::Bigint => "BIGINT",
            Self::Decimal => "DECIMAL",
            Self::Float => "FLOAT",
            Self::Double => "DOUBLE",
            Self::Bit => "BIT",
            Self::Boolean => "BOOLEAN",
            Self::Char => "CHAR",
            Self::Varchar => "VARCHAR",
            Self::Binary => "BINARY",
            Self::Varbinary => "VARBINARY",
            Self::Tinytext => "TINYTEXT",
            Self::Text => "TEXT",
            Self::Mediumtext => "MEDIUMTEXT",
            Self::Longtext => "LONGTEXT",
            Self::Tinyblob => "TINYBLOB",
            Self::Blob => "BLOB",
            Self::Mediumblob => "MEDIUMBLOB",
            Self::Longblob => "LONGBLOB",
            Self::Enum => "ENUM",
            Self::Set => "SET",
            Self::Date => "DATE",
            Self::Time => "TIME",
            Self::Datetime => "DATETIME",
            Self::Timestamp => "TIMESTAMP",
            Self::Year => "YEAR",
            Self::Json => "JSON",
            Self::Geometry => "GEOMETRY",
            Self::Point => "POINT",
            Self::Linestring => "LINESTRING",
            Self::Polygon => "POLYGON",
            Self::Multipoint => "MULTIPOINT",
            Self::Multilinestring => "MULTILINESTRING",
            Self::Multipolygon => "MULTIPOLYGON",
            Self::Geometrycollection => "GEOMETRYCOLLECTION",
            Self::Inet4 => "INET4",
            Self::Inet6 => "INET6",
            Self::Uuid => "UUID",
            Self::Vector => "VECTOR",
        }
    }

    pub(super) fn is_integer(self) -> bool {
        matches!(
            self,
            Self::Tinyint | Self::Smallint | Self::Mediumint | Self::Int | Self::Bigint
        )
    }

    fn is_inexact_or_decimal(self) -> bool {
        matches!(self, Self::Decimal | Self::Float | Self::Double)
    }

    fn is_numeric(self) -> bool {
        self.is_integer() || self.is_inexact_or_decimal()
    }

    /// Types that take a character set and collation.
    pub(super) fn is_character(self) -> bool {
        matches!(
            self,
            Self::Char
                | Self::Varchar
                | Self::Tinytext
                | Self::Text
                | Self::Mediumtext
                | Self::Longtext
                | Self::Enum
                | Self::Set
        )
    }

    pub(super) fn is_spatial(self) -> bool {
        matches!(
            self,
            Self::Geometry
                | Self::Point
                | Self::Linestring
                | Self::Polygon
                | Self::Multipoint
                | Self::Multilinestring
                | Self::Multipolygon
                | Self::Geometrycollection
        )
    }

    /// Types MySQL gives only expression defaults (`DEFAULT (expr)`).
    pub(super) fn mysql_expression_default_only(self) -> bool {
        self.is_spatial()
            || matches!(
                self,
                Self::Tinytext
                    | Self::Text
                    | Self::Mediumtext
                    | Self::Longtext
                    | Self::Tinyblob
                    | Self::Blob
                    | Self::Mediumblob
                    | Self::Longblob
                    | Self::Json
            )
    }

    /// Types `CURRENT_TIMESTAMP` defaults and `ON UPDATE` apply to.
    fn takes_current_timestamp(self) -> bool {
        matches!(self, Self::Datetime | Self::Timestamp)
    }
}

/// A full data type: name, length or precision, members, sign and
/// character set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataType {
    /// The type.
    pub name: TypeName,
    /// Length (`CHAR`, `VARCHAR`, `BINARY`, `VARBINARY`, `TEXT`, `BLOB`),
    /// precision (`DECIMAL`, `FLOAT`, `DOUBLE`), width (`BIT`, deprecated
    /// integer display width), fractional seconds (`TIME`, `DATETIME`,
    /// `TIMESTAMP`) or dimensions (`VECTOR`).
    #[serde(default)]
    pub length: Option<u32>,
    /// Scale of `DECIMAL`, `FLOAT` and `DOUBLE`.
    #[serde(default)]
    pub scale: Option<u32>,
    /// Members of `ENUM` and `SET`.
    #[serde(default)]
    pub values: Vec<String>,
    /// `UNSIGNED` (numeric types).
    #[serde(default)]
    pub unsigned: bool,
    /// `ZEROFILL` (numeric types; deprecated on MySQL 8.0.17+).
    #[serde(default)]
    pub zerofill: bool,
    /// Character set of a character type.
    #[serde(default)]
    pub charset: Option<String>,
    /// Collation of a character type.
    #[serde(default)]
    pub collation: Option<String>,
}

impl DataType {
    /// A type with nothing but its name.
    #[must_use]
    pub fn new(name: TypeName) -> Self {
        Self {
            name,
            length: None,
            scale: None,
            values: Vec::new(),
            unsigned: false,
            zerofill: false,
            charset: None,
            collation: None,
        }
    }

    /// The type as SQL, after checking it against the server.
    pub(super) fn render(&self, t: &Target, plan: &mut Plan) -> CatalogResult<String> {
        let name = self.name;
        let keyword = name.keyword();
        self.check_available(t)?;
        if self.scale.is_some() && !name.is_inexact_or_decimal() {
            return Err(invalid(format!("{keyword} takes no scale")));
        }
        if !self.values.is_empty() && !matches!(name, TypeName::Enum | TypeName::Set) {
            return Err(invalid(format!("{keyword} takes no member list")));
        }
        let mut out = String::from(keyword);
        match name {
            TypeName::Tinyint
            | TypeName::Smallint
            | TypeName::Mediumint
            | TypeName::Int
            | TypeName::Bigint => {
                if let Some(width) = self.length {
                    if !(1..=255).contains(&width) {
                        return Err(invalid("an integer display width must be 1 to 255"));
                    }
                    let tinyint_bool = name == TypeName::Tinyint && width == 1;
                    if t.mysql_at(8, 0, 17) && !tinyint_bool {
                        plan.warn(
                            "integer display widths are deprecated on MySQL 8.0.17 and newer",
                        );
                    }
                    out.push_str(&format!("({width})"));
                }
            }
            TypeName::Decimal => match (self.length, self.scale) {
                (None, None) => {}
                (Some(precision), scale) => {
                    if !(1..=65).contains(&precision) {
                        return Err(invalid("DECIMAL precision must be 1 to 65"));
                    }
                    match scale {
                        Some(scale) if scale > 30 || scale > precision => {
                            return Err(invalid(
                                "DECIMAL scale must be at most 30 and at most the precision",
                            ));
                        }
                        Some(scale) => out.push_str(&format!("({precision},{scale})")),
                        None => out.push_str(&format!("({precision})")),
                    }
                }
                (None, Some(_)) => {
                    return Err(invalid("a DECIMAL scale needs a precision"));
                }
            },
            TypeName::Float | TypeName::Double => match (self.length, self.scale) {
                (None, None) => {}
                (Some(precision), None) if name == TypeName::Float => {
                    if precision > 53 {
                        return Err(invalid("FLOAT precision must be 0 to 53"));
                    }
                    out.push_str(&format!("({precision})"));
                }
                (Some(digits), Some(scale)) => {
                    if !(1..=255).contains(&digits) || scale > 30 || scale > digits {
                        return Err(invalid(format!(
                            "{keyword}(M,D) needs M from 1 to 255 and D at most 30 and at most M"
                        )));
                    }
                    if t.mysql_at(8, 0, 17) {
                        plan.warn(format!(
                            "{keyword}(M,D) is deprecated on MySQL 8.0.17 and newer"
                        ));
                    }
                    out.push_str(&format!("({digits},{scale})"));
                }
                _ => {
                    return Err(invalid(format!(
                        "{keyword} takes either no precision or both digits and scale"
                    )));
                }
            },
            TypeName::Bit => {
                if let Some(width) = self.length {
                    if !(1..=64).contains(&width) {
                        return Err(invalid("BIT width must be 1 to 64"));
                    }
                    out.push_str(&format!("({width})"));
                }
            }
            TypeName::Char | TypeName::Binary => {
                if let Some(length) = self.length {
                    if length > 255 {
                        return Err(invalid(format!("{keyword} length must be 0 to 255")));
                    }
                    out.push_str(&format!("({length})"));
                }
            }
            TypeName::Varchar | TypeName::Varbinary => {
                let length = self
                    .length
                    .ok_or_else(|| invalid(format!("{keyword} needs a length")))?;
                if length > 65_535 {
                    return Err(invalid(format!("{keyword} length must be 0 to 65535")));
                }
                out.push_str(&format!("({length})"));
            }
            TypeName::Text | TypeName::Blob => {
                if let Some(length) = self.length {
                    out.push_str(&format!("({length})"));
                }
            }
            TypeName::Enum | TypeName::Set => {
                if self.length.is_some() {
                    return Err(invalid(format!("{keyword} takes no length")));
                }
                if self.values.is_empty() {
                    return Err(invalid(format!("{keyword} needs at least one member")));
                }
                if name == TypeName::Set && self.values.len() > 64 {
                    return Err(invalid("SET has at most 64 members"));
                }
                let members = self
                    .values
                    .iter()
                    .map(|v| plain_text(v, "an ENUM or SET member"))
                    .collect::<CatalogResult<Vec<_>>>()?;
                out.push_str(&format!("({})", members.join(",")));
            }
            TypeName::Time | TypeName::Datetime | TypeName::Timestamp => {
                if let Some(fsp) = self.length {
                    if fsp > 6 {
                        return Err(invalid("fractional seconds precision must be 0 to 6"));
                    }
                    out.push_str(&format!("({fsp})"));
                }
            }
            TypeName::Vector => {
                let dimensions = self
                    .length
                    .ok_or_else(|| invalid("VECTOR needs a number of dimensions"))?;
                if dimensions == 0 {
                    return Err(invalid("VECTOR needs at least one dimension"));
                }
                out.push_str(&format!("({dimensions})"));
            }
            TypeName::Boolean
            | TypeName::Tinytext
            | TypeName::Mediumtext
            | TypeName::Longtext
            | TypeName::Tinyblob
            | TypeName::Mediumblob
            | TypeName::Longblob
            | TypeName::Date
            | TypeName::Year
            | TypeName::Json
            | TypeName::Geometry
            | TypeName::Point
            | TypeName::Linestring
            | TypeName::Polygon
            | TypeName::Multipoint
            | TypeName::Multilinestring
            | TypeName::Multipolygon
            | TypeName::Geometrycollection
            | TypeName::Inet4
            | TypeName::Inet6
            | TypeName::Uuid => {
                if self.length.is_some() {
                    return Err(invalid(format!("{keyword} takes no length")));
                }
            }
        }
        if self.unsigned || self.zerofill {
            if !name.is_numeric() {
                return Err(invalid(format!("{keyword} cannot be UNSIGNED or ZEROFILL")));
            }
            // ZEROFILL implies UNSIGNED; the server shows both.
            out.push_str(" UNSIGNED");
            if self.zerofill {
                out.push_str(" ZEROFILL");
                if t.mysql_at(8, 0, 17) {
                    plan.warn("ZEROFILL is deprecated on MySQL 8.0.17 and newer");
                }
            }
            if name.is_inexact_or_decimal() && t.mysql_at(8, 0, 17) {
                plan.warn(format!(
                    "UNSIGNED {keyword} is deprecated on MySQL 8.0.17 and newer"
                ));
            }
        }
        if self.charset.is_some() || self.collation.is_some() {
            if !name.is_character() {
                return Err(invalid(format!(
                    "{keyword} takes no character set or collation"
                )));
            }
            if let Some(charset) = &self.charset {
                out.push_str(&format!(" CHARACTER SET {}", ident(charset)?));
            }
            if let Some(collation) = &self.collation {
                out.push_str(&format!(" COLLATE {}", ident(collation)?));
            }
        }
        Ok(out)
    }

    fn check_available(&self, t: &Target) -> CatalogResult<()> {
        let missing = match self.name {
            TypeName::Inet6 => !t.mariadb_at(10, 5),
            TypeName::Uuid => !t.mariadb_at(10, 7),
            TypeName::Inet4 => !t.mariadb_at(10, 10),
            TypeName::Vector => !(t.mysql_at(9, 0, 0) || t.mariadb_at(11, 7)),
            _ => false,
        };
        if missing {
            return Err(t.lacks(&format!("the {} type", self.name.keyword())));
        }
        Ok(())
    }
}

/// The default of a column.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ColumnDefault {
    /// `DEFAULT NULL`.
    Null,
    /// A constant value, quoted for the column's type.
    Literal {
        /// The value.
        value: Value,
    },
    /// `DEFAULT CURRENT_TIMESTAMP[(precision)]` for `DATETIME` and
    /// `TIMESTAMP`.
    CurrentTimestamp {
        /// Fractional seconds; must match the column's.
        #[serde(default)]
        precision: Option<u32>,
    },
    /// An expression default, `DEFAULT (expr)` (MySQL 8.0.13+, MariaDB).
    Expression {
        /// The expression.
        sql: String,
    },
}

impl ColumnDefault {
    /// The `DEFAULT ...` clause for a column of type `ty`.
    pub(super) fn render(
        &self,
        t: &Target,
        ty: &DataType,
        nullable: bool,
    ) -> CatalogResult<String> {
        Ok(match self {
            Self::Null => {
                if !nullable {
                    return Err(invalid("a NOT NULL column cannot default to NULL"));
                }
                "DEFAULT NULL".into()
            }
            Self::Literal { value } => {
                let literal = sql::literal(Dialect::Mysql, value)?;
                if matches!(value, Value::Null) && !nullable {
                    return Err(invalid("a NOT NULL column cannot default to NULL"));
                }
                if !t.is_mariadb() && ty.name.mysql_expression_default_only() {
                    // MySQL accepts defaults on these types only as
                    // expressions.
                    if !t.mysql_at(8, 0, 13) {
                        return Err(t.lacks(&format!(
                            "defaults on {} columns (MySQL 8.0.13+)",
                            ty.name.keyword()
                        )));
                    }
                    format!("DEFAULT ({literal})")
                } else {
                    format!("DEFAULT {literal}")
                }
            }
            Self::CurrentTimestamp { precision } => {
                if !ty.name.takes_current_timestamp() {
                    return Err(invalid(
                        "CURRENT_TIMESTAMP defaults apply to DATETIME and TIMESTAMP; use an expression default for other types",
                    ));
                }
                check_timestamp_precision(ty, *precision)?;
                format!("DEFAULT {}", current_timestamp(*precision))
            }
            Self::Expression { sql } => {
                if !(t.mysql_at(8, 0, 13) || t.is_mariadb()) {
                    return Err(t.lacks("expression defaults (MySQL 8.0.13+)"));
                }
                format!("DEFAULT ({})", fragment(sql, "a default expression")?)
            }
        })
    }

    fn is_expression(&self) -> bool {
        matches!(self, Self::Expression { .. })
    }
}

fn current_timestamp(precision: Option<u32>) -> String {
    match precision {
        Some(p) if p > 0 => format!("CURRENT_TIMESTAMP({p})"),
        _ => "CURRENT_TIMESTAMP".into(),
    }
}

/// MySQL and MariaDB need `CURRENT_TIMESTAMP(p)` to match the column's
/// fractional seconds.
fn check_timestamp_precision(ty: &DataType, precision: Option<u32>) -> CatalogResult<()> {
    if precision.unwrap_or(0) != ty.length.unwrap_or(0) {
        return Err(invalid(
            "CURRENT_TIMESTAMP must have the same fractional seconds precision as the column",
        ));
    }
    Ok(())
}

/// How a generated column is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GeneratedStorage {
    /// Computed when read.
    Virtual,
    /// Computed when written and stored.
    Stored,
}

/// A generated column's expression.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Generated {
    /// The expression.
    pub expression: String,
    /// Virtual or stored.
    pub storage: GeneratedStorage,
}

/// MariaDB system-versioning row period column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RowPeriod {
    /// `GENERATED ALWAYS AS ROW START`.
    Start,
    /// `GENERATED ALWAYS AS ROW END`.
    End,
}

/// A column of the table designer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnDef {
    /// Column name.
    pub name: String,
    /// Type, with character set and collation for character types.
    pub data_type: DataType,
    /// `NULL` when true, `NOT NULL` when false.
    #[serde(default = "yes")]
    pub nullable: bool,
    /// The default.
    #[serde(default)]
    pub default: Option<ColumnDefault>,
    /// `AUTO_INCREMENT` (integer columns).
    #[serde(default)]
    pub auto_increment: bool,
    /// `ON UPDATE CURRENT_TIMESTAMP[(precision)]`: the precision, or
    /// `None` for no `ON UPDATE`. Use `Some(0)` for plain
    /// `CURRENT_TIMESTAMP`.
    #[serde(default)]
    pub on_update_current_timestamp: Option<u32>,
    /// Generated column expression.
    #[serde(default)]
    pub generated: Option<Generated>,
    /// `INVISIBLE` (MySQL 8.0.23+, MariaDB).
    #[serde(default)]
    pub invisible: bool,
    /// Column comment.
    #[serde(default)]
    pub comment: Option<String>,
    /// Spatial reference system of a spatial column (`SRID` on MySQL,
    /// `REF_SYSTEM_ID` on MariaDB).
    #[serde(default)]
    pub srid: Option<u32>,
    /// MariaDB: `WITHOUT SYSTEM VERSIONING` (changes are not versioned).
    #[serde(default)]
    pub without_system_versioning: bool,
    /// MariaDB: the column is a system-versioning row start or end column.
    #[serde(default)]
    pub row_period: Option<RowPeriod>,
}

fn yes() -> bool {
    true
}

impl ColumnDef {
    /// A nullable column with only a name and a type.
    #[must_use]
    pub fn new(name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            name: name.into(),
            data_type,
            nullable: true,
            default: None,
            auto_increment: false,
            on_update_current_timestamp: None,
            generated: None,
            invisible: false,
            comment: None,
            srid: None,
            without_system_versioning: false,
            row_period: None,
        }
    }

    /// True when the definition contains user SQL (generated or default
    /// expressions) that may name other columns.
    pub(super) fn has_expressions(&self) -> bool {
        self.generated.is_some()
            || self
                .default
                .as_ref()
                .is_some_and(ColumnDefault::is_expression)
    }

    /// True for a stored generated column.
    pub(super) fn is_stored_generated(&self) -> bool {
        self.generated
            .as_ref()
            .is_some_and(|g| g.storage == GeneratedStorage::Stored)
    }

    /// `name type attributes...`.
    pub(super) fn render(&self, t: &Target, plan: &mut Plan) -> CatalogResult<String> {
        let ty = &self.data_type;
        let mut out = format!("{} {}", ident(&self.name)?, ty.render(t, plan)?);
        if let Some(srid) = self.srid {
            if !ty.name.is_spatial() {
                return Err(invalid("only spatial columns have an SRID"));
            }
            if t.is_mariadb() {
                out.push_str(&format!(" REF_SYSTEM_ID={srid}"));
            } else {
                out.push_str(&format!(" SRID {srid}"));
            }
        }
        let special = self.generated.is_some() || self.row_period.is_some();
        if special && (self.default.is_some() || self.auto_increment) {
            return Err(invalid(
                "a generated column has no default and no AUTO_INCREMENT",
            ));
        }
        if special && self.on_update_current_timestamp.is_some() {
            return Err(invalid("a generated column has no ON UPDATE"));
        }
        if let Some(generated) = &self.generated {
            if self.row_period.is_some() {
                return Err(invalid(
                    "a column is either generated from an expression or a row period column",
                ));
            }
            let storage = match generated.storage {
                GeneratedStorage::Virtual => "VIRTUAL",
                GeneratedStorage::Stored => "STORED",
            };
            out.push_str(&format!(
                " GENERATED ALWAYS AS ({}) {storage}",
                fragment(&generated.expression, "a generated column expression")?
            ));
        }
        if let Some(period) = self.row_period {
            if !t.is_mariadb() {
                return Err(t.lacks("system-versioned tables"));
            }
            let timestamp6 = ty.name == TypeName::Timestamp && ty.length == Some(6);
            let bigint_unsigned = ty.name == TypeName::Bigint && ty.unsigned;
            if !(timestamp6 || bigint_unsigned) {
                return Err(invalid(
                    "row start and end columns are TIMESTAMP(6) or BIGINT UNSIGNED",
                ));
            }
            out.push_str(match period {
                RowPeriod::Start => " GENERATED ALWAYS AS ROW START",
                RowPeriod::End => " GENERATED ALWAYS AS ROW END",
            });
        }
        if special && t.is_mariadb() {
            // MariaDB rejects NULL / NOT NULL on generated columns.
            if !self.nullable && self.generated.is_some() {
                return Err(t.lacks("NOT NULL on generated columns"));
            }
        } else {
            out.push_str(if self.nullable { " NULL" } else { " NOT NULL" });
        }
        if let Some(default) = &self.default {
            out.push(' ');
            out.push_str(&default.render(t, ty, self.nullable)?);
        }
        if let Some(precision) = self.on_update_current_timestamp {
            if !ty.name.takes_current_timestamp() {
                return Err(invalid(
                    "ON UPDATE CURRENT_TIMESTAMP applies to DATETIME and TIMESTAMP",
                ));
            }
            check_timestamp_precision(ty, Some(precision))?;
            out.push_str(&format!(
                " ON UPDATE {}",
                current_timestamp(Some(precision))
            ));
        }
        if self.auto_increment {
            if !ty.name.is_integer() {
                return Err(invalid("AUTO_INCREMENT applies to integer columns"));
            }
            out.push_str(" AUTO_INCREMENT");
        }
        if self.invisible {
            if !(t.is_mariadb() || t.mysql_at(8, 0, 23)) {
                return Err(t.lacks("invisible columns (MySQL 8.0.23+)"));
            }
            out.push_str(" INVISIBLE");
        }
        if self.without_system_versioning {
            if !t.is_mariadb() {
                return Err(t.lacks("system-versioned tables"));
            }
            out.push_str(" WITHOUT SYSTEM VERSIONING");
        }
        if let Some(comment) = &self.comment {
            out.push_str(&format!(" COMMENT {}", plain_text(comment, "a comment")?));
        }
        Ok(out)
    }
}

/// One part of an index key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum KeyPart {
    /// A column, optionally a prefix of it.
    Column {
        /// Column name.
        name: String,
        /// Prefix length in characters (bytes for binary types).
        #[serde(default)]
        length: Option<u32>,
        /// Descending order.
        #[serde(default)]
        descending: bool,
    },
    /// A functional key part (MySQL 8.0.13+).
    Expression {
        /// The expression.
        sql: String,
        /// Descending order.
        #[serde(default)]
        descending: bool,
    },
}

impl KeyPart {
    /// A plain ascending column.
    #[must_use]
    pub fn column(name: impl Into<String>) -> Self {
        Self::Column {
            name: name.into(),
            length: None,
            descending: false,
        }
    }

    pub(super) fn column_name(&self) -> Option<&str> {
        match self {
            Self::Column { name, .. } => Some(name),
            Self::Expression { .. } => None,
        }
    }

    fn descending(&self) -> bool {
        match self {
            Self::Column { descending, .. } | Self::Expression { descending, .. } => *descending,
        }
    }
}

/// What an index is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum IndexKind {
    /// A plain index.
    Index,
    /// A unique index.
    Unique,
    /// A full-text index.
    Fulltext,
    /// A spatial index.
    Spatial,
}

/// Index structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum IndexMethod {
    /// `USING BTREE`.
    Btree,
    /// `USING HASH` (MEMORY and NDB; InnoDB builds a B-tree).
    Hash,
}

impl IndexMethod {
    pub(super) fn clause(self) -> &'static str {
        match self {
            Self::Btree => "USING BTREE",
            Self::Hash => "USING HASH",
        }
    }
}

/// An index (also a unique key) of a table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexDef {
    /// Index name; inside `CREATE TABLE` and `ADD INDEX` it may be left
    /// for the server to choose.
    #[serde(default)]
    pub name: Option<String>,
    /// Plain, unique, full-text or spatial.
    pub kind: IndexKind,
    /// B-tree or hash (plain and unique indexes only).
    #[serde(default)]
    pub method: Option<IndexMethod>,
    /// Key parts in order.
    pub parts: Vec<KeyPart>,
    /// Full-text parser plugin (`WITH PARSER`), for example `ngram`.
    #[serde(default)]
    pub parser: Option<String>,
    /// Index comment.
    #[serde(default)]
    pub comment: Option<String>,
    /// Invisible to the optimizer: `INVISIBLE` on MySQL, `IGNORED` on
    /// MariaDB 10.6+.
    #[serde(default)]
    pub invisible: bool,
}

impl IndexDef {
    /// Checks the definition and returns the quoted key part list with its
    /// parentheses.
    pub(super) fn parts_sql(&self, t: &Target, plan: &mut Plan) -> CatalogResult<String> {
        if self.parts.is_empty() {
            return Err(invalid("an index needs at least one key part"));
        }
        if let Some(name) = &self.name
            && name.eq_ignore_ascii_case("PRIMARY")
        {
            return Err(invalid(
                "PRIMARY is reserved for the primary key; add a primary key constraint instead",
            ));
        }
        let special = matches!(self.kind, IndexKind::Fulltext | IndexKind::Spatial);
        if special && self.method.is_some() {
            return Err(invalid(
                "full-text and spatial indexes take no USING BTREE or USING HASH",
            ));
        }
        if self.parser.is_some() && self.kind != IndexKind::Fulltext {
            return Err(invalid("only full-text indexes have a parser"));
        }
        if self.kind == IndexKind::Spatial && self.parts.len() != 1 {
            return Err(invalid("a spatial index has exactly one column"));
        }
        if self.method == Some(IndexMethod::Hash) {
            plan.warn("USING HASH builds a hash index only on MEMORY and NDB tables; InnoDB, MyISAM and Aria build a B-tree");
        }
        let mut parts = Vec::with_capacity(self.parts.len());
        for part in &self.parts {
            if part.descending() {
                if special {
                    return Err(invalid(
                        "full-text and spatial key parts cannot be descending",
                    ));
                }
                if t.is_mariadb() && !t.mariadb_at(10, 8) {
                    plan.warn(
                        "MariaDB before 10.8 parses DESC in an index but builds it ascending",
                    );
                }
            }
            let mut sql = match part {
                KeyPart::Column { name, length, .. } => {
                    let mut sql = ident(name)?;
                    if let Some(length) = length {
                        if special {
                            return Err(invalid(
                                "full-text and spatial key parts take no prefix length",
                            ));
                        }
                        if *length == 0 {
                            return Err(invalid("a prefix length must be at least 1"));
                        }
                        sql.push_str(&format!("({length})"));
                    }
                    sql
                }
                KeyPart::Expression { sql, .. } => {
                    if t.is_mariadb() {
                        return Err(unsupported(
                            "MariaDB has no functional key parts; index a generated column instead",
                        ));
                    }
                    if !t.mysql_at(8, 0, 13) {
                        return Err(t.lacks("functional key parts (MySQL 8.0.13+)"));
                    }
                    if special {
                        return Err(invalid(
                            "full-text and spatial indexes cannot have functional key parts",
                        ));
                    }
                    format!("({})", fragment(sql, "a key part expression")?)
                }
            };
            if part.descending() {
                sql.push_str(" DESC");
            }
            parts.push(sql);
        }
        Ok(format!("({})", parts.join(", ")))
    }

    /// Options after the key parts: parser, comment, visibility.
    fn options_sql(&self, t: &Target) -> CatalogResult<String> {
        let mut out = String::new();
        if let Some(parser) = &self.parser {
            out.push_str(&format!(" WITH PARSER {}", ident(parser)?));
        }
        if let Some(comment) = &self.comment {
            out.push_str(&format!(" COMMENT {}", plain_text(comment, "a comment")?));
        }
        if self.invisible {
            out.push(' ');
            out.push_str(visibility_keyword(t, true)?);
        }
        Ok(out)
    }

    /// The index inside `CREATE TABLE` or after `ALTER TABLE ... ADD`.
    pub(super) fn inline(&self, t: &Target, plan: &mut Plan) -> CatalogResult<String> {
        let parts = self.parts_sql(t, plan)?;
        let mut out = String::from(match self.kind {
            IndexKind::Index => "INDEX",
            IndexKind::Unique => "UNIQUE INDEX",
            IndexKind::Fulltext => "FULLTEXT INDEX",
            IndexKind::Spatial => "SPATIAL INDEX",
        });
        if let Some(name) = &self.name {
            out.push(' ');
            out.push_str(&ident(name)?);
        }
        if let Some(method) = self.method {
            out.push(' ');
            out.push_str(method.clause());
        }
        out.push(' ');
        out.push_str(&parts);
        out.push_str(&self.options_sql(t)?);
        Ok(out)
    }

    /// `CREATE ... INDEX name [USING ...] ON table (parts) options`.
    pub(super) fn create_statement(
        &self,
        t: &Target,
        table: &str,
        plan: &mut Plan,
    ) -> CatalogResult<String> {
        let name = self
            .name
            .as_deref()
            .ok_or_else(|| invalid("CREATE INDEX needs an index name"))?;
        let parts = self.parts_sql(t, plan)?;
        let kind = match self.kind {
            IndexKind::Index => "",
            IndexKind::Unique => "UNIQUE ",
            IndexKind::Fulltext => "FULLTEXT ",
            IndexKind::Spatial => "SPATIAL ",
        };
        let mut out = format!("CREATE {kind}INDEX {}", ident(name)?);
        if let Some(method) = self.method {
            out.push(' ');
            out.push_str(method.clause());
        }
        out.push_str(&format!(" ON {table} {parts}"));
        out.push_str(&self.options_sql(t)?);
        Ok(out)
    }

    /// Column names the key parts use.
    pub(super) fn column_names(&self) -> impl Iterator<Item = &str> {
        self.parts.iter().filter_map(KeyPart::column_name)
    }

    pub(super) fn has_expressions(&self) -> bool {
        self.parts
            .iter()
            .any(|p| matches!(p, KeyPart::Expression { .. }))
    }
}

/// `INVISIBLE` / `VISIBLE` on MySQL, `IGNORED` / `NOT IGNORED` on MariaDB.
pub(super) fn visibility_keyword(t: &Target, invisible: bool) -> CatalogResult<&'static str> {
    if t.is_mariadb() {
        if !t.mariadb_at(10, 6) {
            return Err(t.lacks("ignored indexes (MariaDB 10.6+)"));
        }
        Ok(if invisible { "IGNORED" } else { "NOT IGNORED" })
    } else {
        Ok(if invisible { "INVISIBLE" } else { "VISIBLE" })
    }
}

/// The account a view, routine, trigger or event runs as.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Definer {
    /// `DEFINER = CURRENT_USER`.
    CurrentUser,
    /// `DEFINER = 'user'@'host'`.
    Account {
        /// User name.
        user: String,
        /// Host part, for example `%` or `localhost`.
        host: String,
    },
}

impl Definer {
    pub(super) fn clause(&self) -> CatalogResult<String> {
        Ok(match self {
            Self::CurrentUser => "DEFINER = CURRENT_USER".into(),
            Self::Account { user, host } => {
                if user.is_empty() {
                    return Err(invalid("a definer needs a user name"));
                }
                format!(
                    "DEFINER = {}@{}",
                    plain_text(user, "a definer user")?,
                    plain_text(host, "a definer host")?
                )
            }
        })
    }
}

/// `SQL SECURITY` of views and routines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SqlSecurity {
    /// Runs with the definer's privileges.
    Definer,
    /// Runs with the caller's privileges.
    Invoker,
}

impl SqlSecurity {
    pub(super) fn clause(self) -> &'static str {
        match self {
            Self::Definer => "SQL SECURITY DEFINER",
            Self::Invoker => "SQL SECURITY INVOKER",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;

    fn target(server: &varve_driver::ServerInfo) -> CatalogResult<Target> {
        Target::new(server)
    }

    fn ty(name: TypeName) -> DataType {
        DataType::new(name)
    }

    #[test]
    fn types_render_with_lengths_and_flags() -> CatalogResult<()> {
        let t = target(&ma106())?;
        let mut plan = Plan::default();
        let mut d = ty(TypeName::Decimal);
        d.length = Some(10);
        d.scale = Some(2);
        d.unsigned = true;
        assert_eq!(d.render(&t, &mut plan)?, "DECIMAL(10,2) UNSIGNED");
        let mut v = ty(TypeName::Varchar);
        v.length = Some(40);
        v.charset = Some("utf8mb4".into());
        v.collation = Some("utf8mb4_bin".into());
        assert_eq!(
            v.render(&t, &mut plan)?,
            "VARCHAR(40) CHARACTER SET `utf8mb4` COLLATE `utf8mb4_bin`"
        );
        let mut e = ty(TypeName::Enum);
        e.values = vec!["a".into(), "it's".into()];
        assert_eq!(e.render(&t, &mut plan)?, "ENUM('a','it''s')");
        let mut i = ty(TypeName::Int);
        i.zerofill = true;
        assert_eq!(i.render(&t, &mut plan)?, "INT UNSIGNED ZEROFILL");
        let mut ts = ty(TypeName::Timestamp);
        ts.length = Some(6);
        assert_eq!(ts.render(&t, &mut plan)?, "TIMESTAMP(6)");
        let mut f = ty(TypeName::Float);
        f.length = Some(7);
        f.scale = Some(2);
        assert_eq!(f.render(&t, &mut plan)?, "FLOAT(7,2)");
        assert!(plan.warnings.is_empty());
        assert_eq!(ty(TypeName::Inet6).render(&t, &mut plan)?, "INET6");
        Ok(())
    }

    #[test]
    fn deprecated_type_forms_warn_on_mysql() -> CatalogResult<()> {
        let t = target(&my97())?;
        let mut plan = Plan::default();
        let mut i = ty(TypeName::Int);
        i.length = Some(5);
        i.zerofill = true;
        assert_eq!(i.render(&t, &mut plan)?, "INT(5) UNSIGNED ZEROFILL");
        let mut d = ty(TypeName::Double);
        d.unsigned = true;
        assert_eq!(d.render(&t, &mut plan)?, "DOUBLE UNSIGNED");
        assert_eq!(plan.warnings.len(), 3);
        let mut b = ty(TypeName::Tinyint);
        b.length = Some(1);
        let mut quiet = Plan::default();
        assert_eq!(b.render(&t, &mut quiet)?, "TINYINT(1)");
        assert!(quiet.warnings.is_empty());
        Ok(())
    }

    #[test]
    fn invalid_types_are_refused() -> CatalogResult<()> {
        let t = target(&my80())?;
        let mut plan = Plan::default();
        assert!(ty(TypeName::Varchar).render(&t, &mut plan).is_err());
        let mut s = ty(TypeName::Text);
        s.unsigned = true;
        assert!(s.render(&t, &mut plan).is_err());
        let mut c = ty(TypeName::Int);
        c.charset = Some("utf8mb4".into());
        assert!(c.render(&t, &mut plan).is_err());
        let mut d = ty(TypeName::Decimal);
        d.length = Some(5);
        d.scale = Some(6);
        assert!(d.render(&t, &mut plan).is_err());
        let mut e = ty(TypeName::Enum);
        e.values = vec!["a\\b".into()];
        assert!(e.render(&t, &mut plan).is_err());
        assert!(is_unsupported(&ty(TypeName::Uuid).render(&t, &mut plan)));
        assert!(is_unsupported(
            &ty(TypeName::Inet4).render(&target(&ma106())?, &mut plan)
        ));
        let mut v = ty(TypeName::Vector);
        v.length = Some(3);
        assert!(is_unsupported(&v.render(&t, &mut plan)));
        assert_eq!(v.render(&target(&my97())?, &mut plan)?, "VECTOR(3)");
        assert_eq!(v.render(&target(&ma123())?, &mut plan)?, "VECTOR(3)");
        Ok(())
    }

    #[test]
    fn columns_render_per_engine() -> CatalogResult<()> {
        let mut c = ColumnDef::new("created", {
            let mut t = ty(TypeName::Datetime);
            t.length = Some(3);
            t
        });
        c.nullable = false;
        c.default = Some(ColumnDefault::CurrentTimestamp { precision: Some(3) });
        c.on_update_current_timestamp = Some(3);
        c.invisible = true;
        c.comment = Some("row's time".into());
        let expected = "`created` DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3) ON UPDATE CURRENT_TIMESTAMP(3) INVISIBLE COMMENT 'row''s time'";
        for server in [my80(), my97(), ma106(), ma123()] {
            let mut plan = Plan::default();
            assert_eq!(c.render(&target(&server)?, &mut plan)?, expected);
        }
        let mut plan = Plan::default();
        assert!(is_unsupported(
            &c.render(&target(&mysql(8, 0, 22))?, &mut plan)
        ));

        let mut g = ColumnDef::new("total", ty(TypeName::Int));
        g.generated = Some(Generated {
            expression: "a + b;".into(),
            storage: GeneratedStorage::Stored,
        });
        g.nullable = false;
        assert_eq!(
            g.render(&target(&my80())?, &mut plan)?,
            "`total` INT GENERATED ALWAYS AS (a + b) STORED NOT NULL"
        );
        assert!(is_unsupported(&g.render(&target(&ma123())?, &mut plan)));
        g.nullable = true;
        assert_eq!(
            g.render(&target(&ma123())?, &mut plan)?,
            "`total` INT GENERATED ALWAYS AS (a + b) STORED"
        );

        let mut p = ColumnDef::new("pos", ty(TypeName::Point));
        p.nullable = false;
        p.srid = Some(4326);
        assert_eq!(
            p.render(&target(&my97())?, &mut plan)?,
            "`pos` POINT SRID 4326 NOT NULL"
        );
        assert_eq!(
            p.render(&target(&ma106())?, &mut plan)?,
            "`pos` POINT REF_SYSTEM_ID=4326 NOT NULL"
        );

        let mut id = ColumnDef::new("id", {
            let mut t = ty(TypeName::Bigint);
            t.unsigned = true;
            t
        });
        id.nullable = false;
        id.auto_increment = true;
        assert_eq!(
            id.render(&target(&my80())?, &mut plan)?,
            "`id` BIGINT UNSIGNED NOT NULL AUTO_INCREMENT"
        );
        Ok(())
    }

    #[test]
    fn defaults_follow_engine_rules() -> CatalogResult<()> {
        let mut text = ColumnDef::new("note", ty(TypeName::Text));
        text.default = Some(ColumnDefault::Literal {
            value: Value::Text("n/a".into()),
        });
        let mut plan = Plan::default();
        assert_eq!(
            text.render(&target(&my80())?, &mut plan)?,
            "`note` TEXT NULL DEFAULT ('n/a')"
        );
        assert_eq!(
            text.render(&target(&ma106())?, &mut plan)?,
            "`note` TEXT NULL DEFAULT 'n/a'"
        );
        assert!(is_unsupported(
            &text.render(&target(&mysql(8, 0, 12))?, &mut plan)
        ));

        let mut n = ColumnDef::new("n", ty(TypeName::Int));
        n.default = Some(ColumnDefault::Expression {
            sql: "floor(rand() * 10)".into(),
        });
        assert_eq!(
            n.render(&target(&my97())?, &mut plan)?,
            "`n` INT NULL DEFAULT (floor(rand() * 10))"
        );
        n.default = Some(ColumnDefault::Literal {
            value: Value::Int(-3),
        });
        assert_eq!(
            n.render(&target(&ma123())?, &mut plan)?,
            "`n` INT NULL DEFAULT -3"
        );
        n.nullable = false;
        n.default = Some(ColumnDefault::Null);
        assert!(is_invalid(&n.render(&target(&ma123())?, &mut plan)));

        let mut d = ColumnDef::new("d", ty(TypeName::Datetime));
        d.default = Some(ColumnDefault::CurrentTimestamp { precision: Some(2) });
        assert!(is_invalid(&d.render(&target(&my80())?, &mut plan)));
        let mut s = ColumnDef::new("s", ty(TypeName::Varchar));
        s.data_type.length = Some(5);
        s.default = Some(ColumnDefault::Literal {
            value: Value::Text("a\\b".into()),
        });
        assert_eq!(
            s.render(&target(&my80())?, &mut plan)?,
            "`s` VARCHAR(5) NULL DEFAULT _utf8mb4 X'615c62'"
        );
        Ok(())
    }

    #[test]
    fn row_period_columns_are_mariadb_only() -> CatalogResult<()> {
        let mut ts = ty(TypeName::Timestamp);
        ts.length = Some(6);
        let mut c = ColumnDef::new("rs", ts);
        c.row_period = Some(RowPeriod::Start);
        c.invisible = true;
        let mut plan = Plan::default();
        assert_eq!(
            c.render(&target(&ma106())?, &mut plan)?,
            "`rs` TIMESTAMP(6) GENERATED ALWAYS AS ROW START INVISIBLE"
        );
        assert!(is_unsupported(&c.render(&target(&my97())?, &mut plan)));
        c.data_type.length = None;
        assert!(is_invalid(&c.render(&target(&ma106())?, &mut plan)));
        Ok(())
    }

    #[test]
    fn indexes_render_per_engine() -> CatalogResult<()> {
        let index = IndexDef {
            name: Some("ix".into()),
            kind: IndexKind::Index,
            method: Some(IndexMethod::Btree),
            parts: vec![
                KeyPart::Column {
                    name: "a".into(),
                    length: Some(10),
                    descending: true,
                },
                KeyPart::column("b"),
            ],
            parser: None,
            comment: Some("lookup".into()),
            invisible: true,
        };
        let mut plan = Plan::default();
        assert_eq!(
            index.inline(&target(&my80())?, &mut plan)?,
            "INDEX `ix` USING BTREE (`a`(10) DESC, `b`) COMMENT 'lookup' INVISIBLE"
        );
        assert!(plan.warnings.is_empty());
        assert_eq!(
            index.create_statement(&target(&ma106())?, "`d`.`t`", &mut plan)?,
            "CREATE INDEX `ix` USING BTREE ON `d`.`t` (`a`(10) DESC, `b`) COMMENT 'lookup' IGNORED"
        );
        assert_eq!(plan.warnings.len(), 1);

        let functional = IndexDef {
            name: None,
            kind: IndexKind::Unique,
            method: None,
            parts: vec![KeyPart::Expression {
                sql: "lower(email)".into(),
                descending: false,
            }],
            parser: None,
            comment: None,
            invisible: false,
        };
        assert_eq!(
            functional.inline(&target(&my97())?, &mut plan)?,
            "UNIQUE INDEX ((lower(email)))"
        );
        assert!(is_unsupported(
            &functional.inline(&target(&ma123())?, &mut plan)
        ));
        assert!(is_unsupported(
            &functional.inline(&target(&mysql(8, 0, 12))?, &mut plan)
        ));

        let fulltext = IndexDef {
            name: Some("ft".into()),
            kind: IndexKind::Fulltext,
            method: None,
            parts: vec![KeyPart::column("body")],
            parser: Some("ngram".into()),
            comment: None,
            invisible: false,
        };
        assert_eq!(
            fulltext.inline(&target(&my80())?, &mut plan)?,
            "FULLTEXT INDEX `ft` (`body`) WITH PARSER `ngram`"
        );
        let mut bad = fulltext.clone();
        bad.method = Some(IndexMethod::Btree);
        assert!(is_invalid(&bad.inline(&target(&my80())?, &mut plan)));
        let mut primary = fulltext;
        primary.name = Some("primary".into());
        assert!(is_invalid(&primary.inline(&target(&my80())?, &mut plan)));
        Ok(())
    }

    #[test]
    fn definers_quote_account_parts() -> CatalogResult<()> {
        assert_eq!(Definer::CurrentUser.clause()?, "DEFINER = CURRENT_USER");
        let account = Definer::Account {
            user: "o'neil".into(),
            host: "%".into(),
        };
        assert_eq!(account.clause()?, "DEFINER = 'o''neil'@'%'");
        let bad = Definer::Account {
            user: "a\\b".into(),
            host: "%".into(),
        };
        assert!(bad.clause().is_err());
        Ok(())
    }
}
