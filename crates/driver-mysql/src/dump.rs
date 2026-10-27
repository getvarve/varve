//! Dump boilerplate: the character set lines `mysqldump` and `mariadb-dump`
//! write around a dump, made acceptable for a Varve session.
//!
//! A dump saves the client character set in its header
//! (`SET @OLD_CHARACTER_SET_CLIENT=@@CHARACTER_SET_CLIENT`), switches to its
//! own (`SET NAMES utf8mb4`), and restores the saved values in its footer
//! and around each `CREATE TABLE`
//! (`SET character_set_client = @saved_cs_client`). Varve keeps every
//! session at utf8mb4 and refuses to set the character set from a
//! variable, so [`normalize_dump_boilerplate`] removes those restore lines
//! (they would only put utf8mb4 back) and rewrites the `utf8`/`utf8mb3`
//! assignments of older dumps to utf8mb4. Nothing else changes; a dump in
//! another character set (`SET NAMES latin1`) is still refused when it
//! runs.

use std::borrow::Cow;

/// The variables a dump saves and restores, with the user variables it
/// saves them in.
const RESTORED: [(&str, [&str; 2]); 3] = [
    (
        "character_set_client",
        ["old_character_set_client", "saved_cs_client"],
    ),
    (
        "character_set_results",
        ["old_character_set_results", "saved_cs_results"],
    ),
    (
        "collation_connection",
        ["old_collation_connection", "saved_col_connection"],
    ),
];

/// The variables older dumps set to `utf8`.
const CHARSET_TARGETS: [&str; 3] = ["names", "character_set_client", "character_set_results"];

/// Rewrites the character set boilerplate of a `mysqldump` or
/// `mariadb-dump` file so it runs in a Varve session.
///
/// Whole lines only, each one statement as the dump tools write it (with or
/// without a `/*!NNNNN ... */` wrapper):
///
/// - `SET <variable> = @<saved>` restoring the client character set,
///   results character set or connection collation from the variable the
///   dump saved it in becomes an empty line;
/// - `SET NAMES utf8` / `utf8mb3` and `SET character_set_client = utf8` (or
///   `utf8mb3`, also for `character_set_results`) become `utf8mb4`.
///
/// Line numbers stay the same. Returns the input unchanged (borrowed) when
/// there is nothing to rewrite.
#[must_use]
pub fn normalize_dump_boilerplate(sql: &str) -> Cow<'_, str> {
    let mut changed = false;
    let mut out = String::with_capacity(sql.len());
    for line in sql.split_inclusive('\n') {
        let (body, ending) = match line.strip_suffix("\r\n") {
            Some(body) => (body, "\r\n"),
            None => match line.strip_suffix('\n') {
                Some(body) => (body, "\n"),
                None => (line, ""),
            },
        };
        match rewrite(body) {
            Some(new) => {
                changed = true;
                out.push_str(&new);
                out.push_str(ending);
            }
            None => out.push_str(line),
        }
    }
    if changed {
        Cow::Owned(out)
    } else {
        Cow::Borrowed(sql)
    }
}

/// The statement inside a line: without `;`, the `/*!NNNNN ... */`
/// wrapper, and surrounding blanks. Returns it and the wrapper parts.
fn unwrap(line: &str) -> Option<(&str, &str, &str)> {
    let trimmed = line.trim();
    let statement = trimmed.strip_suffix(';').unwrap_or(trimmed).trim_end();
    if let Some(rest) = statement.strip_prefix("/*!") {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let inner = rest[digits..].strip_suffix("*/")?;
        let prefix_len = 3 + digits;
        Some((
            inner.trim(),
            &statement[..prefix_len],
            &statement[statement.len() - 2..],
        ))
    } else {
        Some((statement, "", ""))
    }
}

/// `SET <name> = <value>` (`SET NAMES <value>`): the name and the value,
/// lower-cased. `None` for anything else.
fn assignment(statement: &str) -> Option<(String, String)> {
    let rest = statement.strip_prefix(|c: char| c.eq_ignore_ascii_case(&'s'))?;
    let rest = rest.strip_prefix(|c: char| c.eq_ignore_ascii_case(&'e'))?;
    let rest = rest.strip_prefix(|c: char| c.eq_ignore_ascii_case(&'t'))?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = rest.trim_start();
    let name_len = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    let name = rest[..name_len].to_ascii_lowercase();
    let rest = rest[name_len..].trim_start();
    let value = if name == "names" {
        rest
    } else {
        rest.strip_prefix('=')?.trim_start()
    };
    let value = value.trim();
    let plain = value
        .strip_prefix('@')
        .unwrap_or(value)
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_');
    if name.is_empty() || value.is_empty() || !plain {
        return None;
    }
    Some((name, value.to_ascii_lowercase()))
}

/// The rewritten line, or `None` to keep it.
fn rewrite(line: &str) -> Option<String> {
    let (statement, open, close) = unwrap(line)?;
    let (name, value) = assignment(statement)?;
    if let Some(saved) = value.strip_prefix('@') {
        let restores = RESTORED
            .iter()
            .any(|(variable, names)| *variable == name && names.contains(&saved));
        return restores.then(String::new);
    }
    if CHARSET_TARGETS.contains(&name.as_str()) && matches!(value.as_str(), "utf8" | "utf8mb3") {
        let fixed = if name == "names" {
            "SET NAMES utf8mb4".to_owned()
        } else {
            format!("SET {name} = utf8mb4")
        };
        let indent = &line[..line.len() - line.trim_start().len()];
        let wrapped = if open.is_empty() {
            fixed
        } else {
            format!("{open} {fixed} {close}")
        };
        return Some(format!("{indent}{wrapped};"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_lines_become_empty() {
        let dump = "/*!40101 SET @OLD_CHARACTER_SET_CLIENT=@@CHARACTER_SET_CLIENT */;\n\
                    /*!50503 SET NAMES utf8mb4 */;\n\
                    /*!40101 SET @saved_cs_client     = @@character_set_client */;\n\
                    /*!40101 SET character_set_client = @saved_cs_client */;\n\
                    /*!40101 SET CHARACTER_SET_CLIENT=@OLD_CHARACTER_SET_CLIENT */;\n\
                    /*!40101 SET CHARACTER_SET_RESULTS=@OLD_CHARACTER_SET_RESULTS */;\n\
                    /*!40101 SET COLLATION_CONNECTION=@OLD_COLLATION_CONNECTION */;\n\
                    SELECT 1;\n";
        assert_eq!(
            normalize_dump_boilerplate(dump),
            "/*!40101 SET @OLD_CHARACTER_SET_CLIENT=@@CHARACTER_SET_CLIENT */;\n\
             /*!50503 SET NAMES utf8mb4 */;\n\
             /*!40101 SET @saved_cs_client     = @@character_set_client */;\n\
             \n\n\n\n\
             SELECT 1;\n"
        );
    }

    #[test]
    fn old_utf8_becomes_utf8mb4() {
        assert_eq!(
            normalize_dump_boilerplate(
                "/*!40101 SET NAMES utf8 */;\r\nSET character_set_client = utf8;"
            ),
            "/*!40101 SET NAMES utf8mb4 */;\r\nSET character_set_client = utf8mb4;"
        );
    }

    #[test]
    fn everything_else_stays() {
        for sql in [
            "SELECT 1;\nSET NAMES latin1;\n",
            // A variable the dump does not use for saving.
            "SET character_set_client = @mine;",
            // Not a whole line.
            "SELECT 1; SET character_set_client = @saved_cs_client;",
            "SET collation_connection = @saved_cs_client;",
            "",
        ] {
            assert!(
                matches!(normalize_dump_boilerplate(sql), Cow::Borrowed(same) if same == sql),
                "{sql}"
            );
        }
    }

    #[test]
    fn both_fixture_dumps_pass_the_guard() {
        for dump in [
            include_str!("../tests/fixtures/mysqldump-8.0.sql"),
            include_str!("../tests/fixtures/mariadb-dump-12.3.sql"),
        ] {
            assert!(crate::guard::check_charset(dump, "the dump").is_err());
            let normalized = normalize_dump_boilerplate(dump);
            assert!(crate::guard::check_charset(&normalized, "the dump").is_ok());
            assert_eq!(normalized.lines().count(), dump.lines().count());
        }
    }
}
