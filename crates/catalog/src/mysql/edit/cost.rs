//! How expensive an `ALTER TABLE` operation is on InnoDB: the cheapest
//! `ALGORITHM` the server accepts for it.
//!
//! The table is built from live runs of every operation with every
//! algorithm on MySQL 8.0 and 9.7 and MariaDB 10.6 and 12.3 (the
//! `algorithms` integration test re-checks it). It describes InnoDB tables;
//! other engines mostly copy.

use super::Target;
use super::types::{ColumnDef, DataType, GeneratedStorage, TypeName};

/// The cheapest algorithm an operation runs with, from cheap to costly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Level {
    /// Metadata only (`ALGORITHM=INSTANT`).
    Instant,
    /// MariaDB: in place without rebuilding the clustered index
    /// (`ALGORITHM=NOCOPY`). MySQL has no such level: there it counts as
    /// [`Level::Inplace`].
    Nocopy,
    /// In place, possibly rebuilding the table (`ALGORITHM=INPLACE`).
    Inplace,
    /// Copies the table (`ALGORITHM=COPY`).
    Copy,
}

impl Level {
    /// `INSTANT` on MySQL and MariaDB alike, `NOCOPY` on MariaDB and
    /// `INPLACE` on MySQL, and so on.
    pub(super) fn pick(t: &Target, mysql: Level, mariadb: Level) -> Level {
        if t.is_mariadb() { mariadb } else { mysql }
    }

    pub(super) fn keyword(self) -> &'static str {
        match self {
            Self::Instant => "INSTANT",
            Self::Nocopy => "NOCOPY",
            Self::Inplace => "INPLACE",
            Self::Copy => "COPY",
        }
    }
}

/// What changing a column definition means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ColumnChange {
    /// Cheapest algorithm.
    pub level: Level,
    /// The change can cut, convert or drop stored values.
    pub lossy: bool,
    /// Warnings for the user.
    pub warnings: Vec<String>,
}

/// Bytes per character of a character set, for the `VARCHAR` length
/// prefix (1 byte up to 255 bytes, 2 bytes above). Unknown sets count as
/// 4, like `utf8mb4`, the default.
fn max_bytes_per_char(charset: Option<&str>) -> u32 {
    match charset.map(str::to_ascii_lowercase).as_deref() {
        Some(
            "latin1" | "latin2" | "latin5" | "latin7" | "ascii" | "binary" | "cp1250" | "cp1251"
            | "cp1256" | "cp1257" | "cp850" | "cp852" | "cp866" | "dec8" | "greek" | "hebrew"
            | "hp8" | "keybcs2" | "koi8r" | "koi8u" | "macce" | "macroman" | "swe7" | "tis620"
            | "armscii8" | "geostd8",
        ) => 1,
        Some("ucs2" | "big5" | "gbk" | "sjis" | "cp932" | "euckr") => 2,
        Some("utf8" | "utf8mb3" | "ujis" | "eucjpms" | "gb2312") => 3,
        _ => 4,
    }
}

fn integer_rank(name: TypeName) -> Option<u8> {
    match name {
        TypeName::Tinyint => Some(1),
        TypeName::Smallint => Some(2),
        TypeName::Mediumint => Some(3),
        TypeName::Int => Some(4),
        TypeName::Bigint => Some(5),
        _ => None,
    }
}

fn text_rank(name: TypeName) -> Option<(bool, u8)> {
    match name {
        TypeName::Tinytext => Some((false, 1)),
        TypeName::Text => Some((false, 2)),
        TypeName::Mediumtext => Some((false, 3)),
        TypeName::Longtext => Some((false, 4)),
        TypeName::Tinyblob => Some((true, 1)),
        TypeName::Blob => Some((true, 2)),
        TypeName::Mediumblob => Some((true, 3)),
        TypeName::Longblob => Some((true, 4)),
        _ => None,
    }
}

/// True when every value of `old` fits `new` unchanged.
fn widens(old: &DataType, new: &DataType) -> bool {
    if let (Some(a), Some(b)) = (integer_rank(old.name), integer_rank(new.name)) {
        return match (old.unsigned, new.unsigned) {
            (false, false) | (true, true) => b >= a,
            // Unsigned values fit a strictly larger signed type.
            (true, false) => b > a,
            (false, true) => false,
        };
    }
    if old.unsigned != new.unsigned {
        return false;
    }
    if let (Some((ab, a)), Some((bb, b))) = (text_rank(old.name), text_rank(new.name)) {
        return ab == bb && b >= a;
    }
    match (old.name, new.name) {
        (TypeName::Float, TypeName::Double) => true,
        (TypeName::Decimal, TypeName::Decimal) => {
            let precision = |t: &DataType| t.length.unwrap_or(10);
            let scale = |t: &DataType| t.scale.unwrap_or(0);
            scale(new) >= scale(old)
                && precision(new).saturating_sub(scale(new))
                    >= precision(old).saturating_sub(scale(old))
        }
        (TypeName::Char, TypeName::Char | TypeName::Varchar)
        | (TypeName::Varchar, TypeName::Varchar)
        | (TypeName::Binary, TypeName::Binary | TypeName::Varbinary)
        | (TypeName::Varbinary, TypeName::Varbinary) => {
            new.length.unwrap_or(1) >= old.length.unwrap_or(1)
        }
        (TypeName::Bit, TypeName::Bit) => new.length.unwrap_or(1) >= old.length.unwrap_or(1),
        (TypeName::Enum, TypeName::Enum) | (TypeName::Set, TypeName::Set) => {
            old.values.iter().all(|v| new.values.contains(v))
        }
        (a, b) => a == b,
    }
}

/// The type without character set and collation, for comparison.
fn bare(ty: &DataType) -> DataType {
    let mut out = ty.clone();
    out.charset = None;
    out.collation = None;
    out
}

/// Classifies a column definition change on InnoDB.
pub(super) fn column_change(
    t: &Target,
    old: &ColumnDef,
    new: &ColumnDef,
    moved: bool,
    indexed: bool,
) -> ColumnChange {
    let mut level = Level::Instant;
    let mut lossy = false;
    let mut warnings = Vec::new();
    let mut raise = |to: Level| level = level.max(to);
    let name = &new.name;

    if moved {
        raise(Level::pick(t, Level::Inplace, Level::Instant));
    }
    if old.generated != new.generated {
        let stored = |c: &ColumnDef| {
            c.generated
                .as_ref()
                .is_some_and(|g| g.storage == GeneratedStorage::Stored)
        };
        match (&old.generated, &new.generated) {
            (Some(_), Some(_)) if !stored(old) && !stored(new) => {
                raise(Level::pick(t, Level::Copy, Level::Instant));
            }
            (None, Some(_)) => {
                lossy = true;
                warnings.push(format!(
                    "making {name} a generated column replaces its stored values"
                ));
                raise(Level::Copy);
            }
            _ => raise(Level::Copy),
        }
    }
    if old.row_period != new.row_period || old.srid != new.srid {
        raise(Level::Copy);
    }
    if old.auto_increment != new.auto_increment {
        raise(Level::Copy);
    }
    if old.nullable != new.nullable {
        raise(Level::Inplace);
        if !new.nullable {
            warnings.push(format!(
                "making {name} NOT NULL fails when a row holds NULL in it"
            ));
        }
    }

    let (ot, nt) = (&old.data_type, &new.data_type);
    if bare(ot) != bare(nt) {
        let same = ot.name == nt.name;
        let fits = widens(ot, nt);
        let varlen = matches!(nt.name, TypeName::Varchar | TypeName::Varbinary);
        if same && varlen && fits && ot.unsigned == nt.unsigned {
            if t.is_mariadb() {
                raise(Level::Instant);
            } else {
                let per_char = if nt.name == TypeName::Varbinary {
                    1
                } else {
                    max_bytes_per_char(nt.charset.as_deref().or(ot.charset.as_deref()))
                };
                let bytes = |ty: &DataType| ty.length.unwrap_or(0) * per_char;
                if (bytes(ot) > 255) == (bytes(nt) > 255) {
                    raise(Level::Inplace);
                } else {
                    raise(Level::Copy);
                }
            }
        } else if same
            && matches!(nt.name, TypeName::Enum | TypeName::Set)
            && nt.values.starts_with(&ot.values)
            && ot.length == nt.length
        {
            // Appending members is metadata only while the storage size
            // stays the same.
            let storage = |ty: &DataType| match ty.name {
                TypeName::Enum => usize::from(ty.values.len() > 255),
                _ => ty.values.len().div_ceil(8),
            };
            if storage(ot) != storage(nt) {
                raise(Level::Copy);
            }
        } else {
            raise(Level::Copy);
        }
        if !fits {
            lossy = true;
            warnings.push(format!(
                "changing the type of {name} converts its values; values that do not fit are cut or rejected"
            ));
        }
    }

    let charset_changed =
        matches!((&ot.charset, &nt.charset), (Some(a), Some(b)) if !a.eq_ignore_ascii_case(b));
    let collation_changed =
        matches!((&ot.collation, &nt.collation), (Some(a), Some(b)) if !a.eq_ignore_ascii_case(b));
    if charset_changed {
        raise(Level::Copy);
        let to_utf8mb4 = nt
            .charset
            .as_deref()
            .is_some_and(|c| c.eq_ignore_ascii_case("utf8mb4"));
        if !to_utf8mb4 {
            lossy = true;
            warnings.push(format!(
                "converting {name} to another character set loses characters the new set lacks"
            ));
        }
    } else if collation_changed {
        if indexed {
            raise(Level::pick(t, Level::Copy, Level::Nocopy));
        } else {
            raise(Level::pick(t, Level::Inplace, Level::Instant));
        }
        if indexed {
            warnings.push(format!(
                "a new collation on indexed column {name} can make unique keys collide"
            ));
        }
    }
    ColumnChange {
        level,
        lossy,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::model::CatalogResult;

    fn varchar(name: &str, length: u32) -> ColumnDef {
        let mut ty = DataType::new(TypeName::Varchar);
        ty.length = Some(length);
        ColumnDef::new(name, ty)
    }

    #[test]
    fn column_changes_match_live_evidence() -> CatalogResult<()> {
        let my = Target::new(&my80())?;
        let ma = Target::new(&ma106())?;
        let c10 = varchar("c", 10);
        let mut commented = c10.clone();
        commented.comment = Some("k".into());
        for t in [&my, &ma] {
            assert_eq!(
                column_change(t, &c10, &commented, false, false).level,
                Level::Instant
            );
        }
        let widen = column_change(&my, &c10, &varchar("c", 20), false, false);
        assert_eq!((widen.level, widen.lossy), (Level::Inplace, false));
        assert_eq!(
            column_change(&ma, &c10, &varchar("c", 20), false, false).level,
            Level::Instant
        );
        assert_eq!(
            column_change(&my, &c10, &varchar("c", 100), false, false).level,
            Level::Copy
        );
        assert_eq!(
            column_change(&ma, &c10, &varchar("c", 100), false, false).level,
            Level::Instant
        );
        let mut latin = varchar("l", 10);
        latin.data_type.charset = Some("latin1".into());
        let mut latin255 = varchar("l", 255);
        latin255.data_type.charset = Some("latin1".into());
        let mut latin256 = varchar("l", 256);
        latin256.data_type.charset = Some("latin1".into());
        assert_eq!(
            column_change(&my, &latin, &latin255, false, false).level,
            Level::Inplace
        );
        assert_eq!(
            column_change(&my, &latin, &latin256, false, false).level,
            Level::Copy
        );
        let narrow = column_change(&my, &c10, &varchar("c", 5), false, false);
        assert_eq!((narrow.level, narrow.lossy), (Level::Copy, true));

        let int = ColumnDef::new("d", DataType::new(TypeName::Int));
        let big = ColumnDef::new("d", DataType::new(TypeName::Bigint));
        let change = column_change(&my, &int, &big, false, false);
        assert_eq!((change.level, change.lossy), (Level::Copy, false));
        let back = column_change(&ma, &big, &int, false, false);
        assert_eq!((back.level, back.lossy), (Level::Copy, true));
        let mut not_null = int.clone();
        not_null.nullable = false;
        let nn = column_change(&ma, &int, &not_null, false, false);
        assert_eq!(
            (nn.level, nn.lossy, nn.warnings.len()),
            (Level::Inplace, false, 1)
        );
        assert_eq!(
            column_change(&my, &int, &int, true, false).level,
            Level::Inplace
        );
        assert_eq!(
            column_change(&ma, &int, &int, true, false).level,
            Level::Instant
        );

        let mut en = ColumnDef::new("en", DataType::new(TypeName::Enum));
        en.data_type.values = vec!["a".into(), "b".into()];
        let mut appended = en.clone();
        appended.data_type.values.push("c".into());
        assert_eq!(
            column_change(&my, &en, &appended, false, false).level,
            Level::Instant
        );
        let mut reordered = en.clone();
        reordered.data_type.values.reverse();
        let r = column_change(&ma, &en, &reordered, false, false);
        assert_eq!((r.level, r.lossy), (Level::Copy, false));
        let mut removed = en.clone();
        removed.data_type.values.pop();
        assert!(column_change(&ma, &en, &removed, false, false).lossy);

        let mut bin = c10.clone();
        bin.data_type.charset = Some("utf8mb4".into());
        bin.data_type.collation = Some("utf8mb4_general_ci".into());
        let mut bin2 = bin.clone();
        bin2.data_type.collation = Some("utf8mb4_bin".into());
        assert_eq!(
            column_change(&my, &bin, &bin2, false, false).level,
            Level::Inplace
        );
        assert_eq!(
            column_change(&my, &bin, &bin2, false, true).level,
            Level::Copy
        );
        assert_eq!(
            column_change(&ma, &bin, &bin2, false, false).level,
            Level::Instant
        );
        assert_eq!(
            column_change(&ma, &bin, &bin2, false, true).level,
            Level::Nocopy
        );
        let mut latin_c = bin.clone();
        latin_c.data_type.charset = Some("latin1".into());
        latin_c.data_type.collation = None;
        let cs = column_change(&my, &bin, &latin_c, false, false);
        assert_eq!((cs.level, cs.lossy), (Level::Copy, true));
        Ok(())
    }
}
